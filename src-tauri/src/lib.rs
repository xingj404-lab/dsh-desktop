use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tauri::{
    menu::{MenuBuilder, MenuItem, PredefinedMenuItem, SubmenuBuilder},
    tray::TrayIconBuilder,
    webview::PageLoadEvent,
    AppHandle, Emitter, Manager, RunEvent, WindowEvent,
};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_updater::UpdaterExt;

mod backend;

/// A downloaded-but-not-yet-installed update, held in memory until the user
/// chooses to restart and apply it.
struct PendingUpdate {
    version: String,
    update: tauri_plugin_updater::Update,
    bytes: Vec<u8>,
}

/// Shared state between the backend watchdog thread and the UI commands.
pub(crate) struct AppState {
    /// PID of the running `dsh web` child, if any.
    pub pid: Mutex<Option<u32>>,
    /// Canonical loopback URL of the running backend, if ready.
    pub url: Mutex<Option<String>>,
    /// Current webview zoom scale.
    pub zoom: Mutex<f64>,
    /// Set once the app is exiting; stops the watchdog restart loop.
    pub shutting_down: AtomicBool,
    /// Keeps the tray icon alive for the process lifetime.
    pub tray: Mutex<Option<tauri::tray::TrayIcon>>,
    /// A downloaded-but-not-yet-installed update, if any.
    pub pending: Mutex<Option<PendingUpdate>>,
    /// Set while an update download/install is in progress.
    pub downloading: AtomicBool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            pid: Mutex::new(None),
            url: Mutex::new(None),
            zoom: Mutex::new(1.0),
            shutting_down: AtomicBool::new(false),
            tray: Mutex::new(None),
            pending: Mutex::new(None),
            downloading: AtomicBool::new(false),
        }
    }
}

#[tauri::command]
fn get_backend_url(state: tauri::State<'_, AppState>) -> Option<String> {
    state.url.lock().unwrap().clone()
}

#[tauri::command]
fn restart_backend(state: tauri::State<'_, AppState>) -> Result<(), String> {
    // The watchdog loop always respawns; killing the current child (if any)
    // short-circuits the wait and triggers an immediate restart.
    if let Some(pid) = state.pid.lock().unwrap().as_ref().copied() {
        backend::graceful_kill(pid);
    }
    Ok(())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Payload for the `update-available` event consumed by the injected badge.
#[derive(Clone, serde::Serialize)]
struct UpdatePayload {
    version: String,
}

/// Injected into the webview (idempotent) to show a style-isolated update
/// button beside the dsh sidebar settings control. It only overlays the page;
/// no dsh web source or existing DOM node is modified.
const BADGE_SCRIPT: &str = r#"
(function () {
  if (window.__dshUpdateBadgeInstalled) return;
  window.__dshUpdateBadgeInstalled = true;

  function showBadge(version) {
    if (document.getElementById("dsh-desktop-update-host")) return;

    var host = document.createElement("div");
    host.id = "dsh-desktop-update-host";
    host.setAttribute("style", "position:fixed;left:220px;bottom:18px;z-index:2147483647;pointer-events:none;");
    var root = host.attachShadow({ mode: "closed" });
    var b = document.createElement("button");
    b.title = "发现新版本 v" + version + "，点击更新";
    b.textContent = "更新";
    b.setAttribute("style", "pointer-events:auto;min-width:64px;height:36px;padding:0 17px;border:0;border-radius:18px;background:#2688f7;color:#fff;font:600 14px/36px -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;cursor:pointer;box-shadow:0 2px 8px rgba(0,0,0,.18);white-space:nowrap;");

    var cachedAnchor = null;
    var positionFrame = 0;
    function settingsAnchor() {
      if (cachedAnchor && cachedAnchor.isConnected) return cachedAnchor;
      var nodes = document.querySelectorAll('button,a,[role="button"]');
      for (var i = 0; i < nodes.length; i += 1) {
        var text = (nodes[i].textContent || "").replace(/\s+/g, "").trim();
        if (text === "设置" || text.toLowerCase() === "settings") {
          cachedAnchor = nodes[i];
          return cachedAnchor;
        }
      }
      return null;
    }

    function positionBadge() {
      var anchor = settingsAnchor();
      if (!anchor) {
        host.style.left = "220px";
        host.style.top = "auto";
        host.style.bottom = "18px";
        return;
      }

      var row = anchor;
      var parent = anchor.parentElement;
      while (parent) {
        var parentRect = parent.getBoundingClientRect();
        if (parentRect.height > 110 || parentRect.width > window.innerWidth * 0.6) break;
        row = parent;
        parent = parent.parentElement;
      }
      var rect = row.getBoundingClientRect();
      var width = b.getBoundingClientRect().width || 64;
      host.style.left = Math.max(12, rect.right - width - 16) + "px";
      host.style.top = Math.max(8, rect.top + (rect.height - 36) / 2) + "px";
      host.style.bottom = "auto";
    }

    function schedulePosition() {
      if (positionFrame) return;
      positionFrame = window.requestAnimationFrame(function () {
        positionFrame = 0;
        positionBadge();
      });
    }

    b.addEventListener("click", function () {
      var invoke = window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;
      if (!invoke || b.disabled) return;
      b.disabled = true;
      b.textContent = "更新中…";
      b.style.cursor = "default";
      b.style.opacity = "0.75";
      invoke("install_update").catch(function () {
        b.disabled = false;
        b.textContent = "更新";
        b.style.cursor = "pointer";
        b.style.opacity = "1";
      });
    });
    root.appendChild(b);
    document.body.appendChild(host);
    positionBadge();
    window.addEventListener("resize", schedulePosition);
    new MutationObserver(schedulePosition).observe(document.body, {
      childList: true,
      subtree: true
    });
  }

  var invoke = window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;
  var listen = window.__TAURI__ && window.__TAURI__.event && window.__TAURI__.event.listen;

  if (listen) {
    listen("update-available", function (e) {
      var v = (e.payload && e.payload.version) || "";
      if (v) showBadge(v);
    });
  }
  if (invoke) {
    invoke("get_update_version").then(function (v) { if (v) showBadge(v); }).catch(function () {});
  }
})();
"#;

#[tauri::command]
fn get_update_version(state: tauri::State<'_, AppState>) -> Option<String> {
    state
        .pending
        .lock()
        .unwrap()
        .as_ref()
        .map(|p| p.version.clone())
}

#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        if state.downloading.swap(true, Ordering::SeqCst) {
            return Err("Another update operation is already running.".to_string());
        }
        let _guard = DownloadGuard(app.clone());
        install_pending_update(&app)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn build_menu(app: &tauri::App) -> tauri::Result<()> {
    let h = app.handle();

    let app_menu = SubmenuBuilder::new(h, "DeepSeek Harness")
        .item(&PredefinedMenuItem::about(h, None, None)?)
        .separator()
        .item(&MenuItem::with_id(h, "check-updates", "Check for Updates…", true, None::<&str>)?)
        .separator()
        .item(&PredefinedMenuItem::quit(h, Some("Quit DeepSeek Harness"))?)
        .build()?;

    let edit_menu = SubmenuBuilder::new(h, "Edit")
        .item(&PredefinedMenuItem::undo(h, None)?)
        .item(&PredefinedMenuItem::redo(h, None)?)
        .separator()
        .item(&PredefinedMenuItem::cut(h, None)?)
        .item(&PredefinedMenuItem::copy(h, None)?)
        .item(&PredefinedMenuItem::paste(h, None)?)
        .item(&PredefinedMenuItem::select_all(h, None)?)
        .build()?;

    let view_menu = SubmenuBuilder::new(h, "View")
        .item(&PredefinedMenuItem::fullscreen(h, None)?)
        .separator()
        .item(&MenuItem::with_id(h, "zoom-in", "Zoom In", true, Some("CmdOrCtrl+Plus"))?)
        .item(&MenuItem::with_id(h, "zoom-out", "Zoom Out", true, Some("CmdOrCtrl+-"))?)
        .item(&MenuItem::with_id(h, "zoom-reset", "Actual Size", true, Some("CmdOrCtrl+0"))?)
        .separator()
        .item(&MenuItem::with_id(h, "reload", "Reload", true, Some("CmdOrCtrl+R"))?)
        .item(&MenuItem::with_id(h, "toggle-devtools", "Toggle Developer Tools", true, Some("CmdOrCtrl+Shift+I"))?)
        .build()?;

    let window_menu = SubmenuBuilder::new(h, "Window")
        .item(&PredefinedMenuItem::minimize(h, None)?)
        .item(&PredefinedMenuItem::maximize(h, None)?)
        .separator()
        .item(&PredefinedMenuItem::close_window(h, None)?)
        .build()?;

    let menu = MenuBuilder::new(h)
        .items(&[&app_menu, &edit_menu, &view_menu, &window_menu])
        .build()?;

    app.set_menu(menu)?;
    Ok(())
}

fn build_tray(app: &tauri::App) -> tauri::Result<tauri::tray::TrayIcon> {
    let h = app.handle();

    let show = MenuItem::with_id(h, "tray-show", "Show DeepSeek Harness", true, None::<&str>)?;
    let quit = MenuItem::with_id(h, "tray-quit", "Quit", true, None::<&str>)?;
    let menu = MenuBuilder::new(h).items(&[&show, &quit]).build()?;

    TrayIconBuilder::new()
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip("DeepSeek Harness")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray-show" => show_main_window(app),
            "tray-quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click { .. } = event {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)
}

fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn zoom(app: &AppHandle, factor: f64) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let state = app.state::<AppState>();
    let mut current = state.zoom.lock().unwrap();
    let next = if factor == 0.0 {
        1.0
    } else {
        (*current * factor).clamp(0.5, 2.0)
    };
    *current = next;
    let _ = window.set_zoom(next);
}

fn reload(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let target = app
        .state::<AppState>()
        .url
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| "index.html".to_string());
    if let Ok(url) = tauri::Url::parse(&target) {
        let _ = window.navigate(url);
    }
}

fn toggle_devtools(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if window.is_devtools_open() {
        let _ = window.close_devtools();
    } else {
        let _ = window.open_devtools();
    }
}

/// Resets the `downloading` flag when the update-flow thread finishes.
struct DownloadGuard(AppHandle);
impl Drop for DownloadGuard {
    fn drop(&mut self) {
        self.0
            .state::<AppState>()
            .downloading
            .store(false, Ordering::SeqCst);
    }
}

/// Check for the latest version and download it, returning the downloaded
/// bytes (signature already verified). `None` means we're up to date.
fn check_and_download(app: &AppHandle) -> Result<Option<PendingUpdate>, String> {
    tauri::async_runtime::block_on(async {
        let updater = app.updater().map_err(|e| e.to_string())?;
        match updater.check().await.map_err(|e| e.to_string())? {
            Some(update) => {
                let version = update.version.to_string();
                let bytes = update
                    .download(|_chunk_length, _content_length| {}, || {})
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(Some(PendingUpdate { version, update, bytes }))
            }
            None => Ok(None),
        }
    })
}

/// Prompt the user to install the already-downloaded update, then install it.
/// Runs in the caller's thread; `pending` is taken (and put back on "Later").
fn prompt_and_install(app: &AppHandle) {
    let state = app.state::<AppState>();
    let pending = state.pending.lock().unwrap().take();
    let Some(pending) = pending else {
        return;
    };

    let yes = app
        .dialog()
        .message(format!(
            "DeepSeek Harness {} has been downloaded.\n\nRestart now to install the update?",
            pending.version
        ))
        .title("Update Ready")
        .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
            "Restart & Update".into(),
            "Later".into(),
        ))
        .blocking_show();

    if !yes {
        // Keep the downloaded update so the badge can be clicked again.
        *state.pending.lock().unwrap() = Some(pending);
        return;
    }

    let PendingUpdate { update, bytes, .. } = pending;
    match update.install(bytes) {
        Ok(()) => app.restart(),
        Err(e) => {
            let _ = app
                .dialog()
                .message(format!("Install failed: {e}"))
                .title("DeepSeek Harness")
                .blocking_show();
        }
    }
}

/// Installs a downloaded update immediately. This is only called from the
/// explicit in-page update button, so no second confirmation is shown.
fn install_pending_update(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let pending = state.pending.lock().unwrap().take();
    let Some(pending) = pending else {
        return Err("No downloaded update is available.".to_string());
    };

    match pending.update.install(&pending.bytes) {
        Ok(()) => app.restart(),
        Err(e) => {
            // Keep the verified download so the user can retry from the badge
            // or the application menu without downloading it again.
            *state.pending.lock().unwrap() = Some(pending);
            let _ = app
                .dialog()
                .message(format!("Install failed: {e}"))
                .title("DeepSeek Harness")
                .blocking_show();
            Err(e.to_string())
        }
    }
}

/// Menu entry: manual check. If an update is already downloaded, prompts to
/// install; otherwise checks, downloads, then prompts.
fn check_for_updates(app: AppHandle) {
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        if state.downloading.swap(true, Ordering::SeqCst) {
            return;
        }
        let _guard = DownloadGuard(app.clone());

        if state.pending.lock().unwrap().is_some() {
            prompt_and_install(&app);
            return;
        }

        match check_and_download(&app) {
            Ok(Some(pending)) => {
                let version = pending.version.clone();
                *state.pending.lock().unwrap() = Some(pending);
                let _ = app.emit(
                    "update-available",
                    UpdatePayload {
                        version: version.clone(),
                    },
                );
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.eval(BADGE_SCRIPT);
                }
                prompt_and_install(&app);
            }
            Ok(None) => {
                let _ = app
                    .dialog()
                    .message("You are running the latest version.")
                    .title("DeepSeek Harness")
                    .blocking_show();
            }
            Err(e) => {
                let _ = app
                    .dialog()
                    .message(format!("Update check failed: {e}"))
                    .title("DeepSeek Harness")
                    .blocking_show();
            }
        }
    });
}

/// Background periodic check. When a new version is found, downloads it in the
/// background and only then (once downloaded) emits the event and injects the
/// badge. Holds at most one downloaded version: while one is pending, no new
/// download starts. After the user installs it (and the app restarts on the new
/// version), the next check downloads the latest version — which may skip
/// intermediate releases.
fn auto_update_check(app: AppHandle) {
    std::thread::spawn(move || {
        // Wait for the backend + UI to come up before the first check.
        std::thread::sleep(std::time::Duration::from_secs(20));
        loop {
            let state = app.state::<AppState>();
            let pending = state.pending.lock().unwrap().is_some();
            if !pending && !state.downloading.swap(true, Ordering::SeqCst) {
                let _guard = DownloadGuard(app.clone());
                let result = check_and_download(&app);

                if let Ok(Some(pending)) = result {
                    let version = pending.version.clone();
                    *state.pending.lock().unwrap() = Some(pending);
                    let _ = app.emit(
                        "update-available",
                        UpdatePayload {
                            version: version.clone(),
                        },
                    );
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.eval(BADGE_SCRIPT);
                    }
                }
            }

            // Check every 30 minutes (each check is a ~1 KB latest.json fetch).
            std::thread::sleep(std::time::Duration::from_secs(30 * 60));
        }
    });
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let _ = webview.eval(BADGE_SCRIPT);
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_backend_url,
            restart_backend,
            quit_app,
            get_update_version,
            install_update
        ])
        .setup(|app| {
            build_menu(app)?;
            let tray = build_tray(app)?;
            *app.state::<AppState>().tray.lock().unwrap() = Some(tray);

            let handle = app.handle().clone();
            std::thread::spawn(move || {
                backend::start_watchdog(&handle);
            });
            auto_update_check(app.handle().clone());
            Ok(())
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "zoom-in" => zoom(app, 1.1),
            "zoom-out" => zoom(app, 0.9),
            "zoom-reset" => zoom(app, 0.0),
            "reload" => reload(app),
            "toggle-devtools" => toggle_devtools(app),
            "check-updates" => check_for_updates(app.clone()),
            _ => {}
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Close button hides to tray; the app keeps running (and keeps
                // the backend alive). Quit is via the tray menu or Cmd+Q.
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building dsh-desktop")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                let state = app.state::<AppState>();
                state.shutting_down.store(true, Ordering::SeqCst);
                let pid = state.pid.lock().unwrap().as_ref().copied();
                if let Some(pid) = pid {
                    backend::graceful_kill(pid);
                }
            }
        });
}

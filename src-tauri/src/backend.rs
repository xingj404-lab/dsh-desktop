use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const RESTART_DELAY: Duration = Duration::from_secs(1);
const ERROR_RESTART_DELAY: Duration = Duration::from_secs(3);
const STDOUT_TAIL_CAP: usize = 8192;

/// Payload for the `backend-status` event consumed by the splash page.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendStatus {
    pub state: String,
    pub url: Option<String>,
    pub message: Option<String>,
}

fn emit(app: &AppHandle, state: &str, url: Option<String>, message: Option<String>) {
    let _ = app.emit(
        "backend-status",
        BackendStatus {
            state: state.to_string(),
            url,
            message,
        },
    );
}

/// How to launch the backend. Either a direct `dsh` executable, or the bundled
/// `node` binary running the bundled `dsh` bin script.
enum BackendCmd {
    Direct(std::path::PathBuf),
    Node {
        node: std::path::PathBuf,
        script: std::path::PathBuf,
    },
}

/// Resolve the backend launch command, in priority order:
/// 1. `DSH_BIN` env override,
/// 2. the bundled backend in the app resources (`backend/node` + `backend/node_modules/.../dsh`),
/// 3. `dsh` on `PATH`.
fn resolve_backend_cmd(app: &AppHandle) -> Result<BackendCmd, String> {
    if let Ok(p) = std::env::var("DSH_BIN") {
        if !p.is_empty() {
            return Ok(BackendCmd::Direct(std::path::PathBuf::from(p)));
        }
    }
    if let Ok(res) = app.path().resource_dir() {
        // The staged Node runtime is named `node` on macOS/Linux and `node.exe`
        // on Windows; pick whichever the bundle actually carries.
        let node = res.join("backend").join("node");
        let node_exe = res.join("backend").join("node.exe");
        let node = if node.exists() {
            node
        } else {
            node_exe
        };
        let script = res
            .join("backend")
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js");
        if node.exists() && script.exists() {
            return Ok(BackendCmd::Node { node, script });
        }
        let dsh = res.join("backend").join("dsh");
        if dsh.exists() {
            return Ok(BackendCmd::Direct(dsh));
        }
    }
    if let Ok(out) = Command::new("sh").arg("-c").arg("command -v dsh").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Ok(BackendCmd::Direct(std::path::PathBuf::from(s)));
            }
        }
    }
    Err(
        "DeepSeek Harness CLI (`dsh`) not found.\nInstall it (npm i -g @deepseek-ai/dsh), set DSH_BIN, or bundle it with scripts/bundle-backend.sh."
            .to_string(),
    )
}

fn pick_free_port() -> Result<u16, String> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("failed to bind loopback: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .port();
    drop(listener);
    Ok(port)
}

/// Create and validate the app-owned directory used as the backend cwd.
///
/// In particular, this rejects Windows drive-relative paths such as `D:`. Node
/// resolves those paths relative to per-drive process state and can reduce its
/// entry point to the drive itself, failing with `EISDIR ... lstat 'D:'`.
#[cfg(any(windows, test))]
fn prepare_backend_workspace(app_data_dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
    if !app_data_dir.is_absolute() {
        return Err(format!(
            "backend app data directory must be absolute: {}",
            app_data_dir.display()
        ));
    }

    let workspace = app_data_dir.join("backend-workspace");
    std::fs::create_dir_all(&workspace).map_err(|e| {
        format!(
            "failed to create backend workspace {}: {e}",
            workspace.display()
        )
    })?;

    // Do not canonicalize this path on Windows. Rust canonicalization changes
    // it to verbatim `\\?\C:\...` syntax, which is not accepted reliably as a
    // child process working directory and prevents the bundled Node from
    // starting. The app-data base is already absolute and create_dir_all above
    // proves the resulting directory is usable.
    Ok(workspace)
}

/// Validate an existing system-resolved directory before using it as a cwd.
/// This keeps the historical home-directory workspace on macOS/Linux without
/// trusting a raw HOME environment variable.
#[cfg(not(windows))]
fn prepare_existing_workspace(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!(
            "backend working directory must be absolute: {}",
            path.display()
        ));
    }
    if !path.is_dir() {
        return Err(format!(
            "backend working directory is not a directory: {}",
            path.display()
        ));
    }
    path.canonicalize().map_err(|e| {
        format!(
            "failed to resolve backend working directory {}: {e}",
            path.display()
        )
    })
}

/// A stable working directory for the backend. dsh groups sessions by
/// `process.cwd()`, so pinning this keeps the session namespace identical no
/// matter how the app was launched.
fn default_workspace_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    #[cfg(windows)]
    {
        // Use the Windows Known Folder-backed Tauri path instead of HOME or
        // USERPROFILE. Environment variables can be absent, redirected, or
        // malformed as a bare drive letter on real user machines.
        let app_data_dir = app
            .path()
            .app_local_data_dir()
            .map_err(|e| format!("failed to resolve app local data directory: {e}"))?;
        prepare_backend_workspace(&app_data_dir)
    }

    #[cfg(not(windows))]
    {
        let home_dir = app
            .path()
            .home_dir()
            .map_err(|e| format!("failed to resolve system home directory: {e}"))?;
        prepare_existing_workspace(&home_dir)
    }
}

/// Drain a child's output pipe so it cannot fill up and block the child.
/// Captures a capped tail of the bytes for error reporting.
fn drain<R: Read + Send + 'static>(reader: R, sink: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut s = sink.lock().unwrap();
                    s.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if s.len() > STDOUT_TAIL_CAP {
                        let cut = s.len() - STDOUT_TAIL_CAP;
                        *s = s[cut..].to_string();
                    }
                }
            }
        }
    });
}

/// Spawn `dsh web` bound to 127.0.0.1 on a freshly-picked free port.
/// Returns the child, the canonical URL, the port, and a captured stderr tail.
fn spawn_backend(app: &AppHandle) -> Result<(Child, String, u16, Arc<Mutex<String>>), String> {
    let cmd = resolve_backend_cmd(app)?;
    let port = pick_free_port()?;
    let url = format!("http://127.0.0.1:{port}");
    let port_s = port.to_string();

    let mut command = match cmd {
        BackendCmd::Direct(dsh) => {
            let mut c = Command::new(dsh);
            c.arg("web").arg("--host").arg("127.0.0.1").arg("--port").arg(&port_s);
            c
        }
        BackendCmd::Node { node, script } => {
            let mut c = Command::new(node);
            c.arg(&script);
            c.arg("web").arg("--host").arg("127.0.0.1").arg("--port").arg(&port_s);
            c
        }
    };

    // Resolve and validate the cwd before spawning. Never let a drive-relative
    // path such as `D:` reach Node on Windows.
    command.current_dir(default_workspace_dir(app)?);

    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start backend: {e}"))?;

    let out_sink = Arc::new(Mutex::new(String::new()));
    let err_sink = Arc::new(Mutex::new(String::new()));
    if let Some(stdout) = child.stdout.take() {
        drain(stdout, out_sink);
    }
    if let Some(stderr) = child.stderr.take() {
        drain(stderr, err_sink.clone());
    }

    Ok((child, url, port, err_sink))
}

fn wait_until_ready(child: &mut Child, port: u16, timeout: Duration) -> ReadyOutcome {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(Some(status)) = child.try_wait() {
            return ReadyOutcome::Exited(status);
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return ReadyOutcome::Ready;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    ReadyOutcome::TimedOut
}

enum ReadyOutcome {
    Ready,
    Exited(std::process::ExitStatus),
    TimedOut,
}

#[cfg(test)]
mod tests {
    use super::prepare_backend_workspace;
    #[cfg(not(windows))]
    use super::prepare_existing_workspace;
    use std::path::Path;

    #[test]
    fn rejects_a_windows_drive_relative_app_data_path() {
        let error = prepare_backend_workspace(Path::new("D:"))
            .expect_err("a bare drive letter must never reach Command::current_dir");

        assert!(error.contains("absolute"), "unexpected error: {error}");
    }

    #[test]
    fn creates_a_dedicated_backend_workspace() {
        let root = std::env::temp_dir().join(format!(
            "dsh-desktop-backend-workspace-test-{}",
            std::process::id()
        ));
        let workspace = prepare_backend_workspace(&root).expect("workspace should be created");

        assert_eq!(
            workspace,
            root.join("backend-workspace"),
            "the child-process cwd must retain normal path syntax"
        );
        assert!(workspace.is_dir());

        std::fs::remove_dir_all(root).expect("test workspace should be removable");
    }

    #[cfg(not(windows))]
    #[test]
    fn rejects_a_relative_unix_home_path() {
        let error = prepare_existing_workspace(Path::new("relative/home"))
            .expect_err("a relative HOME must never reach Command::current_dir");

        assert!(error.contains("absolute"), "unexpected error: {error}");
    }
}

/// Terminate a backend process by pid.
///
/// Unix: send SIGTERM, wait up to ~3s for exit, then SIGKILL.
/// Windows: `taskkill /T /F` (kills the process tree, forcing).
pub fn graceful_kill(pid: u32) {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        for _ in 0..30 {
            let alive = unsafe { libc::kill(pid as i32, 0) };
            if alive != 0 {
                return; // process gone
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }

    #[cfg(windows)]
    {
        // /T terminates the whole child tree, /F forces termination.
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The long-running watchdog. Spawns the backend, publishes readiness/crashes
/// over the `backend-status` event, and restarts it. The splash page listens
/// and navigates to the backend URL once `ready` is emitted.
pub fn start_watchdog(app: &AppHandle) {
    loop {
        let state = app.state::<AppState>();
        if state.shutting_down.load(Ordering::SeqCst) {
            break;
        }

        emit(app, "starting", None, None);

        let mut errored = false;

        match spawn_backend(app) {
            Ok((mut child, url, port, err_sink)) => {
                *state.pid.lock().unwrap() = Some(child.id());

                match wait_until_ready(&mut child, port, STARTUP_TIMEOUT) {
                    ReadyOutcome::Ready => {
                        // Only publish the URL once the backend is actually
                        // listening. The splash page's `get_backend_url` race
                        // guard navigates as soon as a URL is present, so
                        // publishing it here (instead of right after spawn)
                        // prevents the webview from hitting a port that is not
                        // yet bound — which surfaced as "127.0.0.1 refused
                        // connection" on slow Windows first launches.
                        *state.url.lock().unwrap() = Some(url.clone());
                        emit(app, "ready", Some(url.clone()), None);
                        // Block until the child exits (a crash, or a SIGTERM
                        // from `restart_backend` / shutdown).
                        let _status = child.wait();
                        *state.pid.lock().unwrap() = None;
                        *state.url.lock().unwrap() = None;
                        emit(
                            app,
                            "stopped",
                            None,
                            Some("backend process exited; restarting…".to_string()),
                        );
                    }
                    ReadyOutcome::Exited(status) => {
                        let tail = err_sink.lock().unwrap().clone();
                        let _ = child.wait(); // reap
                        *state.pid.lock().unwrap() = None;
                        *state.url.lock().unwrap() = None;
                        errored = true;
                        emit(
                            app,
                            "error",
                            None,
                            Some(format!("backend exited before ready ({status}).\n{tail}")),
                        );
                    }
                    ReadyOutcome::TimedOut => {
                        let tail = err_sink.lock().unwrap().clone();
                        graceful_kill(child.id());
                        let _ = child.wait();
                        *state.pid.lock().unwrap() = None;
                        *state.url.lock().unwrap() = None;
                        errored = true;
                        emit(
                            app,
                            "error",
                            None,
                            Some(format!(
                                "backend did not become ready within {:?}.\n{tail}",
                                STARTUP_TIMEOUT
                            )),
                        );
                    }
                }
            }
            Err(e) => {
                errored = true;
                emit(app, "error", None, Some(e));
            }
        }

        if state.shutting_down.load(Ordering::SeqCst) {
            break;
        }
        // Longer backoff after a hard error so a missing `dsh` binary does not
        // spin hot.
        std::thread::sleep(if errored {
            ERROR_RESTART_DELAY
        } else {
            RESTART_DELAY
        });
    }
}

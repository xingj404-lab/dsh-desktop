# Changelog

All notable changes to **dsh-desktop** (the native desktop app for DeepSeek Harness).

The format follows [Keep a Changelog](https://keepachangelog.com/), and this
project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.7] - 2026-08-16

### Fixed

- **Windows**: backend startup no longer trusts `USERPROFILE` as its working
  directory. It now uses a validated, app-owned local-data directory, preventing
  bare drive paths such as `D:` from crashing Node with `EISDIR`.
- **Windows**: the app-owned backend directory is no longer canonicalized to
  verbatim `\\?\` syntax before spawning Node, which could prevent the bundled
  backend from starting.
- **macOS/Linux**: backend startup now resolves the home directory through the
  system path API and validates/canonicalizes it instead of directly trusting
  `$HOME` or falling back to the filesystem root.

## [0.1.6] - 2026-08-15

### Fixed

- **Windows**: backend crashed at startup with `EISDIR … lstat 'C:'`. The backend's
  pinned working directory used `$HOME` (unset on Windows) and fell back to `/`,
  which Windows resolves to the drive root. Now uses `%USERPROFILE%` on Windows.

## [0.1.5] - 2026-08-14

### Added

- **Background download before prompt**: a detected update is downloaded silently in
  the background first; the blue badge appears only after the download finishes, so
  clicking it installs instantly (no waiting for the ~100 MB download).
- **One-version-at-a-time**: while a downloaded update is pending, no newer version
  is fetched; updates apply sequentially.
- **Version skipping**: after updating and restarting, the next check downloads the
  *latest* version, so intermediate releases can be skipped (e.g. 0.1.3 → 0.1.6).
- **Aliyun OSS hosting**: update bundles + installers are mirrored to Aliyun OSS
  (China-reachable) with GitHub Releases as fallback.

### Changed

- Update check interval shortened from 4 hours to 30 minutes.
- Update badge restyled as a blue **“⬇ 更新”** pill in the bottom-left corner.

## [0.1.4] - 2026-08-14

### Changed

- Update flow split into **download → prompt → install**: clicking the entry downloads
  in the background, then a dialog asks the user to restart to apply the update.

## [0.1.3] - 2026-08-14

### Added

- Background periodic update check (on launch and every few hours).
- Bottom-left update badge.
- Pinned backend working directory so session history is stable across launches.

## [0.1.2] - 2026-08-14

### Fixed

- Backend bundled Node 20, but dsh requires Node 22+; the backend now bundles Node 22.

## [0.1.1] - 2026-08-14

### Added

- New whale app icon (derived from the DeepSeek whale).
- In-app updater (menu bar → “Check for Updates…”).

## [0.1.0] - 2026-08-14

### Added

- Initial release: Tauri v2 native shell around the `dsh web` UI — native window,
  menu bar, system tray, window-state memory, self-contained backend (bundled
  Node + dsh), macOS + Windows builds.

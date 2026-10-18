# 0001 — macOS support behind a cfg-gated platform seam

- **Status:** proposed (2026-10-18)
- **Asked for by:** Nate, 2026-10-18: "can we have someone work on immichsync and add mac support?" The target is his M4 Max MacBook Pro (macOS 27, Apple Silicon).

## Context

ImmichSync was built for Windows only. `src/platform/mod.rs` says so outright ("No cross-platform abstractions are provided"). Most of the engine is already portable: the watch engine (`notify`), the upload pipeline (`tokio` + `reqwest`), the state DB (`rusqlite` bundled), hashing, EXIF, the tray (`tray-icon`) and the egui windows (`eframe`) all build on macOS as they stand. The parts that are Windows-only fall into three groups:

1. **Things that stop it compiling**: the `windows` and `winrt-notification` crates are unconditional dependencies, and these files call into them without a `cfg`: `app.rs` (the Win32 `PeekMessageW` pump), `platform/*` (every module), `ui/notifications.rs`, and `ui/{settings,about,upload_log,trash_log}.rs` (`EventLoopBuilderExtWindows::with_any_thread`). `build.rs` embeds a `.ico` with `winresource`.
2. **Things that compile but mean nothing on a Mac**: the `%LOCALAPPDATA%\Programs` self-install, Apps & Features, `.lnk` shortcuts, `--uninstall`, the `.exe` rename-dance self-updater, and the legacy-path migrations.
3. **Things with a real macOS equivalent** that we want: the menu-bar icon, login item, credential storage, notifications, the Pictures folder, cloud-placeholder detection, and graceful shutdown.

## Decision

### One seam: `src/platform/`, chosen by `cfg(target_os)`

`platform/mod.rs` keeps one public surface. Behind it, each concern is either a per-OS file, or one file with `#[cfg]` arms where the code is short. Callers outside `platform/` never write `cfg(windows)`. Windows code moves, and its behaviour does not change. The Windows CI job (`Build & test` on `windows-latest`) stays required and is the proof.

The Windows-only crates move to `[target.'cfg(windows)'.dependencies]`, and `winresource` moves to `[target.'cfg(windows)'.build-dependencies]` (`build.rs` already only needs it for the `.ico`). macOS crates go under `[target.'cfg(target_os = "macos")'.dependencies]`, using the `objc2` family (`objc2`, `objc2-foundation`, `objc2-app-kit`), which `tray-icon`/`winit` already pull in, so the dependency tree barely grows.

Linux is not a goal. A `cfg(not(any(windows, macos)))` arm may exist only where it costs nothing, such as `dirs::picture_dir()`.

### The Windows → macOS map

| Concern | Windows today | macOS | Slice |
|---|---|---|---|
| Main loop | `PeekMessageW`/`DispatchMessageW` pump every 50 ms in `App::run` | `NSApplication` with `ActivationPolicy::Accessory` (no Dock icon), pumped the same way: `nextEventMatchingMask:untilDate:` + `sendEvent:` each tick. This keeps `App::run`'s poll-everything loop identical | tracer |
| Tray icon | `tray-icon` (Shell_NotifyIcon) | `tray-icon` (`NSStatusItem`, a menu-bar extra). The icon should be a template image (monochrome, so it follows light/dark menu bars); the tracer uses the existing brand RGBA | tracer (icon polish later) |
| Settings/log/about windows | `eframe` in a `--window <x>` subprocess, `with_any_thread(true)` | The same subprocess model. `eframe` runs on the subprocess's main thread, so `with_any_thread` isn't needed and the call is `cfg(windows)` | tracer |
| File watching | `notify` `RecommendedWatcher` (ReadDirectoryChangesW) + poll fallback | `notify` `RecommendedWatcher` = **FSEvents**, with the same poll fallback for SMB/NFS mounts under `/Volumes` | tracer (no code change expected) |
| Single instance | Named mutex `Global\ImmichSync` | Exclusive `flock` on `~/Library/Application Support/bees-roadhouse/immichsync/immichsync.lock`, released by the kernel when the process dies | tracer |
| Config + state paths | `dirs::config_dir()` = `%APPDATA%`, `dirs::data_local_dir()` = `%LOCALAPPDATA%` | Both resolve to `~/Library/Application Support/`, so config, `state.db` and `logs/` share `…/bees-roadhouse/immichsync/`. Logs would conventionally go in `~/Library/Logs`, but keeping them beside the DB means no special case | tracer |
| Pictures folder | `SHGetKnownFolderPath(FOLDERID_Pictures)` | `dirs::picture_dir()` = `~/Pictures` | tracer |
| API key at rest | DPAPI-wrapped AES-256-GCM, `enc:` prefix in `config.toml` | **Keychain** generic password (service `com.beesroadhouse.immichsync`, account = server URL); `config.toml` holds a `keychain:` marker. Needs a stable code signature, or every rebuild re-prompts for Keychain access | Keychain slice |
| | | *Tracer only:* `encrypt_api_key` returns an error on macOS, so `Config::save` takes its existing "saving in plain text" path, and the file is `chmod 600`. This is stated in the tracer PR and closed by the Keychain slice | tracer |
| Autostart | `HKCU\…\Run` registry value | `SMAppService.mainApp.register()` (macOS 13+). It needs a signed `.app` bundle; a hand-written `~/Library/LaunchAgents/com.beesroadhouse.immichsync.plist` is the fallback for an unsigned dev build | login-item slice |
| Notifications | `winrt-notification` toast keyed by AppUserModelID | `UNUserNotificationCenter` (UserNotifications). It only works from a signed bundle with a bundle id, and asks permission once. The tracer logs instead of notifying | notifications slice |
| Cloud placeholders | `FILE_ATTRIBUTE_OFFLINE`/`RECALL_ON_*` skip in `watch/filter.rs` | iCloud Drive / File Provider "dataless" files: `st_flags & SF_DATALESS` (`0x40000000`). Opening one triggers a download, which is exactly what the fast path exists to avoid | placeholder slice |
| Hidden trash dir attr | `SetFileAttributesW(HIDDEN)` | The dot-prefixed name is enough; optionally `chflags hidden` | tracer (no-op) |
| Graceful shutdown | Hidden window catching `WM_CLOSE`/`WM_ENDSESSION` | Menu Quit already works. Add SIGTERM/SIGINT (logout, `kill`, launchd stop) to the same quit path; `applicationShouldTerminate` comes with the bundle | tracer (signals), bundle slice (AppKit) |
| Install / uninstall / shortcuts | Self-copy to `%LOCALAPPDATA%\Programs`, Apps & Features key, `.lnk`, `--uninstall` | **None.** A Mac app is a bundle dragged to `/Applications` (or `~/Applications`), and deleting it uninstalls. `is_running_installed()` returns `Ok(true)` and the install dialog never runs | tracer (stubs) |
| Self-update | Download `immichsync.exe`, rename-dance, relaunch | Download a notarized `.app.zip` (or `.dmg`), replace the bundle, relaunch. Until then, update checks are off on macOS so a Mac never downloads the `.exe` | bundle slice; tracer disables |
| Icon / resources | `build.rs` → multi-size `.ico` → `winresource` | `build.rs` → `.icns` (or an `Assets.car`) inside the bundle's `Resources/`, plus `Info.plist` with `LSUIElement = true` | bundle slice |
| Packaging | Bare `.exe` from `release.yml` | `ImmichSync.app`, signed with a Developer ID, notarized (`notarytool`) and stapled, released as a zip/dmg asset next to the `.exe`. Unsigned dev builds until the Developer Program account exists | bundle slice |
| Removable media | (PLAN lists `WM_DEVICECHANGE`; no `device.rs` exists in the tree today) | Watch `/Volumes` for mounts with a `DCIM/`, via DiskArbitration or plain FSEvents on `/Volumes` | removable-media slice |

### Apple Photos: folders first, PhotoKit later and separately

There are two different things a Mac user might mean by "sync my photos":

- **Folders**, such as `~/Pictures`, `~/Desktop`, an SD card import folder, or iCloud Drive folders. This is what ImmichSync already does on Windows, and the tracer gives it to macOS with almost no new code.
- **The Photos library** (`~/Pictures/Photos Library.photoslibrary`). This is a package, not a folder. Its `originals/` tree is an implementation detail Apple changes between releases; its edits, Live Photo pairs, bursts and "Optimize Mac Storage" originals (which live only in iCloud until requested) are not visible as files. Reading the package directly is fragile and needs Full Disk Access. The supported path is **PhotoKit** (`PHPhotoLibrary`, `PHAsset`, `PHAssetResourceManager`). It needs a signed bundle with `NSPhotoLibraryUsageDescription`, a TCC permission prompt, a change-token-based incremental scanner in place of a file watcher, iCloud download handling, and its own dedup key (`PHAsset.localIdentifier` plus the original's SHA-1). That is a second watch *source*, comparable in size to the whole watch engine.

**Recommendation:** ship folders first, through the tracer and the slices above. Treat PhotoKit as its own spec, decided after Nate has used the folder sync on his Mac. Immich's official iOS app already backs up an iPhone's library, and that covers most of what a Mac Photos library holds when iCloud Photos is on. The watch engine must not grow a special case for `.photoslibrary`. It goes into the default exclusions **in the tracer**: the default watch folder is `~/Pictures`, and without the exclusion the first scan would upload Photos' internal derivatives and thumbnails. **Open for Nate**, via Pia: is PhotoKit wanted at all, and if so, after which slice?

## Consequences

- Windows behaviour is unchanged, and the Windows CI job proves it on every PR. A macOS CI job (`macos-14` or newer, Apple Silicon) is added in the tracer PR. It runs `cargo test`, `clippy -D warnings` and `fmt`, and becomes a required check once green.
- Tests that pin Windows paths (`config_dir_under_roaming_appdata` and others) get `#[cfg(windows)]` or a macOS twin.
- The tracer is an unsigned dev build, run as a bare binary rather than an `.app`. Without a bundle it has no login item, no notifications and no Keychain. Each is its own issue below, and the bundle slice unblocks the last three.
- `CLAUDE.md` and `README.md` change from "Windows-only" to "Windows and macOS" in the tracer PR. ARCHITECTURE.md's *Platform Integration* section points here.

## Slices (each its own issue and PR)

1. This note.
2. **Tracer**: builds and runs on Apple Silicon, menu-bar icon visible, one watched folder syncs end to end; `*.photoslibrary` excluded by default; macOS CI job.
3. Login item: `SMAppService` with a LaunchAgent fallback, wired to the existing "Start with Windows"/"Start at login" setting.
4. Keychain API-key storage, migrating a plaintext key on first save.
5. Notifications through UserNotifications.
6. `.app` bundle, icon, `Info.plist` (`LSUIElement`), Developer ID signing and notarization in `release.yml`, and the macOS self-updater.
7. iCloud Drive dataless-placeholder skip (`SF_DATALESS`).
8. Removable media: DCIM on `/Volumes`.
9. (Decision first) Apple Photos library via PhotoKit.

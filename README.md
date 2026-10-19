# ImmichSync

A lightweight, native desktop app for **Windows and macOS** that watches folders, camera cards and (on the Mac) your Photos library, and uploads new photos and videos to your [Immich](https://immich.app) server. It lives in the system tray on Windows and the menu bar on macOS.

Install it, point it at your server, forget it exists.

## Why ImmichSync?

Immich has 84K+ GitHub stars and a thriving community, but no official desktop sync client. The existing options are CLI scripts, janky Python wrappers, or closed-source tools. ImmichSync fills the gap: a native app that sits in your tray or menu bar and Just Works.

- **Zero friction** — watches your Pictures folder by default
- **Handles everything** — local folders, network shares, USB drives, SD cards, and the macOS Photos library
- **Smart uploads** — deduplication, retry with backoff, bandwidth throttling
- **Tiny footprint** — pure Rust, a single small binary, no runtime dependencies
- **Set and forget** — starts at login, syncs in the background

## Platforms

| | Windows | macOS |
|---|---|---|
| Supported versions | Windows 10 and 11 | macOS 13 or newer, Apple Silicon |
| Lives in | System tray | Menu bar (no Dock icon) |
| Folder watching | `ReadDirectoryChangesW` | FSEvents |
| Cloud placeholders skipped | OneDrive, iCloud for Windows, Google Drive, Dropbox … (recall attributes) | iCloud Drive and File Provider "dataless" files |
| API key storage | DPAPI | Login Keychain |
| Notifications | Windows toasts | Notification Center |
| Start at login | Registry Run key | Login Items (SMAppService) |
| Removable media (DCIM cards) | `WM_DEVICECHANGE` | DiskArbitration |
| "Trash after upload" | `.immichsync-trash` folder, kept for the retention period | Finder Trash (recoverable) |
| Photos library | — | PhotoKit, opt-in and read-only |
| Self-update | Renames the `.exe` aside | Swaps the `.app` bundle from the release zip (Apple Silicon) |
| Signing | — | Ad-hoc for now; Developer ID and notarization are [#93](https://github.com/bees-roadhouse/immichsync/issues/93) |

## Features

- **Folder watching** — recursive, native filesystem events on each platform, with per-folder include and exclude patterns
- **Network share support** — automatic fallback to polling when native watching fails
- **Removable device detection** — auto-detects USB drives and SD cards with DCIM folders, on both platforms
- **Photos library (macOS)** — opt-in sync of the Apple Photos library through PhotoKit. It is read-only: ImmichSync never changes your library.
- **Upload queue** — persistent, crash-resistant, with configurable concurrency
- **SHA-1 deduplication** — matches Immich's internal checksum, saves bandwidth, and works across sources: a photo already uploaded from a folder isn't uploaded again from the Photos library
- **Album auto-creation** — organize by folder name, date, or a custom album
- **EXIF-aware** — extracts photo dates for accurate timestamps
- **Safe after-upload actions** — Keep, Trash or Delete per folder. A file that changed since its upload is never trashed or deleted, and app libraries and system folders (`.Trashes`, AppleDouble `._` files, Photos libraries) are never touched.
- **Encrypted API key storage** — Windows DPAPI, or the macOS login Keychain
- **Tray and menu-bar icon** — status, pause and resume, upload progress
- **Settings UI** — a clean configuration window built with egui
- **Start at login** — optional, on both platforms

## Installation

Releases are on [GitHub Releases](https://github.com/bees-roadhouse/immichsync/releases).

### Windows

Run the binary. On first launch it offers to install itself per-user (no admin needed) to `%LOCALAPPDATA%\Programs\immichsync\` and register in Apps & Features, so you can uninstall via Settings → Apps later. Pick **Run Portable** if you want to keep the binary wherever you downloaded it.

**Uninstall:** Settings → Apps → Installed apps → ImmichSync → Uninstall. Or from a terminal:

```powershell
winget uninstall ImmichSync
```

Uninstall removes the binary, shortcuts, autostart entry, and the Apps & Features registry block. **It preserves your settings and upload history** in `%APPDATA%\bees-roadhouse\immichsync\` and `%LOCALAPPDATA%\bees-roadhouse\immichsync\`, so a future reinstall picks up where you left off: no first-run wizard, no re-upload of already-synced files.

### macOS

Releases carry `ImmichSync-<version>-macos-arm64.zip` from the first release cut after macOS support landed; until then, build the bundle yourself (below). Unzip it and move `ImmichSync.app` to Applications.

- **First launch:** the app is ad-hoc signed until Developer ID signing lands, so the first time, right-click → **Open** to get past Gatekeeper.
- **Keychain:** the API key is kept in your login Keychain (`config.toml` holds only a `keychain:` marker). Because the build is ad-hoc signed, macOS asks once after each update whether ImmichSync may use its Keychain item; choose **Always Allow**. If you don't answer, the app keeps watching but can't upload until you do.
- **Updates:** **Check for updates** in the menu bar downloads the release zip, checks its signature, swaps it in for the running `ImmichSync.app` and relaunches. The app has to live in a folder you can write, such as Applications; run straight from Downloads or a disk image, it says so instead of updating.
- **Notifications** go to Notification Center; macOS asks once.
- **Start at login** in Settings registers ImmichSync under System Settings → General → Login Items.
- **Photos library:** off by default. Turning it on in Settings raises the macOS Photos permission prompt. Library uploads currently go into one album (default "Photos library"); mirroring your Photos albums is [#115](https://github.com/bees-roadhouse/immichsync/issues/115).
- **Uninstall:** quit ImmichSync from the menu bar, turn off Start at login, and drag the app to the Trash. Settings and history stay in `~/Library/Application Support/bees-roadhouse/immichsync/`; delete that folder too for a clean removal.

To build the bundle yourself: `packaging/macos/make-app.sh` (writes `target/macos/ImmichSync.app` and a zip).

### Building from Source

**Prerequisites:**
- [Rust](https://rustup.rs/) (stable, 1.75+)
- Windows 10 or 11, or macOS 13+ on Apple Silicon with the Xcode command-line tools

```bash
git clone https://github.com/bees-roadhouse/immichsync.git
cd immichsync
cargo build --release
```

The binary lands in `target/release/immichsync.exe` on Windows and `target/release/immichsync` on macOS. On macOS, run it from the app bundle (`packaging/macos/make-app.sh`): Keychain, notifications, login items and Photos access all need the bundle identity.

## Quick Start

1. Launch ImmichSync — it appears in your system tray (Windows) or menu bar (macOS)
2. The first-run wizard asks for your Immich server URL and API key
3. Click **Test Connection** to verify
4. Your Pictures folder is watched by default — add more folders if needed
5. Close settings — ImmichSync is now syncing

### Getting Your API Key

1. Open your Immich web interface
2. Go to **User Settings** → **API Keys**
3. Click **New API Key**, give it a name, copy the key

## Configuration

You can edit the config file directly or use the Settings UI.

| | Windows | macOS |
|---|---|---|
| Settings (`config.toml`) | `%APPDATA%\bees-roadhouse\immichsync\` (roaming) | `~/Library/Application Support/bees-roadhouse/immichsync/` |
| Upload queue (SQLite) and logs | `%LOCALAPPDATA%\bees-roadhouse\immichsync\` | the same folder |

On Windows, settings roam while machine-local state stays put, so it doesn't get synced across machines on enterprise roaming profiles.

Logs rotate daily as `logs/immichsync.log.YYYY-MM-DD`. The retention cap is 1 GiB total; older logs are pruned automatically on startup and hourly. The current day's file is never deleted.

```toml
[server]
url = "https://immich.example.com"
api_key = "encrypted:base64..."  # Windows (DPAPI); on macOS this is "keychain:..."

[upload]
concurrency = 2
bandwidth_limit_kbps = 0  # 0 = unlimited
order = "newest_first"
max_retries = 5

[devices]
auto_watch = true
look_for_dcim = true
on_insert = "auto"  # "auto" | "ask" | "ignore"

[photos]           # macOS only
enabled = false    # turning it on in Settings raises the Photos permission prompt
album_name = "Photos library"

[ui]
start_with_windows = true  # start at login, on both platforms
minimize_to_tray = true
show_notifications = true
```

## Supported File Types

**Images:** jpg, jpeg, png, gif, webp, heic, heif, avif, tiff, bmp, raw, cr2, cr3, nef, arw, dng, orf, rw2, pef, srw, raf

**Videos:** mp4, mov, avi, mkv, webm, m4v, 3gp, mts, m2ts

Custom include/exclude patterns are configurable per watch folder.

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full technical design, and [docs/adr/](docs/adr/) for the decisions behind it: [0001](docs/adr/0001-macos-platform-seam.md) the macOS platform seam, [0002](docs/adr/0002-photokit-library-source.md) the Photos library source.

**Key components:**
- **Watch Engine** — the `notify` crate (ReadDirectoryChangesW on Windows, FSEvents on macOS), with a poll fallback for network shares
- **Device Monitor** — `WM_DEVICECHANGE` on Windows, DiskArbitration on macOS, behind `platform::volumes`
- **Photos library** — PhotoKit (macOS), read-only
- **Upload Pipeline** — async Tokio workers with a persistent queue
- **State Database** — SQLite via `rusqlite` (bundled)
- **UI** — `tray-icon` + `egui` for the tray or menu-bar icon and the settings window
- **Platform** — `src/platform/`, one module per OS, selected by `cfg(target_os)`

## Contributing

Contributions are welcome. See [PLAN.md](PLAN.md) for the implementation roadmap and open tasks.

1. Fork the repo
2. Create a feature branch (`git checkout -b feature/my-feature`)
3. Commit your changes
4. Push and open a PR

Please keep PRs focused — one feature or fix per PR. CI builds and tests on both Windows and macOS.

## Support the Project

ImmichSync is free and open source. If it saves you time, consider supporting development:

- [GitHub Sponsors](https://github.com/sponsors/Gumbees)
- [Donate via Stripe](https://buy.stripe.com/8x214n0IjaoL0zwcsj4AU00)

## License

GPL-3.0-only — see [LICENSE](LICENSE) for details.

## Acknowledgments

- [Immich](https://immich.app) — the excellent self-hosted photo platform this syncs to
- The Rust ecosystem — `notify`, `tokio`, `reqwest`, `egui`, `rusqlite`, `objc2`, and all the crates that make this possible

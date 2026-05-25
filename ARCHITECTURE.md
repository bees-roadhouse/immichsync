# ImmichSync Architecture

## System Overview

```
+-------------------------------------------------------------+
|                       ImmichSync                             |
|                                                              |
|  +------------+  +----------------+  +-----------------+     |
|  | Watch      |  | Upload         |  | Immich API      |    |
|  | Engine     |->| Queue          |->| Client          |--->| Immich Server
|  |            |  | (async)        |  | (reqwest)       |    |
|  +------------+  +----------------+  +-----------------+     |
|       |                                                      |
|  +------------+  +----------------+  +-----------------+     |
|  | Device     |  | State DB       |  | System Tray     |    |
|  | Monitor    |  | (SQLite)       |  | + Settings UI   |    |
|  | (Win32)    |  |                |  | (egui)          |    |
|  +------------+  +----------------+  +-----------------+     |
+-------------------------------------------------------------+
```

## Component Map

| Component | Crate/Tech | Purpose |
|-----------|-----------|---------|
| Watch Engine | `notify` (ReadDirectoryChangesW) | Filesystem event watching |
| Network Fallback | `notify::PollWatcher` | Polling for NFS/paths where RDCW fails |
| Device Monitor | `windows` crate (WM_DEVICECHANGE) | USB/SD card insertion/removal |
| Upload Queue | `tokio` + custom async pipeline | Concurrent upload with retry |
| API Client | `reqwest` | Multipart upload to Immich REST API |
| State DB | `rusqlite` (bundled) | Track uploads, queue, watch folders |
| System Tray | `tray-icon` + `winit` | Background tray app |
| Settings UI | `egui` + `eframe` | Configuration window |
| Config | `serde` + `toml` | Persistent settings file |
| Logging | `tracing` + `tracing-subscriber` | Structured logging with rotation |

---

## Watch Engine

### Folder Sources (Priority Order)

1. **User's Pictures folder** — auto-detected via `SHGetKnownFolderPath(FOLDERID_Pictures)`, works even if redirected to OneDrive or NAS. Enabled by default on first run.

2. **User-added folders** — any local path, added via Settings UI or drag-and-drop onto tray icon.

3. **Network shares** — UNC paths (`\\server\share`) or mapped drives. Uses `ReadDirectoryChangesW` first, falls back to `PollWatcher` if no events received within a health-check window.

4. **Removable devices** — USB drives, SD cards. Detected via `WM_DEVICECHANGE` -> `DBT_DEVICEARRIVAL` / `DBT_DEVICEREMOVECOMPLETE`. On arrival: enumerate volumes, check for DCIM folder (cameras), or watch entire removable root.

### Watch Strategy

```
Local folders / SMB shares:
  notify::RecommendedWatcher (ReadDirectoryChangesW)
    -> recursive = true
    -> debounce via notify-debouncer-full

Network shares where RDCW fails:
  notify::PollWatcher
    -> interval: 30s (configurable)
    -> auto-detected via health check

Removable devices:
  Hidden message-only window -> WM_DEVICECHANGE
    -> On DBT_DEVICEARRIVAL: resolve drive letter, spawn watcher
    -> On DBT_DEVICEREMOVECOMPLETE: tear down watcher, flush queue
```

### Network Share Health Check

`ReadDirectoryChangesW` works with SMB/CIFS but fails silently on NFS and some exotic filesystems:

1. Start with native watcher on the path
2. Create a temp file, wait for the filesystem event
3. If no event within 5 seconds, switch to `PollWatcher` for that path
4. Log the fallback so users understand the behavior

### Watcher Health Monitoring & Auto-Restart

The mode-selection health check above runs once at startup. `ReadDirectoryChangesW` can also silently die mid-run — a network share drops, a USB device unplugs without a clean `WM_DEVICECHANGE`, or notify's worker thread hits an unrecoverable error. To detect those, the engine spawns a background **health monitor** task that probes every active watcher periodically.

**Probe mechanism** (same as the initial mode-selection check, applied repeatedly):

1. Write a probe file named `.immichsync_watchcheck` into the watched directory.
2. Wait for the watcher's normal event pipeline to emit the matching create/modify event. The handler short-circuits on this filename: it never reaches the upload pipeline.
3. Native watchers: probe timeout = ~8s (debounce window + headroom). Poll watchers: timeout scales with `poll_interval + debounce + 5s`, so a healthy poll watcher isn't flagged on every probe.
4. Probe interval: 60s per watcher.

**State machine** (per `FolderWatcher` slot):

```
Healthy ──probe miss──> Degraded (miss_count == 1)
Degraded ──probe miss──> Degraded (miss_count == 2) ──> restart()
restart ok ──> Degraded (next probe verifies)
restart ok + probe ok ──> Healthy
restart fail ──> Degraded, restart_fail_count++ ──> retry next tick
restart_fail_count == 3 ──> Failed (permanent; no further probes/restarts)
```

- **2 consecutive misses → restart.** Stop the dead watcher, drop it from the engine, re-canonicalise the path (in case a remount changed it), recreate from saved config, swap into the slot.
- **3 consecutive restart failures → give up.** Mark the slot `Failed`, emit a `WatchEvent::Error` so the operator sees it in logs, and stop probing that path until the user reloads config or restarts the app.

**Tray surfacing:**

| Engine health | Tray state |
|---|---|
| `Healthy` | `Idle` (green) or `Syncing` (blue) per queue stats |
| `Degraded` (any restart in progress) | `Error` (red) with "watcher degraded — auto-restart in progress" if idle; stays on `Syncing` if uploads are flowing |
| `Failed` (any permanent failure) | `Error` (red) with "one or more watchers are offline" |

This is complementary to the **DB-vs-engine reconcile pass** (every 2s): that one converges the watched-folder *set* against the DB; this one converges each watcher's *liveness* against reality.

### File Filtering

**Include by default:**
- Images: jpg, jpeg, png, gif, webp, heic, heif, avif, tiff, bmp, raw, cr2, cr3, nef, arw, dng, orf, rw2, pef, srw, raf
- Videos: mp4, mov, avi, mkv, webm, m4v, 3gp, mts, m2ts

**Exclude by default:**
- Thumbs.db, desktop.ini, .DS_Store, Zone.Identifier streams
- Files < 1KB (corrupted/empty)
- Files still being written (detected via write completion check)
- Cloud-storage placeholder files (OneDrive Files On-Demand, iCloud Drive for Windows, Google Drive for Desktop, Dropbox Smart Sync, Box Drive, SeaDrive, etc.) ... see the Cloud-Storage Interaction section below

**User-configurable:**
- Custom include/exclude glob patterns per watch folder
- Minimum file size threshold
- Per-folder `ignore_online_files` toggle (default ON)

### Write Completion Detection

Avoids uploading partial files:

1. Wait for debounce window (2s default)
2. Check file size stability (two reads 500ms apart)
3. Attempt non-exclusive read open
4. If all pass -> queue for upload
5. If not -> re-queue with exponential backoff (up to 30s)

### Cloud-Storage Interaction

Modern cloud-storage clients ... OneDrive Files On-Demand, iCloud Drive for Windows, Google Drive for Desktop, Dropbox Smart Sync, Box Drive, SeaDrive ... store **placeholder files** on disk: full filename and metadata, but no contents. The first read of a placeholder triggers a synchronous download from the cloud. For a folder with thousands of offloaded photos, naive backup-tool behavior would re-pull the entire library on first scan ... defeating the user's storage-tiering decision.

ImmichSync handles this in two layers:

**Filter layer (default ON).** The `FileFilter` reads each candidate file's Windows attribute DWORD via `MetadataExt::file_attributes()` and drops the event if any of these bits are set:

| Constant | Value | Set by |
|---|---|---|
| `FILE_ATTRIBUTE_OFFLINE` | `0x0000_1000` | Legacy hierarchical storage, some sync clients |
| `FILE_ATTRIBUTE_RECALL_ON_OPEN` | `0x0004_0000` | iCloud Drive, older OneDrive |
| `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` | `0x0040_0000` | Modern OneDrive Files On-Demand, Google Drive, Dropbox |

The check is free ... it's a bit-test against a DWORD already loaded by the `metadata()` call we make for the minimum-size filter. Per-folder toggle in Settings (`Skip cloud-only files`); the DB default on `watched_folders.ignore_online_files` is `1`.

**Worker layer (defense in depth).** Even with the filter on, a file's attributes can change between filter-time and hash-time (the cloud client offloads in the background). The two-layer dedup below also covers this case ... if we've ever uploaded this exact byte-identical file before, the fast-path hit means we never open the file at all.

### Two-Layer Deduplication

The dedup chain is now:

```
File ready -> stat (size + mtime + attrs) -> Filter (skip cloud placeholders if enabled)
   -> Fast-path lookup (path + size + mtime)  -> hit: skip
                                              -> miss: SHA-1 hash
                                                  -> hash lookup -> hit: skip
                                                                 -> miss: enqueue
   -> Upload -> Record (path, hash, size, mtime)
```

**Layer 1 ... fast-path dedup** (`uploaded_files.(file_path, file_size, file_mtime)` composite index). Lookup is one indexed SELECT against three already-known values; the file is never opened. This is the load-bearing path for cloud placeholders: on a folder of 10k offloaded photos, a second scan triggers zero downloads.

**Layer 2 ... content dedup** (`uploaded_files.file_hash`, SHA-1). The SHA-1 read happens only after a fast-path miss. Catches the renamed-or-touched cases where path or mtime changed but content is byte-identical.

Default behavior (Settings unchanged):
1. Cloud placeholders are filtered before either layer fires ... zero downloads.
2. Locally-materialized files hit the fast path on the second run ... zero re-reads.
3. Locally-materialized files with a new mtime fall through to SHA-1 ... covered by Immich's server-side dedup if content is identical.

Opt-in behavior (`Skip cloud-only files` toggled OFF on a folder):
1. First scan downloads + uploads all placeholders. Bandwidth cost paid once.
2. Each upload records `(path, size, mtime)` on the row.
3. Subsequent scans hit the fast path ... no re-downloads.

---

## Upload Pipeline

### Flow

```
File Event -> Debounce -> Filter -> Hash -> Dedup Check -> Queue -> Upload -> Confirm -> Record
```

### Hashing & Deduplication

- **Algorithm:** SHA-1 (matches Immich's internal checksum)
- **Layer 1 (fast path):** `(path, size, mtime)` against `uploaded_files` composite index — skip without opening the file. Critical for cloud-placeholder files; see the Cloud-Storage Interaction section above.
- **Layer 2 (content hash):** SHA-1 against `uploaded_files.file_hash` — runs only on fast-path miss.
- **Server dedup:** Immich performs server-side dedup, but local checks save bandwidth.

### Upload Request

Per the Immich API (`POST /api/assets`):

```
Content-Type: multipart/form-data

Fields:
  assetData: <file binary>
  deviceAssetId: "{path_hash}-{filename}"
  deviceId: "immichsync-{machine_id}"
  fileCreatedAt: <EXIF date or file creation time, ISO 8601>
  fileModifiedAt: <file modified time, ISO 8601>
  isFavorite: false
```

### Queue Behavior

- **Concurrency:** configurable, default 2 simultaneous uploads
- **Retry:** exponential backoff (1s, 2s, 4s, 8s, 16s, 32s, max 5 retries)
- **Bandwidth throttle:** optional, configurable max KB/s
- **Priority:** newest files first (configurable)
- **Pause/Resume:** via tray menu
- **Offline handling:** queue persists to SQLite, resumes when server is reachable
- **Large files:** increased timeout for files over configurable threshold

### Album Auto-Creation

Configurable per watch folder:
- Create album from parent folder name
- Create album from date (YYYY-MM)
- Add to a specific user-selected album
- No album (default)

---

## State Database (SQLite)

**Location:** `%APPDATA%\ImmichSync\state.db`

SQLite via `rusqlite` with the `bundled` feature (compiles SQLite from source, no system dependency). WAL mode enabled for crash recovery and concurrent read access.

### Schema

```sql
CREATE TABLE watched_folders (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    label TEXT,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    watch_mode TEXT NOT NULL DEFAULT 'native',     -- 'native' | 'poll'
    poll_interval_secs INTEGER NOT NULL DEFAULT 30,
    album_mode TEXT NOT NULL DEFAULT 'none',       -- 'none' | 'folder' | 'subfolder' | 'date' | 'fixed'
    album_name TEXT,
    include_patterns TEXT,                          -- JSON array of globs
    exclude_patterns TEXT,                          -- JSON array of globs
    post_upload TEXT NOT NULL DEFAULT 'keep',      -- 'keep' | 'trash' | 'delete'        (v2)
    ignore_online_files INTEGER NOT NULL DEFAULT 1,-- skip cloud-placeholder files      (v3)
    auto_added BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE uploaded_files (
    id INTEGER PRIMARY KEY,
    file_path TEXT NOT NULL,
    file_hash TEXT NOT NULL,                       -- SHA-1
    file_size INTEGER NOT NULL,
    file_mtime INTEGER NOT NULL DEFAULT 0,         -- seconds since Unix epoch          (v3)
    immich_asset_id TEXT,
    device_asset_id TEXT NOT NULL,
    uploaded_at TEXT NOT NULL,
    server_url TEXT NOT NULL
);

CREATE TABLE upload_queue (
    id INTEGER PRIMARY KEY,
    file_path TEXT NOT NULL,
    file_hash TEXT,
    file_size INTEGER,
    folder_id INTEGER REFERENCES watched_folders(id),
    status TEXT NOT NULL DEFAULT 'pending',         -- 'pending' | 'uploading' | 'failed' | 'completed'
    retry_count INTEGER NOT NULL DEFAULT 0,
    error_message TEXT,
    queued_at TEXT NOT NULL,
    completed_at TEXT
);

CREATE TABLE config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE INDEX idx_uploaded_hash ON uploaded_files(file_hash);
CREATE INDEX idx_uploaded_path ON uploaded_files(file_path);
CREATE INDEX idx_uploaded_path_size_mtime                                              -- v3 fast-path dedup
    ON uploaded_files(file_path, file_size, file_mtime);
CREATE INDEX idx_queue_status ON upload_queue(status);
```

### Design Notes

- **WAL mode** — enabled on database open for crash safety and concurrent reads
- **Transactions** — all queue state transitions wrapped in transactions
- **Migrations** — versioned via `config` table (`schema_version` key), applied on startup
- **JSON columns** — `include_patterns` and `exclude_patterns` store JSON arrays, parsed in Rust

---

## System Tray & UI

### Tray Icon States

| State | Icon | Description |
|-------|------|-------------|
| Idle | Green circle | Connected, watching, nothing to upload |
| Syncing | Blue animated arrow | Actively uploading |
| Queued | Blue dot | Items in queue, uploading |
| Paused | Yellow bars | User paused |
| Error | Red X | Server unreachable or auth failed |
| Offline | Gray circle | No server connection |

### Tray Context Menu

```
ImmichSync
---
Uploading 3/47 files...
---
Pause Sync
Upload Now           (force flush queue)
---
Watch Folders...     (opens settings)
View Upload Log
---
Server: photos.example.com [connected]
---
Settings
About
Quit
```

### Settings Window (egui)

Five tabs:

**Connection** — Server URL, API key (masked), test connection button, device name.

**Watch Folders** — List with enable/disable toggles, add folder button, per-folder settings (album mode, include/exclude patterns). Pictures folder shown as default, non-removable but disableable.

**Devices** — Auto-watch removable devices toggle, DCIM detection toggle, insert action (auto/ask/ignore).

**Upload** — Concurrent uploads slider (1-8), bandwidth limit, upload order (newest/oldest first), retry attempts (1-10), file type checkboxes.

**Advanced** — Network share poll interval, log level, start with Windows toggle, minimize to tray on close, database location, export/import config.

---

## Platform Integration

### Single Instance

Named mutex (`Global\ImmichSync`) ensures only one instance runs. If a second launch is attempted, it activates the existing instance's settings window.

### Auto-Start

Registry key at `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` with the path to the executable.

### API Key Encryption

API keys are encrypted with AES-256-GCM using a key derived from Windows DPAPI (`CryptProtectData`). Tied to the user's Windows login.

### Known Folder Resolution

`SHGetKnownFolderPath(FOLDERID_Pictures)` resolves the user's Pictures folder, handling OneDrive redirects and custom locations.

---

## Project Structure

```
immichsync/
├── Cargo.toml
├── build.rs                     # Windows resource compilation (icon, manifest)
├── assets/
│   ├── icon.ico                 # Tray/app icon
│   ├── icon_syncing.ico
│   ├── icon_paused.ico
│   ├── icon_error.ico
│   └── app.manifest             # DPI awareness, no admin required
├── src/
│   ├── main.rs                  # Entry point, single-instance check, tray bootstrap
│   ├── app.rs                   # Application state, lifecycle management
│   ├── config.rs                # Config loading/saving, TOML management
│   ├── db.rs                    # SQLite state database
│   ├── updater.rs               # GitHub Releases update checker and applier
│   ├── watch/
│   │   ├── mod.rs               # Watch engine orchestrator
│   │   ├── folder.rs            # Folder watcher (notify-based)
│   │   ├── network.rs           # Network share watcher with fallback
│   │   ├── device.rs            # Removable device monitor (WM_DEVICECHANGE)
│   │   └── filter.rs            # File filtering (extensions, globs, size)
│   ├── upload/
│   │   ├── mod.rs               # Upload pipeline orchestrator
│   │   ├── queue.rs             # Persistent upload queue
│   │   ├── hasher.rs            # SHA-1 hashing
│   │   ├── worker.rs            # Async upload workers
│   │   └── metadata.rs          # EXIF extraction, file timestamps
│   ├── api/
│   │   ├── mod.rs               # Immich API client
│   │   ├── auth.rs              # API key management, connection test
│   │   ├── assets.rs            # Asset upload, duplicate check
│   │   ├── albums.rs            # Album creation and management
│   │   └── server.rs            # Server info, health check
│   ├── ui/
│   │   ├── mod.rs               # UI orchestrator
│   │   ├── tray.rs              # System tray icon and menu
│   │   ├── settings.rs          # Settings window (egui)
│   │   ├── notifications.rs     # Windows toast notifications
│   │   ├── first_run.rs         # First-run setup wizard
│   │   ├── install.rs           # Install/update dialog
│   │   ├── update.rs            # Self-update download dialog
│   │   ├── upload_log.rs        # Upload log viewer
│   │   └── about.rs             # About dialog
│   └── platform/
│       ├── mod.rs               # Platform abstractions
│       ├── known_folders.rs     # SHGetKnownFolderPath wrapper
│       ├── autostart.rs         # Registry-based autostart
│       ├── single_instance.rs   # Named mutex for single instance
│       ├── drives.rs            # Drive enumeration, type detection
│       ├── encryption.rs        # DPAPI + AES-256-GCM API key encryption
│       ├── install.rs           # Self-install to AppData, version tracking
│       └── shortcuts.rs         # Desktop/start menu shortcut creation
├── tests/
│   ├── integration/
│   │   ├── watch_test.rs
│   │   └── upload_test.rs
│   └── mock_server/             # Mock Immich server for testing
```

---

## Dependencies

```toml
[dependencies]
# Async runtime
tokio = { version = "1", features = ["full"] }

# Filesystem watching
notify = "7"
notify-debouncer-full = "0.4"

# HTTP client
reqwest = { version = "0.12", features = ["multipart", "json", "stream"] }

# Database
rusqlite = { version = "0.32", features = ["bundled"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"

# Windows APIs
windows = { version = "0.58", features = [
    "Win32_UI_WindowsAndMessaging",
    "Win32_Storage_FileSystem",
    "Win32_System_IO",
    "Win32_UI_Shell",
    "Win32_Devices_DeviceAndDriverInstallation",
    "Win32_Foundation",
    "Win32_Security_Cryptography",
] }

# System tray
tray-icon = "0.19"
winit = "0.30"

# GUI
eframe = "0.29"
egui = "0.29"

# Hashing
sha1 = "0.10"

# EXIF parsing
kamadak-exif = "0.5"

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["fmt", "env-filter"] }
tracing-appender = "0.2"

# Utility
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["v4"] }
dirs = "5"
hostname = "0.4"
glob = "0.3"
thiserror = "2"
anyhow = "1"

# Encryption for API key storage
aes-gcm = "0.10"
```

---

## Key Technical Decisions

### SQLite for State

SQLite gives us ACID transactions, WAL mode for crash recovery, decades of battle-tested reliability, and fast indexed queries for dedup lookups and queue management. The `rusqlite` crate with `bundled` compiles SQLite from source — no system dependency needed. The data model (upload queue, watched folders, uploaded files) is naturally relational, and SQLite's query planner handles the indexed lookups efficiently.

### egui over Tauri

Tauri adds a WebView2 dependency and ~30MB to the binary. For a system tray app where the settings window is opened rarely, egui gives us a pure Rust UI with a ~5MB binary and instant startup.

### Direct reqwest over the `immich` crate

The `immich` crate is early-stage (0.2.0) with limited API surface. We only need ~5 endpoints (ping, upload, check duplicate, create album, add to album). A thin client with `reqwest` is less dependency risk and more control.

### SHA-1 for hashing

Immich uses SHA-1 internally for its checksum field. Matching their algorithm lets us compare directly. SHA-1 is cryptographically broken but we're using it for content addressing, not security.

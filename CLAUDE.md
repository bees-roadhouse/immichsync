# ImmichSync — Project Instructions

## What This Is

ImmichSync is a native Windows system tray app that watches folders and uploads photos/videos to an Immich server. Written in Rust, targeting Windows 10/11.

## Architecture

Read [ARCHITECTURE.md](ARCHITECTURE.md) for the full design. Read [PLAN.md](PLAN.md) for the implementation roadmap.

Key components:
- **Watch Engine** (`src/watch/`) — filesystem monitoring via `notify` crate
- **Upload Pipeline** (`src/upload/`) — async upload workers via `tokio`
- **API Client** (`src/api/`) — Immich REST API via `reqwest`
- **State DB** (`src/db.rs`) — SQLite via `rusqlite` (bundled)
- **UI** (`src/ui/`) — system tray via `tray-icon`, settings via `egui`
- **Platform** (`src/platform/`) — Windows-specific integrations (Win32 APIs)

## Tech Stack

| What | Crate | Notes |
|------|-------|-------|
| Runtime | `tokio` | Full features |
| HTTP | `reqwest` | Multipart, JSON, streaming |
| Database | `rusqlite` | Bundled SQLite, WAL mode |
| FS watch | `notify` + `notify-debouncer-full` | ReadDirectoryChangesW + poll fallback |
| GUI | `egui` / `eframe` | Settings window only |
| Tray | `tray-icon` + `winit` | System tray icon and menu |
| Win32 | `windows` crate | Device detection, known folders, DPAPI |
| Hashing | `sha1` | Matches Immich's internal checksum |
| EXIF | `kamadak-exif` | Photo date extraction |
| Logging | `tracing` | With file rotation via `tracing-appender` |
| Config | `toml` + `serde` | `%APPDATA%\ImmichSync\config.toml` |

## Conventions

### Rust Style
- Use `thiserror` for library-style errors in modules, `anyhow` at the application boundary
- Prefer `tracing` macros (`info!`, `warn!`, `error!`, `debug!`) over `println!`
- Use `tokio` for all async work — no blocking in async contexts
- Enums for state machines (watch mode, queue status, album mode) — not string constants
- Keep modules focused: one concern per file

### Database
- All schema lives in `src/db.rs`
- WAL mode enabled on every connection open
- All queue state transitions wrapped in transactions
- Schema migrations tracked via `config` table (`schema_version` key)
- Use parameterized queries — never interpolate values into SQL strings
- Timestamps stored as ISO 8601 TEXT

### Error Handling
- API errors: typed via `thiserror` with variants for auth, network, server, and rate limit
- Upload failures: retry with exponential backoff, persist error to queue entry
- Watch failures: log and continue — one bad path shouldn't stop other watchers
- DB errors: fatal on init, recoverable on individual operations

### File Paths
- All paths stored as UTF-8 strings in the database
- Use `std::path::Path` / `PathBuf` in Rust code
- Normalize paths before storage (consistent separators, no trailing slash)
- Handle long paths (Windows `\\?\` prefix) where needed

### Configuration
- `config.toml` is the user-facing config (TOML, human-editable)
- `state.db` is the internal state (SQLite, not user-editable)
- Never store secrets in plaintext config — encrypt API keys via DPAPI + AES-256-GCM

## Building

```bash
cargo build --release
```

Binary output: `target/release/immichsync.exe`

This is a Windows-only project. Cross-compilation is not a goal.

## Testing

```bash
cargo test
```

- Unit tests inline in modules
- Integration tests in `tests/` directory
- Mock Immich server in `tests/mock_server/` for upload testing
- Don't test against real Immich servers in CI

## Git Workflow

Follows the DevOps book standards: [Branching Strategy](https://kb.beesroadhouse.com/books/developer-operations-devops/page/branching-strategy), [Change Taxonomy](https://kb.beesroadhouse.com/books/developer-operations-devops/page/change-taxonomy), [PR Merge Strategy](https://kb.beesroadhouse.com/books/developer-operations-devops/page/pr-merge-strategy), [Issue Workflow](https://kb.beesroadhouse.com/books/developer-operations-devops/page/issue-workflow).

- `development` is the default branch (always shippable). All changes land via PR — the org `Default Branch Protection` ruleset (id 15744970) requires thread resolution and rejects direct pushes with `GH013`. Approving review is not currently required (solo-developer org; CI is the gate). Re-enable the review count when collaborators arrive.
- `release` is also PR-protected by the org `Release Branch Protection` ruleset (id 15553415), which targets `refs/heads/release`, `refs/heads/release/*`, and `refs/heads/release-*`. Same review policy (0 required) and thread-resolution requirement.
- Work branches: `feature/`, `improvement/`, `refactor/`, `bug/`.
- One discrete change per PR; rebase onto `development` before merge.
- **Squash-and-merge** for work-branch → `development`. **Merge commit** for `development` → `release` (squashing the release transition breaks ancestor relationship).
- `BREAKING:` prefix in PR title forces a major version bump regardless of type.
- Standard PR-create incantation (auto-merge once required checks pass):
  ```bash
  gh pr create --base development --head <branch> --title "..." --body "..."
  gh pr merge --auto --squash --delete-branch
  ```
  Use `--admin --squash --delete-branch` instead for the documented "small touchups" OrganizationAdmin bypass (docs typos, link fixes, version-bump-only PRs). See [Branching Strategy](https://kb.beesroadhouse.com/books/developer-operations-devops/page/branching-strategy) for the full policy.
- Required status checks on this repo (per repo-level ruleset id 16845497): `Build & test` (ci.yml) AND `Regenerate SBOM and STRUCTURE` (generate-artifacts.yml).

### Two-Tier Labels

Every PR and issue carries exactly **one type label + one category label**:

| Branch prefix | Type | Category | Default semver bump |
|---|---|---|---|
| `bug/` | `type:problem` | `category:bug` | Patch |
| `refactor/` | `type:problem` | `category:refactor` | Patch (or minor if external behavior changes) |
| `feature/` | `type:enhancement` | `category:feature` | Minor |
| `improvement/` | `type:enhancement` | `category:improvement` | Minor |

Issue templates in `.github/ISSUE_TEMPLATE/` apply the right pair automatically. State labels (`question`, `duplicate`, `wontfix`, `invalid`, `help wanted`) are kept; the GitHub default `bug` / `enhancement` / `documentation` / `good first issue` labels are removed in favor of the two-tier scheme.

Planned work belongs in [GitHub issues](https://github.com/bees-roadhouse/immichsync/issues), not in `PLAN.md` or other tree docs. PLAN.md describes what is, not what will be.

## Key Design Decisions

1. **SQLite over document DBs** — ACID transactions, WAL crash recovery, mature tooling, fast indexed queries. The data model is relational.
2. **egui over Tauri** — No WebView2 dependency, ~5MB binary vs ~35MB, pure Rust stack.
3. **Direct reqwest over `immich` crate** — We only need ~5 endpoints. Less dependency risk.
4. **SHA-1 for hashing** — Matches Immich's internal checksum. Not for security, just content addressing.
5. **`notify` with poll fallback** — Native `ReadDirectoryChangesW` for local/SMB, auto-fallback to polling for NFS/exotic filesystems.

## Immich API Endpoints Used

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/api/server/ping` | GET | Health check |
| `/api/server/about` | GET | Server info (requires `server.about`) |
| `/api/assets` | POST | Upload asset (multipart) |
| `/api/assets/bulk-upload-check` | POST | Check duplicates before upload |
| `/api/albums` | POST | Create album |
| `/api/albums/{id}/assets` | PUT | Add assets to album |

Auth: `x-api-key` header on all requests.

## Duplicate Prevention

Four layers:
1. **Fast-path (path+size+mtime)** — composite index lookup on `uploaded_files`; skips without ever opening the file. Critical for cloud-storage placeholders (OneDrive Files On-Demand, iCloud, SeaDrive, etc.) where opening a file triggers a re-download. See ARCHITECTURE.md → Cloud-Storage Interaction.
2. **Content hash (SHA-1)** — fall through to SHA-1, lookup by `uploaded_files.file_hash`. Catches renamed/touched-but-identical files.
3. **Server bulk check** — `POST /api/assets/bulk-upload-check` with checksums (retry-only).
4. **Server-side dedup** — Immich rejects duplicates on upload (last resort, wastes bandwidth).

## Data Locations

| What | Path |
|------|------|
| Config | `%APPDATA%\ImmichSync\config.toml` |
| Database | `%APPDATA%\ImmichSync\state.db` |
| Logs | `%APPDATA%\ImmichSync\logs\` |

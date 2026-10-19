//! SQLite state database for ImmichSync.
//!
//! Opens (or creates) `%LOCALAPPDATA%\bees-roadhouse\immichsync\state.db`,
//! enables WAL mode, and runs schema migrations on every startup. All queue
//! state transitions are wrapped in transactions.
//!
//! This file is intentionally in `%LOCALAPPDATA%` ... watched folder set,
//! upload progress, and dedup history are machine-local, not roaming.

use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info};

use crate::config::Config;

// ─── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum DbError {
    #[error("Failed to determine data directory: {0}")]
    DataDir(#[from] crate::config::ConfigError),

    #[error("Failed to open database at `{path}`: {source}")]
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("Failed to execute pragma `{pragma}`: {source}")]
    Pragma {
        pragma: &'static str,
        source: rusqlite::Error,
    },

    #[error("Schema migration failed at version {version}: {source}")]
    Migration {
        version: u32,
        source: rusqlite::Error,
    },

    #[error("Database query failed: {0}")]
    Query(#[from] rusqlite::Error),
}

// ─── Domain enums ────────────────────────────────────────────────────────────

/// How a watched folder receives filesystem change notifications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchMode {
    /// Use `ReadDirectoryChangesW` via the `notify` crate (default for local
    /// and SMB paths).
    Native,
    /// Fall back to periodic polling (used for NFS and exotic filesystems).
    Poll,
}

impl WatchMode {
    fn as_str(&self) -> &'static str {
        match self {
            WatchMode::Native => "native",
            WatchMode::Poll => "poll",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "poll" => WatchMode::Poll,
            _ => WatchMode::Native,
        }
    }
}

/// How files from a watched folder are assigned to Immich albums.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlbumMode {
    /// Do not add to any album.
    None,
    /// Create/use an album named after the watched folder root directory.
    Folder,
    /// Create/use an album named after the file's immediate parent subfolder
    /// within the watched directory.
    Subfolder,
    /// Create/use an album named `YYYY-MM` based on the file date.
    Date,
    /// Add to a specific user-chosen album.
    Fixed,
}

impl AlbumMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlbumMode::None => "none",
            AlbumMode::Folder => "folder",
            AlbumMode::Subfolder => "subfolder",
            AlbumMode::Date => "date",
            AlbumMode::Fixed => "fixed",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "folder" => AlbumMode::Folder,
            "subfolder" => AlbumMode::Subfolder,
            "date" => AlbumMode::Date,
            "fixed" => AlbumMode::Fixed,
            _ => AlbumMode::None,
        }
    }
}

/// What to do with a source file after it has been successfully uploaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostUpload {
    /// Leave the file in place (default).
    Keep,
    /// Move to `.immichsync-trash/` inside the watched folder root.
    Trash,
    /// Delete the file immediately after confirmed upload.
    Delete,
}

impl PostUpload {
    pub fn as_str(&self) -> &'static str {
        match self {
            PostUpload::Keep => "keep",
            PostUpload::Trash => "trash",
            PostUpload::Delete => "delete",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "trash" => PostUpload::Trash,
            "delete" => PostUpload::Delete,
            _ => PostUpload::Keep,
        }
    }
}

/// Status of an entry in the upload queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueStatus {
    Pending,
    Uploading,
    Failed,
    Completed,
}

impl QueueStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            QueueStatus::Pending => "pending",
            QueueStatus::Uploading => "uploading",
            QueueStatus::Failed => "failed",
            QueueStatus::Completed => "completed",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "uploading" => QueueStatus::Uploading,
            "failed" => QueueStatus::Failed,
            "completed" => QueueStatus::Completed,
            _ => QueueStatus::Pending,
        }
    }
}

/// Where a queue entry's bytes come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueSource {
    /// A file in a watched folder; `file_path` is the file.
    Folder,
    /// An asset in the Apple Photos library, exported to a temp file when a
    /// worker picks the entry up; `photos_local_id` names it and
    /// `file_path` is only a label (`photos:<localIdentifier>`).
    Photos,
}

impl QueueSource {
    pub fn from_str(s: &str) -> Self {
        match s {
            "photos" => QueueSource::Photos,
            _ => QueueSource::Folder,
        }
    }
}

// ─── Row structs ─────────────────────────────────────────────────────────────

/// A folder being monitored for new media files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchedFolder {
    pub id: i64,
    /// Absolute filesystem path to the folder.
    pub path: String,
    /// Human-readable label shown in the UI.
    pub label: Option<String>,
    /// Whether watching is active for this folder.
    pub enabled: bool,
    /// Native RDCW or poll-based watching.
    pub watch_mode: WatchMode,
    /// Polling interval (only meaningful when `watch_mode == Poll`).
    pub poll_interval_secs: i64,
    /// How new files are grouped into Immich albums.
    pub album_mode: AlbumMode,
    /// Album name used when `album_mode == Fixed`.
    pub album_name: Option<String>,
    /// JSON array of glob patterns to include (e.g. `["*.jpg", "*.png"]`).
    pub include_patterns: Option<String>,
    /// JSON array of glob patterns to exclude.
    pub exclude_patterns: Option<String>,
    /// What to do with files after successful upload.
    pub post_upload: PostUpload,
    /// When `true`, the watcher skips Windows cloud-storage placeholder files
    /// (OneDrive Files On-Demand, iCloud Drive, etc.) so reading them doesn't
    /// trigger a re-download. Default `true` (column default in v3).
    pub ignore_online_files: bool,
    /// `true` if the folder was added automatically (e.g. removable device).
    pub auto_added: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// A pending, active, or terminal entry in the upload queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: i64,
    pub file_path: String,
    pub file_hash: Option<String>,
    pub file_size: Option<i64>,
    /// FK to `watched_folders.id` — may be `None` for manually-queued files.
    pub folder_id: Option<i64>,
    pub status: QueueStatus,
    pub retry_count: i32,
    pub error_message: Option<String>,
    pub queued_at: String,
    pub completed_at: Option<String>,
    pub source: QueueSource,
    /// `PHAsset.localIdentifier` when `source == Photos`.
    pub photos_local_id: Option<String>,
}

/// Aggregated counts of queue entries by status.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueStats {
    pub pending: i64,
    pub uploading: i64,
    pub failed: i64,
    pub completed: i64,
}

// ─── Database ────────────────────────────────────────────────────────────────

/// The schema version a fully migrated database is at.
pub const SCHEMA_VERSION: u32 = 5;

/// An Immich album ImmichSync created to mirror a Photos album (#115).
/// Only albums recorded here are ever renamed or added to by the mirror;
/// albums a person made in Immich are never adopted or touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirroredAlbum {
    pub immich_album_id: String,
    /// The name last given to it, to notice a rename in Photos.
    pub name: String,
}

/// One schema migration step (see [`Database::run_migrations`]).
type Migration = fn(&Database) -> Result<(), DbError>;

/// Wraps a `rusqlite::Connection` and provides typed CRUD methods.
pub struct Database {
    conn: Connection,
}

impl Database {
    /// A fresh in-memory database with migrations run, for tests in other
    /// modules.
    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|source| DbError::Pragma {
                pragma: "foreign_keys",
                source,
            })?;
        let mut db = Self { conn };
        db.run_migrations()?;
        Ok(db)
    }

    /// Open (or create) the state database, enable WAL mode, and run any
    /// pending schema migrations.
    pub fn open() -> Result<Self, DbError> {
        let dir = Config::local_data_dir()?;
        let path = dir.join("state.db");

        let conn = Connection::open(&path).map_err(|source| DbError::Open {
            path: path.clone(),
            source,
        })?;

        // Enable WAL mode for crash safety and concurrent reads.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|source| DbError::Pragma {
                pragma: "journal_mode",
                source,
            })?;

        // Enable foreign key enforcement.
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|source| DbError::Pragma {
                pragma: "foreign_keys",
                source,
            })?;

        let mut db = Self { conn };
        db.run_migrations()?;

        info!(path = %path.display(), "Database opened");
        Ok(db)
    }

    // ── Migrations ───────────────────────────────────────────────────────────

    fn run_migrations(&mut self) -> Result<(), DbError> {
        // Bootstrap: create the config table if it doesn't exist yet so we
        // can read schema_version before any other migration runs.
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS config (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );",
            )
            .map_err(|e| DbError::Migration {
                version: 0,
                source: e,
            })?;

        let current_version = self.get_schema_version()?;
        debug!(current_version, "Running schema migrations");

        let steps: [(u32, Migration); SCHEMA_VERSION as usize] = [
            (1, Self::migrate_v1),
            (2, Self::migrate_v2),
            (3, Self::migrate_v3),
            (4, Self::migrate_v4),
            (5, Self::migrate_v5),
        ];
        for (version, migrate) in steps {
            if current_version < version {
                self.apply_migration(version, migrate)?;
            }
        }

        Ok(())
    }

    /// Run one migration and record its version in a single transaction.
    /// SQLite DDL is transactional, so a step that dies part-way (disk full,
    /// the app killed during start) leaves the previous version intact and
    /// is simply run again next start, instead of leaving a half-applied
    /// schema that fails forever on "duplicate column".
    fn apply_migration(&self, version: u32, migrate: Migration) -> Result<(), DbError> {
        let tx = self.conn.unchecked_transaction()?;
        migrate(self)?;
        self.set_schema_version(version)?;
        tx.commit()?;
        info!(version, "Applied schema migration");
        Ok(())
    }

    /// Initial schema: all four tables and their indexes.
    fn migrate_v1(&self) -> Result<(), DbError> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS watched_folders (
                    id                  INTEGER PRIMARY KEY,
                    path                TEXT NOT NULL UNIQUE,
                    label               TEXT,
                    enabled             BOOLEAN NOT NULL DEFAULT TRUE,
                    watch_mode          TEXT NOT NULL DEFAULT 'native',
                    poll_interval_secs  INTEGER NOT NULL DEFAULT 30,
                    album_mode          TEXT NOT NULL DEFAULT 'none',
                    album_name          TEXT,
                    include_patterns    TEXT,
                    exclude_patterns    TEXT,
                    auto_added          BOOLEAN NOT NULL DEFAULT FALSE,
                    created_at          TEXT NOT NULL,
                    updated_at          TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS uploaded_files (
                    id              INTEGER PRIMARY KEY,
                    file_path       TEXT NOT NULL,
                    file_hash       TEXT NOT NULL,
                    file_size       INTEGER NOT NULL,
                    immich_asset_id TEXT,
                    device_asset_id TEXT NOT NULL,
                    uploaded_at     TEXT NOT NULL,
                    server_url      TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS upload_queue (
                    id            INTEGER PRIMARY KEY,
                    file_path     TEXT NOT NULL,
                    file_hash     TEXT,
                    file_size     INTEGER,
                    folder_id     INTEGER REFERENCES watched_folders(id),
                    status        TEXT NOT NULL DEFAULT 'pending',
                    retry_count   INTEGER NOT NULL DEFAULT 0,
                    error_message TEXT,
                    queued_at     TEXT NOT NULL,
                    completed_at  TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_uploaded_hash
                    ON uploaded_files(file_hash);

                CREATE INDEX IF NOT EXISTS idx_uploaded_path
                    ON uploaded_files(file_path);

                CREATE INDEX IF NOT EXISTS idx_queue_status
                    ON upload_queue(status);
                ",
            )
            .map_err(|e| DbError::Migration {
                version: 1,
                source: e,
            })
    }

    /// v2: add `post_upload` column to `watched_folders`.
    fn migrate_v2(&self) -> Result<(), DbError> {
        self.conn
            .execute_batch(
                "ALTER TABLE watched_folders
                 ADD COLUMN post_upload TEXT NOT NULL DEFAULT 'keep';",
            )
            .map_err(|e| DbError::Migration {
                version: 2,
                source: e,
            })
    }

    /// v3: cloud-placeholder ignore toggle on `watched_folders` + path/size/mtime
    /// fast-path dedup index on `uploaded_files`.
    ///
    /// Two coupled changes for issue #26: the toggle lets watchers skip
    /// online files at event-emit time, and the (path, size, mtime) composite
    /// index lets the worker dedup a file without ever opening it for SHA-1
    /// (which would trigger a cloud download as a side effect).
    fn migrate_v3(&self) -> Result<(), DbError> {
        self.conn
            .execute_batch(
                "ALTER TABLE watched_folders
                    ADD COLUMN ignore_online_files INTEGER NOT NULL DEFAULT 1;

                 ALTER TABLE uploaded_files
                    ADD COLUMN file_mtime INTEGER NOT NULL DEFAULT 0;

                 CREATE INDEX IF NOT EXISTS idx_uploaded_path_size_mtime
                    ON uploaded_files(file_path, file_size, file_mtime);",
            )
            .map_err(|e| DbError::Migration {
                version: 3,
                source: e,
            })
    }

    /// The Photos library source (docs/adr/0002-photokit-library-source.md):
    /// queue entries can name a PhotoKit asset instead of a file, and
    /// `photos_assets` remembers what each asset's resources became on the
    /// server. A Live Photo is two rows under one `local_id`.
    fn migrate_v4(&self) -> Result<(), DbError> {
        self.conn
            .execute_batch(
                "ALTER TABLE upload_queue
                    ADD COLUMN source TEXT NOT NULL DEFAULT 'folder';

                 ALTER TABLE upload_queue
                    ADD COLUMN photos_local_id TEXT;

                 CREATE INDEX IF NOT EXISTS idx_queue_photos_local_id
                    ON upload_queue(photos_local_id);

                 CREATE TABLE IF NOT EXISTS photos_assets (
                    local_id         TEXT NOT NULL,
                    resource         TEXT NOT NULL,
                    sha1             TEXT,
                    immich_asset_id  TEXT,
                    modified_at      TEXT,
                    PRIMARY KEY (local_id, resource)
                 );",
            )
            .map_err(|e| DbError::Migration {
                version: 4,
                source: e,
            })
    }

    /// Album mirroring (#115): which Immich album ImmichSync made for each
    /// Photos album, keyed by the collection's `localIdentifier` so a rename
    /// in Photos renames the same Immich album.
    fn migrate_v5(&self) -> Result<(), DbError> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS mirrored_albums (
                    photos_collection_id TEXT PRIMARY KEY,
                    immich_album_id      TEXT NOT NULL,
                    name                 TEXT NOT NULL,
                    created_at           TEXT NOT NULL
                 );",
            )
            .map_err(|e| DbError::Migration {
                version: 5,
                source: e,
            })
    }

    fn get_schema_version(&self) -> Result<u32, DbError> {
        let version: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM config WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;

        Ok(version.and_then(|v| v.parse::<u32>().ok()).unwrap_or(0))
    }

    fn set_schema_version(&self, version: u32) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO config (key, value) VALUES ('schema_version', ?1)",
            params![version.to_string()],
        )?;
        Ok(())
    }

    // ── watched_folders ──────────────────────────────────────────────────────

    /// Add a new watched folder. Returns the new row's `id`.
    ///
    /// `ignore_online_files` defaults to `TRUE` (the v3 column default), which
    /// keeps the watcher from re-downloading cloud-placeholder files on every
    /// scan. Toggle it off per-folder in Settings if the user wants to upload
    /// placeholders too.
    pub fn add_folder(
        &self,
        path: &str,
        label: Option<&str>,
        auto_added: bool,
    ) -> Result<i64, DbError> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO watched_folders
                (path, label, enabled, watch_mode, poll_interval_secs,
                 album_mode, post_upload, ignore_online_files,
                 auto_added, created_at, updated_at)
             VALUES (?1, ?2, TRUE, 'native', 30, 'none', 'keep', 1, ?3, ?4, ?4)",
            params![path, label, auto_added, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Remove a watched folder by id.
    pub fn remove_folder(&self, id: i64) -> Result<(), DbError> {
        self.conn
            .execute("DELETE FROM watched_folders WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Return all watched folders ordered by id.
    pub fn get_folders(&self) -> Result<Vec<WatchedFolder>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, label, enabled, watch_mode, poll_interval_secs,
                    album_mode, album_name, include_patterns, exclude_patterns,
                    post_upload, ignore_online_files, auto_added, created_at, updated_at
             FROM watched_folders
             ORDER BY id",
        )?;

        let rows = stmt.query_map([], map_watched_folder)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DbError::Query)
    }

    /// Look up a single watched folder by its filesystem path.
    pub fn get_folder_by_path(&self, path: &str) -> Result<Option<WatchedFolder>, DbError> {
        let result = self
            .conn
            .query_row(
                "SELECT id, path, label, enabled, watch_mode, poll_interval_secs,
                        album_mode, album_name, include_patterns, exclude_patterns,
                        post_upload, ignore_online_files, auto_added, created_at, updated_at
                 FROM watched_folders
                 WHERE path = ?1",
                params![path],
                map_watched_folder,
            )
            .optional()?;
        Ok(result)
    }

    /// Rewrite each folder row to its canonical path (`watch::paths`), for
    /// rows stored before #99. Rows that don't resolve (folder missing, a
    /// drive not mounted) are left as they are, and so is a row whose
    /// canonical path another row already holds. Returns how many changed.
    pub fn canonicalize_folder_paths(&self) -> FolderPathMigration {
        let mut out = FolderPathMigration::default();
        let Ok(folders) = self.get_folders() else {
            return out;
        };
        for f in &folders {
            let Ok(canon) = crate::watch::paths::normalize_folder(&f.path) else {
                continue;
            };
            if canon == f.path {
                continue;
            }
            if let Some(other) = folders.iter().find(|o| o.id != f.id && o.path == canon) {
                // `path` is UNIQUE: two rows name the same folder. Leave both
                // and say so, so the duplicate can be removed in Settings.
                tracing::warn!(
                    folder_id = f.id,
                    path = %f.path,
                    canonical = %canon,
                    duplicate_of = other.id,
                    "Watched folder is a duplicate of another row; not migrated"
                );
                out.collisions += 1;
                continue;
            }
            let res = self.conn.execute(
                "UPDATE watched_folders SET path = ?1, updated_at = ?2 WHERE id = ?3",
                params![canon, Utc::now().to_rfc3339(), f.id],
            );
            if res.is_ok() {
                out.changed += 1;
            }
        }
        out
    }

    /// Update mutable fields of a watched folder.
    #[allow(clippy::too_many_arguments)]
    pub fn update_folder(
        &self,
        id: i64,
        label: Option<&str>,
        enabled: bool,
        watch_mode: &WatchMode,
        poll_interval_secs: i64,
        album_mode: &AlbumMode,
        album_name: Option<&str>,
        include_patterns: Option<&str>,
        exclude_patterns: Option<&str>,
        post_upload: &PostUpload,
        ignore_online_files: bool,
    ) -> Result<(), DbError> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE watched_folders
             SET label = ?2,
                 enabled = ?3,
                 watch_mode = ?4,
                 poll_interval_secs = ?5,
                 album_mode = ?6,
                 album_name = ?7,
                 include_patterns = ?8,
                 exclude_patterns = ?9,
                 post_upload = ?10,
                 ignore_online_files = ?11,
                 updated_at = ?12
             WHERE id = ?1",
            params![
                id,
                label,
                enabled,
                watch_mode.as_str(),
                poll_interval_secs,
                album_mode.as_str(),
                album_name,
                include_patterns,
                exclude_patterns,
                post_upload.as_str(),
                ignore_online_files,
                now,
            ],
        )?;
        Ok(())
    }

    // ── uploaded_files / deduplication ───────────────────────────────────────

    /// Return `true` if a file with the given SHA-1 hash has already been
    /// recorded as uploaded.
    pub fn is_file_uploaded(&self, hash: &str) -> Result<bool, DbError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM uploaded_files WHERE file_hash = ?1",
            params![hash],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// The Immich asset id of an upload with this SHA-1, if any.
    pub fn uploaded_asset_id(&self, hash: &str) -> Result<Option<String>, DbError> {
        Ok(self
            .conn
            .query_row(
                "SELECT immich_asset_id FROM uploaded_files
                 WHERE file_hash = ?1 AND immich_asset_id IS NOT NULL AND immich_asset_id != ''
                 LIMIT 1",
                params![hash],
                |r| r.get(0),
            )
            .optional()?)
    }

    // ── mirrored_albums ──────────────────────────────────────────────────────

    pub fn mirrored_album(&self, collection_id: &str) -> Result<Option<MirroredAlbum>, DbError> {
        Ok(self
            .conn
            .query_row(
                "SELECT immich_album_id, name FROM mirrored_albums WHERE photos_collection_id = ?1",
                params![collection_id],
                |r| {
                    Ok(MirroredAlbum {
                        immich_album_id: r.get(0)?,
                        name: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// Insert or update the Immich album mirroring a Photos album.
    pub fn record_mirrored_album(
        &self,
        collection_id: &str,
        immich_album_id: &str,
        name: &str,
    ) -> Result<(), DbError> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO mirrored_albums (photos_collection_id, immich_album_id, name, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (photos_collection_id) DO UPDATE SET
                immich_album_id = excluded.immich_album_id,
                name = excluded.name",
            params![collection_id, immich_album_id, name, now],
        )?;
        Ok(())
    }

    /// Drop a mapping whose Immich album is gone (deleted on the server).
    pub fn forget_mirrored_album(&self, collection_id: &str) -> Result<(), DbError> {
        self.conn.execute(
            "DELETE FROM mirrored_albums WHERE photos_collection_id = ?1",
            params![collection_id],
        )?;
        Ok(())
    }

    /// Record a successfully uploaded file.
    ///
    /// `file_mtime` is the file's modification time in seconds since the Unix
    /// epoch. Stored on the row so subsequent runs can use the fast-path
    /// (path, size, mtime) index in [`is_file_uploaded_fast_path`] to skip
    /// re-hashing — critical for cloud-placeholder files where opening the
    /// file for SHA-1 would trigger a re-download.
    #[allow(clippy::too_many_arguments)]
    pub fn record_upload(
        &self,
        file_path: &str,
        file_hash: &str,
        file_size: i64,
        file_mtime: i64,
        immich_asset_id: Option<&str>,
        device_asset_id: &str,
        server_url: &str,
    ) -> Result<i64, DbError> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO uploaded_files
                (file_path, file_hash, file_size, file_mtime, immich_asset_id,
                 device_asset_id, uploaded_at, server_url)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                file_path,
                file_hash,
                file_size,
                file_mtime,
                immich_asset_id,
                device_asset_id,
                now,
                server_url,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Fast-path dedup: return `true` if a file with this exact path, size,
    /// AND mtime has already been uploaded.
    ///
    /// Backed by the composite index on `(file_path, file_size, file_mtime)`.
    /// This is the first dedup layer the worker checks: if it hits, we skip
    /// without opening the file. On a cloud-placeholder file, that means we
    /// never trigger a re-download. On a miss we fall through to the SHA-1
    /// content dedup layer.
    pub fn is_file_uploaded_fast_path(
        &self,
        file_path: &str,
        file_size: i64,
        file_mtime: i64,
    ) -> Result<bool, DbError> {
        let hit: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM uploaded_files
                 WHERE file_path = ?1 AND file_size = ?2 AND file_mtime = ?3
                 LIMIT 1",
                params![file_path, file_size, file_mtime],
                |row| row.get(0),
            )
            .optional()?;
        Ok(hit.is_some())
    }

    /// Upload records for files inside `folder_path` (at any depth).
    ///
    /// Matching happens in Rust with `Path::starts_with` (whole components),
    /// not with SQL `LIKE`. A LIKE prefix treated `_` and `%` in folder names
    /// as wildcards, so `My_Pics` also matched `MyXPics`, and its hard-coded
    /// `\\` separator matched nothing at all on macOS (#95). Both the path
    /// as configured and its canonical form are tried, and stored paths lose
    /// any `\\?\` extended-length prefix the watcher added.
    pub fn get_uploaded_in_folder(
        &self,
        folder_path: &str,
    ) -> Result<Vec<UploadedRecord>, DbError> {
        let roots = folder_roots(folder_path);
        let mut stmt = self
            .conn
            .prepare("SELECT file_path, file_hash, file_size, file_mtime FROM uploaded_files")?;
        let rows = stmt.query_map([], |row| {
            Ok(UploadedRecord {
                file_path: row.get(0)?,
                file_hash: row.get(1)?,
                file_size: row.get(2)?,
                file_mtime: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            let rec = row?;
            if is_under_any(&rec.file_path, &roots) {
                out.push(rec);
            }
        }
        Ok(out)
    }

    // ── upload_queue ─────────────────────────────────────────────────────────

    /// Add a file to the upload queue with status `pending`.
    /// Returns the new queue entry `id`.
    pub fn enqueue(
        &self,
        file_path: &str,
        file_hash: Option<&str>,
        file_size: Option<i64>,
        folder_id: Option<i64>,
    ) -> Result<i64, DbError> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO upload_queue
                (file_path, file_hash, file_size, folder_id, status,
                 retry_count, queued_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', 0, ?5)",
            params![file_path, file_hash, file_size, folder_id, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Queue a Photos library asset. `file_path` holds a label, not a path.
    pub fn enqueue_photos(&self, local_id: &str) -> Result<i64, DbError> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO upload_queue
                (file_path, status, retry_count, queued_at, source, photos_local_id)
             VALUES (?1, 'pending', 0, ?2, 'photos', ?3)",
            params![format!("photos:{local_id}"), now, local_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// `true` if this Photos asset is already queued (in any state) or
    /// recorded in `photos_assets`, so the source doesn't enqueue it again.
    /// A deferred (iCloud-only) entry counts: retrying those is slice 3.
    pub fn photos_asset_seen(&self, local_id: &str) -> Result<bool, DbError> {
        let seen: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM upload_queue WHERE photos_local_id = ?1)
                 OR EXISTS (SELECT 1 FROM photos_assets WHERE local_id = ?1)",
            params![local_id],
            |r| r.get(0),
        )?;
        Ok(seen)
    }

    /// Record what one resource of a Photos asset became on the server.
    pub fn record_photos_asset(
        &self,
        local_id: &str,
        resource: &str,
        sha1: &str,
        immich_asset_id: &str,
        modified_at: Option<&str>,
    ) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT INTO photos_assets (local_id, resource, sha1, immich_asset_id, modified_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (local_id, resource) DO UPDATE SET
                sha1 = excluded.sha1,
                immich_asset_id = excluded.immich_asset_id,
                modified_at = excluded.modified_at",
            params![local_id, resource, sha1, immich_asset_id, modified_at],
        )?;
        Ok(())
    }

    /// Claim up to `limit` pending queue entries, atomically marking them
    /// `uploading`.  Returns the claimed entries.
    pub fn dequeue_pending(&self, limit: i64) -> Result<Vec<QueueEntry>, DbError> {
        // Use a transaction so the SELECT and UPDATE are atomic, preventing
        // two workers from claiming the same entries.
        let tx = self.conn.unchecked_transaction()?;

        let entries: Vec<QueueEntry> = {
            let mut stmt = tx.prepare(
                "SELECT id, file_path, file_hash, file_size, folder_id, status,
                        retry_count, error_message, queued_at, completed_at,
                        source, photos_local_id
                 FROM upload_queue
                 WHERE status = 'pending'
                 ORDER BY queued_at DESC
                 LIMIT ?1",
            )?;

            let rows = stmt
                .query_map(params![limit], map_queue_entry)?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };

        if !entries.is_empty() {
            let ids: Vec<String> = entries.iter().map(|e| e.id.to_string()).collect();
            let placeholders = ids.join(",");
            tx.execute_batch(&format!(
                "UPDATE upload_queue SET status = 'uploading' WHERE id IN ({placeholders})"
            ))?;
        }

        tx.commit()?;
        Ok(entries)
    }

    /// Update the status of a queue entry.
    ///
    /// Set `error_msg` to `Some(msg)` when transitioning to `failed`.
    /// `completed_at` is recorded automatically when the new status is
    /// `Completed` or `Failed`.
    /// Back to `pending` with `retry_count` untouched (a rejected API key
    /// isn't the entry's failure, #143).
    pub fn requeue_without_retry(&self, id: i64) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE upload_queue SET status = 'pending', error_message = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn update_queue_status(
        &self,
        id: i64,
        status: &QueueStatus,
        error_msg: Option<&str>,
    ) -> Result<(), DbError> {
        let completed_at: Option<String> = match status {
            QueueStatus::Completed | QueueStatus::Failed => Some(Utc::now().to_rfc3339()),
            _ => None,
        };

        // Increment retry_count on failure and on retry (pending with error).
        let retry_increment: i32 = match status {
            QueueStatus::Failed | QueueStatus::Pending => 1,
            _ => 0,
        };

        self.conn.execute(
            "UPDATE upload_queue
             SET status = ?2,
                 error_message = ?3,
                 completed_at = ?4,
                 retry_count = retry_count + ?5
             WHERE id = ?1",
            params![
                id,
                status.as_str(),
                error_msg,
                completed_at,
                retry_increment
            ],
        )?;
        Ok(())
    }

    /// Return aggregate counts of entries grouped by status.
    pub fn get_queue_stats(&self) -> Result<QueueStats, DbError> {
        let mut stats = QueueStats::default();

        let mut stmt = self
            .conn
            .prepare("SELECT status, COUNT(*) FROM upload_queue GROUP BY status")?;

        let rows = stmt.query_map([], |row| {
            let status: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((status, count))
        })?;

        for row in rows {
            let (status, count) = row?;
            match status.as_str() {
                "pending" => stats.pending = count,
                "uploading" => stats.uploading = count,
                "failed" => stats.failed = count,
                "completed" => stats.completed = count,
                _ => {}
            }
        }

        Ok(stats)
    }

    /// Return a single watched folder by id.
    pub fn get_folder(&self, id: i64) -> Result<Option<WatchedFolder>, DbError> {
        let result = self
            .conn
            .query_row(
                "SELECT id, path, label, enabled, watch_mode, poll_interval_secs,
                        album_mode, album_name, include_patterns, exclude_patterns,
                        post_upload, ignore_online_files, auto_added, created_at, updated_at
                 FROM watched_folders
                 WHERE id = ?1",
                params![id],
                map_watched_folder,
            )
            .optional()?;
        Ok(result)
    }

    /// Return recent queue entries ordered by most recent first.
    pub fn get_recent_queue_entries(&self, limit: i64) -> Result<Vec<QueueEntry>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_path, file_hash, file_size, folder_id, status,
                    retry_count, error_message, queued_at, completed_at,
                    source, photos_local_id
             FROM upload_queue
             ORDER BY id DESC
             LIMIT ?1",
        )?;

        let rows = stmt.query_map(params![limit], map_queue_entry)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DbError::Query)
    }

    /// Reset any entries stuck in `uploading` state back to `pending`.
    ///
    /// This recovers from crashes where the app exited mid-upload, leaving
    /// entries in a state that `dequeue_pending` will never pick up.
    pub fn reset_stale_uploading(&self) -> Result<usize, DbError> {
        let count = self.conn.execute(
            "UPDATE upload_queue
             SET status = 'pending',
                 error_message = NULL
             WHERE status = 'uploading'",
            [],
        )?;
        if count > 0 {
            info!(count, "Reset stale uploading entries to pending");
        }
        Ok(count)
    }
}

// ─── Row-mapping helpers ──────────────────────────────────────────────────────

fn map_watched_folder(row: &rusqlite::Row<'_>) -> rusqlite::Result<WatchedFolder> {
    Ok(WatchedFolder {
        id: row.get(0)?,
        path: row.get(1)?,
        label: row.get(2)?,
        enabled: row.get(3)?,
        watch_mode: WatchMode::from_str(&row.get::<_, String>(4)?),
        poll_interval_secs: row.get(5)?,
        album_mode: AlbumMode::from_str(&row.get::<_, String>(6)?),
        album_name: row.get(7)?,
        include_patterns: row.get(8)?,
        exclude_patterns: row.get(9)?,
        post_upload: PostUpload::from_str(&row.get::<_, String>(10)?),
        ignore_online_files: row.get(11)?,
        auto_added: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

fn map_queue_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueueEntry> {
    Ok(QueueEntry {
        id: row.get(0)?,
        file_path: row.get(1)?,
        file_hash: row.get(2)?,
        file_size: row.get(3)?,
        folder_id: row.get(4)?,
        status: QueueStatus::from_str(&row.get::<_, String>(5)?),
        retry_count: row.get(6)?,
        error_message: row.get(7)?,
        queued_at: row.get(8)?,
        completed_at: row.get(9)?,
        source: QueueSource::from_str(&row.get::<_, String>(10)?),
        photos_local_id: row.get(11)?,
    })
}

// ─── DbStore (QueueStore adapter) ────────────────────────────────────────────

/// Thread-safe wrapper around [`Database`] that implements the
/// [`QueueStore`](crate::upload::queue::QueueStore) trait.
///
/// This bridges the typed `db` layer (enums, `i64` sizes, string timestamps)
/// to the stringly-typed interface the upload pipeline expects.
pub struct DbStore {
    db: std::sync::Mutex<Database>,
}

impl DbStore {
    /// Wrap an open `Database` for use by the upload pipeline.
    pub fn new(db: Database) -> Self {
        Self {
            db: std::sync::Mutex::new(db),
        }
    }

    /// Access the inner `Mutex<Database>` for direct DB operations
    /// (e.g. folder management from `app.rs`).
    pub fn inner(&self) -> &std::sync::Mutex<Database> {
        &self.db
    }
}

impl crate::upload::queue::QueueStore for DbStore {
    fn enqueue(&self, entry: crate::upload::queue::NewQueueEntry) -> anyhow::Result<i64> {
        let db = self.db.lock().unwrap();
        let id = db.enqueue(
            &entry.file_path,
            entry.file_hash.as_deref(),
            entry.file_size.map(|s| s as i64),
            entry.folder_id,
        )?;
        Ok(id)
    }

    fn dequeue_pending(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::upload::queue::QueueEntry>> {
        let db = self.db.lock().unwrap();
        let entries = db.dequeue_pending(limit as i64)?;
        Ok(entries
            .into_iter()
            .map(|e| crate::upload::queue::QueueEntry {
                id: e.id,
                file_path: e.file_path,
                file_hash: e.file_hash,
                file_size: e.file_size.map(|s| s as u64),
                folder_id: e.folder_id,
                status: e.status.as_str().to_string(),
                retry_count: e.retry_count as u32,
                error_message: e.error_message,
                queued_at: chrono::DateTime::parse_from_rfc3339(&e.queued_at)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                completed_at: e.completed_at.and_then(|s| {
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .map(|dt| dt.with_timezone(&Utc))
                        .ok()
                }),
                source: e.source,
                photos_local_id: e.photos_local_id,
            })
            .collect())
    }

    fn uploaded_asset_id(&self, hash: &str) -> anyhow::Result<Option<String>> {
        let db = self.db.lock().unwrap();
        Ok(db.uploaded_asset_id(hash)?)
    }

    fn mirrored_album(&self, collection_id: &str) -> anyhow::Result<Option<MirroredAlbum>> {
        let db = self.db.lock().unwrap();
        Ok(db.mirrored_album(collection_id)?)
    }

    fn record_mirrored_album(
        &self,
        collection_id: &str,
        immich_album_id: &str,
        name: &str,
    ) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        db.record_mirrored_album(collection_id, immich_album_id, name)?;
        Ok(())
    }

    fn forget_mirrored_album(&self, collection_id: &str) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        db.forget_mirrored_album(collection_id)?;
        Ok(())
    }

    fn enqueue_photos(&self, local_id: &str) -> anyhow::Result<i64> {
        let db = self.db.lock().unwrap();
        Ok(db.enqueue_photos(local_id)?)
    }

    fn photos_asset_seen(&self, local_id: &str) -> anyhow::Result<bool> {
        let db = self.db.lock().unwrap();
        Ok(db.photos_asset_seen(local_id)?)
    }

    fn record_photos_asset(
        &self,
        local_id: &str,
        resource: &str,
        sha1: &str,
        immich_asset_id: &str,
        modified_at: Option<&str>,
    ) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        db.record_photos_asset(local_id, resource, sha1, immich_asset_id, modified_at)?;
        Ok(())
    }

    fn update_status(&self, id: i64, status: &str, error: Option<&str>) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        let qs = QueueStatus::from_str(status);
        db.update_queue_status(id, &qs, error)?;
        Ok(())
    }

    fn requeue_without_retry(&self, id: i64) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        db.requeue_without_retry(id)?;
        Ok(())
    }

    fn mark_completed(&self, id: i64, _asset_id: Option<&str>) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        db.update_queue_status(id, &QueueStatus::Completed, None)?;
        Ok(())
    }

    fn is_file_uploaded(&self, hash: &str) -> anyhow::Result<bool> {
        let db = self.db.lock().unwrap();
        Ok(db.is_file_uploaded(hash)?)
    }

    fn record_upload(
        &self,
        file_path: &str,
        hash: &str,
        size: u64,
        mtime: i64,
        asset_id: &str,
        device_asset_id: &str,
        server_url: &str,
    ) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        db.record_upload(
            file_path,
            hash,
            size as i64,
            mtime,
            Some(asset_id),
            device_asset_id,
            server_url,
        )?;
        Ok(())
    }

    fn is_file_uploaded_fast_path(
        &self,
        file_path: &str,
        file_size: i64,
        file_mtime: i64,
    ) -> anyhow::Result<bool> {
        let db = self.db.lock().unwrap();
        Ok(db.is_file_uploaded_fast_path(file_path, file_size, file_mtime)?)
    }

    fn get_queue_stats(&self) -> anyhow::Result<crate::upload::queue::QueueStats> {
        let db = self.db.lock().unwrap();
        let stats = db.get_queue_stats()?;
        Ok(crate::upload::queue::QueueStats {
            pending: stats.pending as u64,
            uploading: stats.uploading as u64,
            completed: stats.completed as u64,
            failed: stats.failed as u64,
            total: (stats.pending + stats.uploading + stats.completed + stats.failed) as u64,
        })
    }

    fn get_folder(&self, id: i64) -> anyhow::Result<Option<WatchedFolder>> {
        let db = self.db.lock().unwrap();
        Ok(db.get_folder(id)?)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// What [`Database::canonicalize_folder_paths`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FolderPathMigration {
    /// Rows rewritten to their canonical path.
    pub changed: usize,
    /// Rows left alone because another row already holds their canonical
    /// path (logged as a warning each).
    pub collisions: usize,
}

/// One `uploaded_files` row, as needed to re-check a file before acting on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedRecord {
    pub file_path: String,
    pub file_hash: String,
    pub file_size: i64,
    /// Seconds since the Unix epoch; `0` for rows from before schema v3.
    pub file_mtime: i64,
}

/// Strip the Windows extended-length prefix the watcher may store.
pub fn strip_extended_prefix(p: &str) -> &str {
    p.strip_prefix(r"\\?\").unwrap_or(p)
}

/// The folder as configured plus its canonical form, if it differs.
fn folder_roots(folder_path: &str) -> Vec<PathBuf> {
    let raw = PathBuf::from(strip_extended_prefix(folder_path));
    let mut roots = vec![raw.clone()];
    if let Ok(canon) = std::fs::canonicalize(&raw) {
        let canon = PathBuf::from(strip_extended_prefix(&canon.to_string_lossy()));
        if canon != raw {
            roots.push(canon);
        }
    }
    roots
}

fn is_under_any(file_path: &str, roots: &[PathBuf]) -> bool {
    let file = Path::new(strip_extended_prefix(file_path));
    roots
        .iter()
        .any(|r| file.starts_with(r) && file != r.as_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a fresh in-memory database and run migrations.
    fn open_test_db() -> Database {
        let conn = Connection::open_in_memory().expect("in-memory db");

        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let mut db = Database { conn };
        db.run_migrations().expect("migrations");
        db
    }

    #[cfg(unix)]
    #[test]
    fn canonicalize_folder_paths_migrates_resolvable_rows_only() {
        let real = tempfile::tempdir().unwrap();
        let canon = std::fs::canonicalize(real.path()).unwrap();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("via-link");
        std::os::unix::fs::symlink(&canon, &link).unwrap();
        let other = tempfile::tempdir().unwrap();
        let other_canon = std::fs::canonicalize(other.path()).unwrap();
        let link2 = links.path().join("dup-link");
        std::os::unix::fs::symlink(&other_canon, &link2).unwrap();

        let db = open_test_db();
        let a = db.add_folder(&link.to_string_lossy(), None, false).unwrap();
        let b = db.add_folder("/definitely/not/here", None, false).unwrap();
        // c's canonical path is already held by d: c must be left alone.
        let c = db
            .add_folder(&link2.to_string_lossy(), None, false)
            .unwrap();
        let d = db
            .add_folder(&other_canon.to_string_lossy(), None, false)
            .unwrap();

        let m = db.canonicalize_folder_paths();
        assert_eq!(
            m,
            FolderPathMigration {
                changed: 1,
                collisions: 1
            }
        );
        let path = |id| db.get_folder(id).unwrap().unwrap().path;
        assert_eq!(path(a), canon.to_string_lossy());
        assert_eq!(path(b), "/definitely/not/here");
        assert_eq!(path(c), link2.to_string_lossy());
        assert_eq!(path(d), other_canon.to_string_lossy());
        // Idempotent; the collision is reported again, not resolved.
        assert_eq!(
            db.canonicalize_folder_paths(),
            FolderPathMigration {
                changed: 0,
                collisions: 1
            }
        );
    }

    // ── get_uploaded_in_folder (#95) ─────────────────────────────────────────

    fn record(db: &Database, path: &std::path::Path) {
        db.record_upload(&path.to_string_lossy(), "h", 1, 1, None, "d", "s")
            .unwrap();
    }

    #[test]
    fn uploaded_in_folder_matches_components_not_like_wildcards() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let folder = root.join("My_Pics");
        let sibling = root.join("MyXPics"); // matches `My_Pics%` under LIKE
        let prefix_sib = root.join("My_Pics2"); // shares a string prefix
        for d in [&folder, &sibling, &prefix_sib] {
            std::fs::create_dir_all(d.join("sub")).unwrap();
        }
        let db = open_test_db();
        record(&db, &folder.join("a.jpg"));
        record(&db, &folder.join("sub").join("b.jpg"));
        record(&db, &sibling.join("c.jpg"));
        record(&db, &prefix_sib.join("d.jpg"));

        let mut got: Vec<String> = db
            .get_uploaded_in_folder(&folder.to_string_lossy())
            .unwrap()
            .into_iter()
            .map(|r| r.file_path)
            .collect();
        got.sort();
        let mut want = vec![
            folder.join("a.jpg").to_string_lossy().into_owned(),
            folder
                .join("sub")
                .join("b.jpg")
                .to_string_lossy()
                .into_owned(),
        ];
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn uploaded_in_folder_resolves_the_configured_path() {
        // Stored paths are canonical; the folder may be configured through a
        // symlink (macOS: /tmp → /private/tmp) or with trailing separators.
        let dir = tempfile::tempdir().unwrap();
        let canon = std::fs::canonicalize(dir.path()).unwrap();
        let db = open_test_db();
        record(&db, &canon.join("x.jpg"));
        #[cfg(unix)]
        {
            let link_parent = tempfile::tempdir().unwrap();
            let link = link_parent.path().join("link");
            std::os::unix::fs::symlink(&canon, &link).unwrap();
            assert_eq!(
                db.get_uploaded_in_folder(&link.to_string_lossy())
                    .unwrap()
                    .len(),
                1
            );
        }
        assert_eq!(
            db.get_uploaded_in_folder(&canon.to_string_lossy())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn extended_prefix_is_stripped() {
        assert_eq!(strip_extended_prefix(r"\\?\C:\x\a.jpg"), r"C:\x\a.jpg");
        assert_eq!(strip_extended_prefix("/Users/x/a.jpg"), "/Users/x/a.jpg");
    }

    #[cfg(windows)]
    #[test]
    fn extended_prefix_paths_match_their_folder() {
        assert!(is_under_any(
            r"\\?\C:\Photos\a.jpg",
            &[PathBuf::from(r"C:\Photos")]
        ));
        assert!(!is_under_any(
            r"\\?\C:\Photos2\a.jpg",
            &[PathBuf::from(r"C:\Photos")]
        ));
    }

    // ── watched_folders ──────────────────────────────────────────────────────

    #[test]
    fn add_and_get_folder() {
        let db = open_test_db();
        let id = db
            .add_folder("/some/path", Some("My Photos"), false)
            .unwrap();
        assert!(id > 0);

        let folders = db.get_folders().unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].path, "/some/path");
        assert_eq!(folders[0].label, Some("My Photos".to_string()));
        assert!(folders[0].enabled);
        assert_eq!(folders[0].watch_mode, WatchMode::Native);
        assert_eq!(folders[0].album_mode, AlbumMode::None);
    }

    #[test]
    fn get_folder_by_path() {
        let db = open_test_db();
        db.add_folder("/photos", None, false).unwrap();

        let found = db.get_folder_by_path("/photos").unwrap();
        assert!(found.is_some());

        let not_found = db.get_folder_by_path("/missing").unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn remove_folder() {
        let db = open_test_db();
        let id = db.add_folder("/to/remove", None, false).unwrap();
        db.remove_folder(id).unwrap();
        assert!(db.get_folders().unwrap().is_empty());
    }

    #[test]
    fn update_folder() {
        let db = open_test_db();
        let id = db.add_folder("/update/me", None, false).unwrap();

        db.update_folder(
            id,
            Some("Updated Label"),
            false,
            &WatchMode::Poll,
            60,
            &AlbumMode::Date,
            None,
            None,
            None,
            &PostUpload::Trash,
            false,
        )
        .unwrap();

        let folder = db.get_folder_by_path("/update/me").unwrap().unwrap();
        assert_eq!(folder.label, Some("Updated Label".to_string()));
        assert!(!folder.enabled);
        assert_eq!(folder.watch_mode, WatchMode::Poll);
        assert_eq!(folder.poll_interval_secs, 60);
        assert_eq!(folder.album_mode, AlbumMode::Date);
        assert_eq!(folder.post_upload, PostUpload::Trash);
        assert!(!folder.ignore_online_files);
    }

    #[test]
    fn add_folder_defaults_ignore_online_files_on() {
        let db = open_test_db();
        db.add_folder("/photos/cloud", None, false).unwrap();
        let folder = db.get_folder_by_path("/photos/cloud").unwrap().unwrap();
        assert!(
            folder.ignore_online_files,
            "Newly added folders should default to ignoring cloud-placeholder files"
        );
    }

    // ── uploaded_files ───────────────────────────────────────────────────────

    #[test]
    fn is_file_uploaded_and_record() {
        let db = open_test_db();
        let hash = "aabbcc112233";

        assert!(!db.is_file_uploaded(hash).unwrap());

        db.record_upload(
            "/photos/img.jpg",
            hash,
            1024,
            1_700_000_000,
            Some("asset-uuid-1"),
            "hash123-img.jpg",
            "https://immich.example.com",
        )
        .unwrap();

        assert!(db.is_file_uploaded(hash).unwrap());
    }

    #[test]
    fn fast_path_dedup_hit_on_same_path_size_mtime() {
        let db = open_test_db();
        db.record_upload(
            "/photos/cloud.jpg",
            "deadbeef",
            42_000,
            1_700_000_000,
            Some("asset-1"),
            "device-1",
            "https://immich.example.com",
        )
        .unwrap();

        assert!(db
            .is_file_uploaded_fast_path("/photos/cloud.jpg", 42_000, 1_700_000_000)
            .unwrap());
    }

    #[test]
    fn fast_path_dedup_miss_on_different_mtime() {
        let db = open_test_db();
        db.record_upload(
            "/photos/cloud.jpg",
            "deadbeef",
            42_000,
            1_700_000_000,
            Some("asset-1"),
            "device-1",
            "https://immich.example.com",
        )
        .unwrap();

        // Different mtime → miss (file was touched / re-saved).
        assert!(!db
            .is_file_uploaded_fast_path("/photos/cloud.jpg", 42_000, 1_700_000_001)
            .unwrap());
        // Different size → miss.
        assert!(!db
            .is_file_uploaded_fast_path("/photos/cloud.jpg", 42_001, 1_700_000_000)
            .unwrap());
        // Different path → miss.
        assert!(!db
            .is_file_uploaded_fast_path("/photos/other.jpg", 42_000, 1_700_000_000)
            .unwrap());
    }

    // ── upload_queue ─────────────────────────────────────────────────────────

    #[test]
    fn mirrored_albums_are_recorded_renamed_and_forgotten_by_collection_id() {
        let db = open_test_db();
        assert_eq!(db.mirrored_album("COLL-1").unwrap(), None);

        db.record_mirrored_album("COLL-1", "imm-a", "Hawaii")
            .unwrap();
        assert_eq!(
            db.mirrored_album("COLL-1").unwrap(),
            Some(MirroredAlbum {
                immich_album_id: "imm-a".into(),
                name: "Hawaii".into()
            })
        );

        // A rename keeps the same Immich album.
        db.record_mirrored_album("COLL-1", "imm-a", "Hawaii 2026")
            .unwrap();
        assert_eq!(
            db.mirrored_album("COLL-1").unwrap().unwrap().name,
            "Hawaii 2026"
        );

        db.forget_mirrored_album("COLL-1").unwrap();
        assert_eq!(db.mirrored_album("COLL-1").unwrap(), None);
    }

    #[test]
    fn uploaded_asset_id_finds_the_immich_id_for_a_hash() {
        let db = open_test_db();
        assert_eq!(db.uploaded_asset_id("h1").unwrap(), None);
        db.record_upload("/p/a.jpg", "h1", 1, 0, Some("imm-1"), "dev-1", "")
            .unwrap();
        assert_eq!(
            db.uploaded_asset_id("h1").unwrap().as_deref(),
            Some("imm-1")
        );
    }

    #[test]
    fn v4_photos_entries_round_trip_and_folder_entries_default() {
        let db = open_test_db();
        assert_eq!(db.get_schema_version().unwrap(), SCHEMA_VERSION);

        db.enqueue("/photos/a.jpg", Some("h"), Some(1), None)
            .unwrap();
        db.enqueue_photos("ABC-123/L0/001").unwrap();

        let mut entries = db.dequeue_pending(10).unwrap();
        entries.sort_by_key(|e| e.id);
        assert_eq!(entries[0].source, QueueSource::Folder);
        assert_eq!(entries[0].photos_local_id, None);
        assert_eq!(entries[1].source, QueueSource::Photos);
        assert_eq!(
            entries[1].photos_local_id.as_deref(),
            Some("ABC-123/L0/001")
        );
        assert_eq!(entries[1].file_path, "photos:ABC-123/L0/001");
    }

    /// Build a v3 database the way an existing install has one.
    fn v3_db() -> Database {
        let conn = Connection::open_in_memory().unwrap();
        let db = Database { conn };
        db.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS config (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .unwrap();
        db.migrate_v1().unwrap();
        db.migrate_v2().unwrap();
        db.migrate_v3().unwrap();
        db.set_schema_version(3).unwrap();
        db
    }

    fn has_column(db: &Database, table: &str, column: &str) -> bool {
        let mut stmt = db
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        names.iter().any(|n| n == column)
    }

    /// An upgrade that dies part-way through v4 must leave the DB exactly at
    /// v3, so the next start can run v4 again. Simulated by making v4's
    /// second ALTER fail after its first has run: without a transaction the
    /// first column would stay, and every later start would die on
    /// "duplicate column name: source" (Pia's review on #111).
    #[test]
    fn a_failed_v4_upgrade_rolls_back_and_the_next_start_succeeds() {
        let mut db = v3_db();
        db.conn
            .execute_batch("ALTER TABLE upload_queue ADD COLUMN photos_local_id TEXT;")
            .unwrap();

        assert!(
            db.run_migrations().is_err(),
            "v4 must fail on the planted column"
        );
        assert_eq!(db.get_schema_version().unwrap(), 3);
        assert!(
            !has_column(&db, "upload_queue", "source"),
            "the first ALTER must be rolled back with the rest of v4"
        );

        // The interruption clears (here: drop the planted column) and the
        // next start upgrades normally.
        db.conn
            .execute_batch("ALTER TABLE upload_queue DROP COLUMN photos_local_id;")
            .unwrap();
        db.run_migrations().unwrap();
        assert_eq!(db.get_schema_version().unwrap(), SCHEMA_VERSION);
        assert!(has_column(&db, "upload_queue", "source"));
    }

    #[test]
    fn v3_database_with_queued_rows_upgrades_to_v4_as_folder_entries() {
        let mut db = v3_db();
        db.conn
            .execute(
                "INSERT INTO upload_queue (file_path, status, retry_count, queued_at)
                 VALUES ('/old/a.jpg', 'pending', 0, '2026-10-01T00:00:00Z')",
                [],
            )
            .unwrap();

        db.run_migrations().unwrap();

        assert_eq!(db.get_schema_version().unwrap(), SCHEMA_VERSION);
        let e = db.dequeue_pending(10).unwrap().pop().unwrap();
        assert_eq!(e.file_path, "/old/a.jpg");
        assert_eq!(e.source, QueueSource::Folder);
        assert_eq!(e.photos_local_id, None);
    }

    #[test]
    fn photos_asset_seen_covers_queue_and_photos_assets() {
        let db = open_test_db();
        assert!(!db.photos_asset_seen("A").unwrap());
        db.enqueue_photos("A").unwrap();
        assert!(db.photos_asset_seen("A").unwrap());

        assert!(!db.photos_asset_seen("B").unwrap());
        db.record_photos_asset("B", "photo", "sha", "imm-1", None)
            .unwrap();
        db.record_photos_asset("B", "photo", "sha2", "imm-2", Some("2026-10-18T00:00:00Z"))
            .unwrap();
        assert!(db.photos_asset_seen("B").unwrap());
        let (sha, imm): (String, String) = db
            .conn
            .query_row(
                "SELECT sha1, immich_asset_id FROM photos_assets WHERE local_id = 'B'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((sha.as_str(), imm.as_str()), ("sha2", "imm-2"));
    }

    #[test]
    fn enqueue_and_dequeue() {
        let db = open_test_db();
        db.enqueue("/photos/a.jpg", Some("hash1"), Some(2048), None)
            .unwrap();
        db.enqueue("/photos/b.jpg", Some("hash2"), Some(4096), None)
            .unwrap();

        let entries = db.dequeue_pending(10).unwrap();
        assert_eq!(entries.len(), 2);
        // After dequeue_pending all returned entries should be Uploading.
        let stats = db.get_queue_stats().unwrap();
        assert_eq!(stats.uploading, 2);
        assert_eq!(stats.pending, 0);
    }

    #[test]
    fn update_queue_status_to_completed() {
        let db = open_test_db();
        let id = db.enqueue("/photos/c.jpg", None, None, None).unwrap();

        db.update_queue_status(id, &QueueStatus::Uploading, None)
            .unwrap();
        db.update_queue_status(id, &QueueStatus::Completed, None)
            .unwrap();

        let stats = db.get_queue_stats().unwrap();
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.uploading, 0);
    }

    #[test]
    fn update_queue_status_to_failed_increments_retry() {
        let db = open_test_db();
        let id = db.enqueue("/photos/d.jpg", None, None, None).unwrap();

        db.update_queue_status(id, &QueueStatus::Failed, Some("timeout"))
            .unwrap();

        let stats = db.get_queue_stats().unwrap();
        assert_eq!(stats.failed, 1);
    }

    #[test]
    fn reset_stale_uploading() {
        let db = open_test_db();
        let id1 = db.enqueue("/photos/g.jpg", None, None, None).unwrap();
        let id2 = db.enqueue("/photos/h.jpg", None, None, None).unwrap();

        // Simulate a crash: mark both as uploading.
        db.update_queue_status(id1, &QueueStatus::Uploading, None)
            .unwrap();
        db.update_queue_status(id2, &QueueStatus::Uploading, None)
            .unwrap();

        let stats = db.get_queue_stats().unwrap();
        assert_eq!(stats.uploading, 2);
        assert_eq!(stats.pending, 0);

        // Reset stale entries.
        let count = db.reset_stale_uploading().unwrap();
        assert_eq!(count, 2);

        let stats = db.get_queue_stats().unwrap();
        assert_eq!(stats.uploading, 0);
        assert_eq!(stats.pending, 2);
    }

    #[test]
    fn retry_count_increments_on_pending_retry() {
        let db = open_test_db();
        let id = db.enqueue("/photos/retry.jpg", None, None, None).unwrap();

        // Simulate: dequeued and failed, then set back to pending for retry.
        db.update_queue_status(id, &QueueStatus::Uploading, None)
            .unwrap();
        db.update_queue_status(id, &QueueStatus::Pending, Some("timeout"))
            .unwrap();

        // Check that retry_count was incremented.
        let entries = db.get_recent_queue_entries(10).unwrap();
        let entry = entries.iter().find(|e| e.id == id).unwrap();
        assert_eq!(entry.retry_count, 1);
    }

    #[test]
    fn queue_stats_default_zeroes() {
        let db = open_test_db();
        let stats = db.get_queue_stats().unwrap();
        assert_eq!(stats.pending, 0);
        assert_eq!(stats.uploading, 0);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.completed, 0);
    }
}

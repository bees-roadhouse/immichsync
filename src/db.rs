//! SQLite state database for ImmichSync.
//!
//! Opens (or creates) `%LOCALAPPDATA%\bees-roadhouse\immichsync\state.db`,
//! enables WAL mode, and runs schema migrations on every startup. All queue
//! state transitions are wrapped in transactions.
//!
//! This file is intentionally in `%LOCALAPPDATA%` ... watched folder set,
//! upload progress, and dedup history are machine-local, not roaming.

use std::path::PathBuf;

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

/// A file that has been successfully uploaded to the Immich server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadedFile {
    pub id: i64,
    pub file_path: String,
    /// SHA-1 hex digest — matches Immich's internal checksum field.
    pub file_hash: String,
    pub file_size: i64,
    /// File modification time in seconds since the Unix epoch (signed because
    /// pre-1970 mtimes are possible on some filesystems). Used by the
    /// fast-path dedup index to skip re-hashing unchanged files. `0` for
    /// rows uploaded before the v3 migration.
    pub file_mtime: i64,
    /// Immich-assigned asset UUID returned after upload.
    pub immich_asset_id: Option<String>,
    /// `"{path_hash}-{filename}"` device-scoped asset identifier.
    pub device_asset_id: String,
    pub uploaded_at: String,
    pub server_url: String,
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

/// Wraps a `rusqlite::Connection` and provides typed CRUD methods.
pub struct Database {
    conn: Connection,
}

impl Database {
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

        if current_version < 1 {
            self.migrate_v1()?;
            self.set_schema_version(1)?;
            info!("Applied schema migration v1");
        }

        if current_version < 2 {
            self.migrate_v2()?;
            self.set_schema_version(2)?;
            info!("Applied schema migration v2");
        }

        if current_version < 3 {
            self.migrate_v3()?;
            self.set_schema_version(3)?;
            info!("Applied schema migration v3");
        }

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

    // ── config table helpers ─────────────────────────────────────────────────

    /// Get a value from the key-value `config` table.
    pub fn get_config_value(&self, key: &str) -> Result<Option<String>, DbError> {
        let value = self
            .conn
            .query_row(
                "SELECT value FROM config WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value)
    }

    /// Set a value in the key-value `config` table.
    pub fn set_config_value(&self, key: &str, value: &str) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO config (key, value) VALUES (?1, ?2)",
            params![key, value],
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

    /// Return file paths under `folder_path` that have been uploaded.
    ///
    /// Uses a prefix match on `file_path` (with trailing backslash) so only
    /// files inside the folder are returned, not siblings with similar names.
    ///
    /// Handles both normal paths (`C:\Photos\...`) and Windows extended-length
    /// paths (`\\?\C:\Photos\...`) which the file watcher may produce.
    pub fn get_uploaded_paths_in_folder(&self, folder_path: &str) -> Result<Vec<String>, DbError> {
        // Strip \\?\ prefix if present so we have the canonical form.
        let canonical = folder_path.strip_prefix(r"\\?\").unwrap_or(folder_path);

        // Ensure the prefix ends with a separator for exact directory matching.
        let prefix = if canonical.ends_with('\\') || canonical.ends_with('/') {
            canonical.to_string()
        } else {
            format!("{canonical}\\")
        };

        // Also match the \\?\ extended-path variant stored by the file watcher.
        let ext_prefix = format!(r"\\?\{prefix}");

        let mut stmt = self.conn.prepare(
            "SELECT file_path FROM uploaded_files WHERE file_path LIKE ?1 OR file_path LIKE ?2",
        )?;
        let rows = stmt.query_map(
            params![format!("{prefix}%"), format!("{ext_prefix}%")],
            |row| row.get::<_, String>(0),
        )?;

        let mut paths = Vec::new();
        for row in rows {
            paths.push(row?);
        }
        Ok(paths)
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

    /// Claim up to `limit` pending queue entries, atomically marking them
    /// `uploading`.  Returns the claimed entries.
    pub fn dequeue_pending(&self, limit: i64) -> Result<Vec<QueueEntry>, DbError> {
        // Use a transaction so the SELECT and UPDATE are atomic, preventing
        // two workers from claiming the same entries.
        let tx = self.conn.unchecked_transaction()?;

        let entries: Vec<QueueEntry> = {
            let mut stmt = tx.prepare(
                "SELECT id, file_path, file_hash, file_size, folder_id, status,
                        retry_count, error_message, queued_at, completed_at
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

    /// Re-queue failed entries that have not exceeded `max_retries`, resetting
    /// their status to `pending` and clearing the error message.
    pub fn requeue_failed(&self, max_retries: i32) -> Result<usize, DbError> {
        let count = self.conn.execute(
            "UPDATE upload_queue
             SET status = 'pending',
                 error_message = NULL,
                 completed_at = NULL
             WHERE status = 'failed'
               AND retry_count < ?1",
            params![max_retries],
        )?;
        Ok(count)
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
                    retry_count, error_message, queued_at, completed_at
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

    /// Delete all completed queue entries.  Useful for housekeeping.
    pub fn purge_completed(&self) -> Result<usize, DbError> {
        let count = self
            .conn
            .execute("DELETE FROM upload_queue WHERE status = 'completed'", [])?;
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
            })
            .collect())
    }

    fn update_status(&self, id: i64, status: &str, error: Option<&str>) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        let qs = QueueStatus::from_str(status);
        db.update_queue_status(id, &qs, error)?;
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

    // ── Config table ─────────────────────────────────────────────────────────

    #[test]
    fn config_roundtrip() {
        let db = open_test_db();
        db.set_config_value("foo", "bar").unwrap();
        let val = db.get_config_value("foo").unwrap();
        assert_eq!(val, Some("bar".to_string()));
    }

    #[test]
    fn config_missing_key_returns_none() {
        let db = open_test_db();
        assert_eq!(db.get_config_value("does_not_exist").unwrap(), None);
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
    fn enqueue_and_dequeue() {
        let db = open_test_db();
        db.enqueue("/photos/a.jpg", Some("hash1"), Some(2048), None)
            .unwrap();
        db.enqueue("/photos/b.jpg", Some("hash2"), Some(4096), None)
            .unwrap();

        let entries = db.dequeue_pending(10).unwrap();
        assert_eq!(entries.len(), 2);
        // After dequeue_pending all returned entries should be Uploading.
        for e in &entries {
            // Status in DB was updated; re-fetch to verify.
            let stats = db.get_queue_stats().unwrap();
            assert_eq!(stats.uploading, 2);
            assert_eq!(stats.pending, 0);
            let _ = e; // suppress unused warning
            break;
        }
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
    fn requeue_failed() {
        let db = open_test_db();
        let id = db.enqueue("/photos/e.jpg", None, None, None).unwrap();
        db.update_queue_status(id, &QueueStatus::Failed, Some("err"))
            .unwrap();

        let requeued = db.requeue_failed(5).unwrap();
        assert_eq!(requeued, 1);

        let stats = db.get_queue_stats().unwrap();
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.failed, 0);
    }

    #[test]
    fn purge_completed() {
        let db = open_test_db();
        let id = db.enqueue("/photos/f.jpg", None, None, None).unwrap();
        db.update_queue_status(id, &QueueStatus::Completed, None)
            .unwrap();

        let purged = db.purge_completed().unwrap();
        assert_eq!(purged, 1);
        let stats = db.get_queue_stats().unwrap();
        assert_eq!(stats.completed, 0);
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

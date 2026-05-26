//! Upload queue manager.
//!
//! Owns the queue logic: enqueue, dedup check, and stats.
//! Does NOT own the database — it works through the `QueueStore` trait,
//! which the `db` module will implement.
//!
//! This design allows the queue to be constructed and tested independently
//! of any specific database backend.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use thiserror::Error;
use tracing::{debug, info};

use crate::upload::hasher;

// ─── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("Failed to hash file `{path}`: {source}")]
    Hash {
        path: String,
        source: hasher::HasherError,
    },

    #[error("Queue store error: {0}")]
    Store(#[from] anyhow::Error),
}

// ─── Store trait ─────────────────────────────────────────────────────────────

/// The interface the queue needs from its storage backend.
///
/// The `db` module's concrete implementation will satisfy this trait.
/// A mock implementation can be used in tests.
pub trait QueueStore: Send + Sync {
    /// Insert a new entry into the upload queue.
    /// Returns the row ID of the newly inserted entry.
    fn enqueue(&self, entry: NewQueueEntry) -> anyhow::Result<i64>;

    /// Return up to `limit` entries with status `"pending"`, ordered
    /// by insertion time (oldest first).
    fn dequeue_pending(&self, limit: usize) -> anyhow::Result<Vec<QueueEntry>>;

    /// Update the status (and optional error message) of a queue entry.
    fn update_status(&self, id: i64, status: &str, error: Option<&str>) -> anyhow::Result<()>;

    /// Mark a queue entry as completed and record the resulting asset ID.
    fn mark_completed(&self, id: i64, asset_id: Option<&str>) -> anyhow::Result<()>;

    /// Return `true` if a file with this SHA-1 hash has already been
    /// successfully uploaded (exists in `uploaded_files`).
    fn is_file_uploaded(&self, hash: &str) -> anyhow::Result<bool>;

    /// Insert a record into `uploaded_files` after a successful upload.
    ///
    /// `mtime` is the file's modification time in seconds since the Unix epoch,
    /// stored so the fast-path dedup index can hit on subsequent scans without
    /// re-hashing the file (which would trigger a cloud-storage download for
    /// placeholder files).
    #[allow(clippy::too_many_arguments)]
    fn record_upload(
        &self,
        file_path: &str,
        hash: &str,
        size: u64,
        mtime: i64,
        asset_id: &str,
        device_asset_id: &str,
        server_url: &str,
    ) -> anyhow::Result<()>;

    /// Fast-path dedup: returns `true` if a file with this exact path, size,
    /// and mtime has already been uploaded. Lets the worker skip a file
    /// without ever opening it — critical for cloud-storage placeholders.
    /// Default returns `Ok(false)` so existing mocks don't have to implement it.
    fn is_file_uploaded_fast_path(
        &self,
        _file_path: &str,
        _file_size: i64,
        _file_mtime: i64,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    /// Return current queue statistics.
    fn get_queue_stats(&self) -> anyhow::Result<QueueStats>;

    /// Look up a watched folder by id (needed for album auto-creation).
    fn get_folder(&self, _id: i64) -> anyhow::Result<Option<crate::db::WatchedFolder>> {
        Ok(None)
    }
}

// ─── Data types ──────────────────────────────────────────────────────────────

/// Data required to insert a new entry into the upload queue.
#[derive(Debug, Clone)]
pub struct NewQueueEntry {
    /// Absolute path to the file.
    pub file_path: String,

    /// SHA-1 hash of the file content (computed before enqueue).
    pub file_hash: Option<String>,

    /// File size in bytes.
    pub file_size: Option<u64>,

    /// The watched-folder row ID that triggered this file, if any.
    pub folder_id: Option<i64>,
}

/// A row from the `upload_queue` table.
///
/// `status`, `error_message`, `queued_at`, and `completed_at` are read only
/// by the in-module test suite (see `upload/mod.rs`); release builds
/// construct entries from the DB layer but never read these fields back.
#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub id: i64,
    pub file_path: String,
    pub file_hash: Option<String>,
    pub file_size: Option<u64>,
    pub folder_id: Option<i64>,

    /// One of `"pending"`, `"uploading"`, `"failed"`, `"completed"`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub status: String,

    pub retry_count: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub error_message: Option<String>,
    #[allow(dead_code)]
    pub queued_at: DateTime<Utc>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub completed_at: Option<DateTime<Utc>>,
}

/// Aggregate counts across all queue entries.
///
/// `failed` and `total` are populated by the test mock but the production UI
/// only surfaces `pending`, `uploading`, and `completed` today.
#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    pub pending: u64,
    pub uploading: u64,
    pub completed: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    pub failed: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    pub total: u64,
}

// ─── UploadQueue ─────────────────────────────────────────────────────────────

/// Manages the upload queue for a single Immich target.
///
/// Callers submit files via [`UploadQueue::process_file`]; the worker
/// picks them up via the store's [`QueueStore::dequeue_pending`].
pub struct UploadQueue {
    store: Arc<dyn QueueStore>,
    /// Maximum number of concurrent upload workers (advisory; not enforced here).
    /// Carried so the pipeline can hand it back to the worker config; the
    /// queue itself does not throttle on it.
    #[allow(dead_code)]
    pub concurrency: usize,
}

impl UploadQueue {
    /// Create a new queue backed by `store`.
    pub fn new(store: Arc<dyn QueueStore>, concurrency: usize) -> Self {
        Self { store, concurrency }
    }

    /// Hash the file, check for local dedup, and enqueue if not already uploaded.
    ///
    /// Returns `Ok(Some(id))` when a new queue entry was created, or
    /// `Ok(None)` when the file was skipped due to local dedup.
    ///
    /// Dedup runs in two layers:
    /// 1. **Fast path** ... `(canonical path, size, mtime)` against the
    ///    `uploaded_files` composite index. On hit we skip without opening
    ///    the file. This is the load-bearing path for cloud-storage
    ///    placeholders (OneDrive, iCloud, SeaDrive, etc.) ... opening a
    ///    placeholder for SHA-1 hashing would trigger a cloud download as a
    ///    side effect.
    /// 2. **Content hash** ... fall through, compute SHA-1, check against
    ///    `uploaded_files.file_hash`. Catches the moved-or-renamed case where
    ///    path/mtime changed but content is identical.
    pub fn process_file(
        &self,
        path: PathBuf,
        folder_id: Option<i64>,
    ) -> Result<Option<i64>, QueueError> {
        let path_str = path.display().to_string();

        // Read metadata once: needed for the fast-path query AND, if we miss,
        // for size + mtime fields we'll persist alongside the hash. Failing
        // to stat is fatal for this file ... bail with an error so the caller
        // logs it and moves on.
        let meta = std::fs::metadata(&path).map_err(|e| QueueError::Hash {
            path: path_str.clone(),
            source: hasher::HasherError::Open {
                path: path_str.clone(),
                source: e,
            },
        })?;
        let file_size_u64 = meta.len();
        let file_size_i64 = file_size_u64 as i64;
        let file_mtime = mtime_secs(&meta);

        // Placeholder re-check: the watcher's filter dropped cloud-placeholder
        // files at discovery time, but a file can be evicted to the cloud
        // *between* discovery and now (OneDrive/SeaDrive auto-evict under
        // storage pressure, or a user manually toggles "free up space"). If
        // we hash a placeholder, the read triggers a full cloud download
        // ... gigabytes of bandwidth and CPU for a file we're just going
        // to skip on the next watcher pass anyway.
        if crate::watch::filter::is_online_file(&meta) {
            info!(
                path = %path_str,
                "Skipping file that became a cloud placeholder after watcher discovery"
            );
            return Ok(None);
        }

        // 1. Fast-path dedup ... canonicalise the path so the lookup matches
        // the form `record_upload` stored (no `\\?\` prefix). Free win when
        // the file's been uploaded before with the same size + mtime.
        let canonical = canonical_path_for_storage(&path);
        let fast_hit = self
            .store
            .is_file_uploaded_fast_path(&canonical, file_size_i64, file_mtime)
            .map_err(QueueError::Store)?;
        if fast_hit {
            info!(
                path = %path_str,
                size = file_size_i64,
                mtime = file_mtime,
                "Skipping already-uploaded file (path+size+mtime fast-path)"
            );
            return Ok(None);
        }

        // 2. SHA-1 hash + content dedup. Safe to read at this point ... the
        // placeholder re-check above already covered the watcher/queue race.
        let hash = hasher::hash_file(&path).map_err(|source| QueueError::Hash {
            path: path_str.clone(),
            source,
        })?;

        let already_uploaded = self
            .store
            .is_file_uploaded(&hash)
            .map_err(QueueError::Store)?;

        if already_uploaded {
            info!(path = %path_str, hash = %hash, "Skipping already-uploaded file (content hash)");
            return Ok(None);
        }

        // 3. Enqueue.
        let entry = NewQueueEntry {
            file_path: path_str.clone(),
            file_hash: Some(hash.clone()),
            file_size: Some(file_size_u64),
            folder_id,
        };

        let id = self.store.enqueue(entry).map_err(QueueError::Store)?;

        debug!(
            path = %path_str,
            hash = %hash,
            id = id,
            "Enqueued file for upload"
        );

        Ok(Some(id))
    }

    /// Return current queue statistics.
    pub fn get_stats(&self) -> Result<QueueStats, QueueError> {
        self.store.get_queue_stats().map_err(QueueError::Store)
    }
}

/// Extract a file's modification time as seconds since the Unix epoch.
/// Returns `0` if the platform / filesystem doesn't expose `modified()`.
/// Stable across runs for the same physical file, which is what the fast-path
/// dedup index needs.
pub(crate) fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Normalize a path for storage in `uploaded_files.file_path` and for
/// fast-path lookups: strip the Windows extended-length `\\?\` prefix so
/// the value matches what `worker.rs` writes via `record_upload`.
pub(crate) fn canonical_path_for_storage(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ── Minimal in-memory QueueStore for testing ──────────────────────────

    #[derive(Default)]
    struct MockStore {
        entries: Mutex<Vec<QueueEntry>>,
        uploaded: Mutex<HashMap<String, bool>>,
        next_id: Mutex<i64>,
    }

    impl QueueStore for MockStore {
        fn enqueue(&self, entry: NewQueueEntry) -> anyhow::Result<i64> {
            let mut id_guard = self.next_id.lock().unwrap();
            *id_guard += 1;
            let id = *id_guard;

            let mut entries = self.entries.lock().unwrap();
            entries.push(QueueEntry {
                id,
                file_path: entry.file_path,
                file_hash: entry.file_hash,
                file_size: entry.file_size,
                folder_id: entry.folder_id,
                status: "pending".to_string(),
                retry_count: 0,
                error_message: None,
                queued_at: Utc::now(),
                completed_at: None,
            });
            Ok(id)
        }

        fn dequeue_pending(&self, limit: usize) -> anyhow::Result<Vec<QueueEntry>> {
            let entries = self.entries.lock().unwrap();
            Ok(entries
                .iter()
                .filter(|e| e.status == "pending")
                .take(limit)
                .cloned()
                .collect())
        }

        fn update_status(&self, id: i64, status: &str, error: Option<&str>) -> anyhow::Result<()> {
            let mut entries = self.entries.lock().unwrap();
            if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
                e.status = status.to_string();
                e.error_message = error.map(|s| s.to_string());
            }
            Ok(())
        }

        fn mark_completed(&self, id: i64, _asset_id: Option<&str>) -> anyhow::Result<()> {
            let mut entries = self.entries.lock().unwrap();
            if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
                e.status = "completed".to_string();
                e.completed_at = Some(Utc::now());
            }
            Ok(())
        }

        fn is_file_uploaded(&self, hash: &str) -> anyhow::Result<bool> {
            Ok(*self.uploaded.lock().unwrap().get(hash).unwrap_or(&false))
        }

        fn record_upload(
            &self,
            _file_path: &str,
            hash: &str,
            _size: u64,
            _mtime: i64,
            _asset_id: &str,
            _device_asset_id: &str,
            _server_url: &str,
        ) -> anyhow::Result<()> {
            self.uploaded.lock().unwrap().insert(hash.to_string(), true);
            Ok(())
        }

        fn get_queue_stats(&self) -> anyhow::Result<QueueStats> {
            let entries = self.entries.lock().unwrap();
            let mut stats = QueueStats::default();
            for e in entries.iter() {
                stats.total += 1;
                match e.status.as_str() {
                    "pending" => stats.pending += 1,
                    "uploading" => stats.uploading += 1,
                    "completed" => stats.completed += 1,
                    "failed" => stats.failed += 1,
                    _ => {}
                }
            }
            Ok(stats)
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────

    fn make_temp_file() -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"test image content").unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn process_file_enqueues_new_file() {
        let store = Arc::new(MockStore::default());
        let queue = UploadQueue::new(store.clone(), 2);
        let tmp = make_temp_file();

        let id = queue.process_file(tmp.path().to_path_buf(), None).unwrap();
        assert!(id.is_some());

        let stats = queue.get_stats().unwrap();
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.total, 1);
    }

    #[test]
    fn process_file_skips_already_uploaded() {
        let store = Arc::new(MockStore::default());
        let queue = UploadQueue::new(store.clone(), 2);
        let tmp = make_temp_file();

        // First pass: enqueued.
        let id1 = queue.process_file(tmp.path().to_path_buf(), None).unwrap();
        assert!(id1.is_some());

        // Simulate the file being uploaded: record its hash.
        let hash = hasher::hash_file(tmp.path()).unwrap();
        store
            .record_upload(
                &tmp.path().display().to_string(),
                &hash,
                18,
                0,
                "asset-abc",
                "device-abc",
                "https://immich.example.com",
            )
            .unwrap();

        // Second pass: should skip.
        let id2 = queue.process_file(tmp.path().to_path_buf(), None).unwrap();
        assert!(id2.is_none());

        // Queue should still only have the original 1 entry.
        let stats = queue.get_stats().unwrap();
        assert_eq!(stats.total, 1);
    }

    #[test]
    fn process_file_missing_path_returns_error() {
        let store = Arc::new(MockStore::default());
        let queue = UploadQueue::new(store, 2);
        let result = queue.process_file(PathBuf::from("/nonexistent/photo.jpg"), None);
        assert!(result.is_err());
    }

    /// Mark a file with FILE_ATTRIBUTE_OFFLINE so the placeholder re-check
    /// sees it as a cloud placeholder. Mirror of the helper in filter::tests.
    #[cfg(windows)]
    fn mark_offline(path: &std::path::Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let current = windows::Win32::Storage::FileSystem::GetFileAttributesW(
                windows::core::PCWSTR(wide.as_ptr()),
            );
            let combined = current | crate::watch::filter::FILE_ATTRIBUTE_OFFLINE;
            windows::Win32::Storage::FileSystem::SetFileAttributesW(
                windows::core::PCWSTR(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(combined),
            )
            .is_ok()
        }
    }

    #[cfg(windows)]
    #[test]
    fn process_file_skips_cloud_placeholder() {
        let store = Arc::new(MockStore::default());
        let queue = UploadQueue::new(store.clone(), 2);
        let tmp = make_temp_file();
        assert!(mark_offline(tmp.path()), "Failed to mark file offline");

        // Should be skipped (return Ok(None)) without hashing the file or
        // enqueueing — that's the whole point of the re-check.
        let id = queue.process_file(tmp.path().to_path_buf(), None).unwrap();
        assert!(id.is_none(), "Placeholder file should not enqueue");

        let stats = queue.get_stats().unwrap();
        assert_eq!(stats.total, 0, "Placeholder must not produce a queue row");
    }
}

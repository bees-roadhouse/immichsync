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
use tracing::{debug, info, warn};

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
    fn record_upload(
        &self,
        file_path: &str,
        hash: &str,
        size: u64,
        asset_id: &str,
        device_asset_id: &str,
        server_url: &str,
    ) -> anyhow::Result<()>;

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
/// Mirror of the DB row. Some fields are written during dequeue (so the
/// struct round-trips faithfully) but not yet read by any consumer outside
/// the in-memory mock store used in tests. Allowed at the struct level so
/// the model stays whole.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct QueueEntry {
    pub id: i64,
    pub file_path: String,
    pub file_hash: Option<String>,
    pub file_size: Option<u64>,
    pub folder_id: Option<i64>,

    /// One of `"pending"`, `"uploading"`, `"failed"`, `"completed"`.
    pub status: String,

    pub retry_count: u32,
    pub error_message: Option<String>,
    pub queued_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// Aggregate counts across all queue entries.
///
/// `failed` and `total` are populated by the store implementations but no
/// consumer reads them yet; kept for symmetry with the DB-level QueueStats.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct QueueStats {
    pub pending: u64,
    pub uploading: u64,
    pub completed: u64,
    pub failed: u64,
    pub total: u64,
}

// ─── UploadQueue ─────────────────────────────────────────────────────────────

/// Manages the upload queue for a single Immich target.
///
/// Callers submit files via [`UploadQueue::process_file`]; the worker
/// picks them up via the store's [`QueueStore::dequeue_pending`].
pub struct UploadQueue {
    store: Arc<dyn QueueStore>,
}

impl UploadQueue {
    /// Create a new queue backed by `store`. The `_concurrency` argument is
    /// kept on the signature for callers passing through a config value, but
    /// the concurrency cap is enforced by [`crate::upload::worker::WorkerConfig`],
    /// not by the queue.
    pub fn new(store: Arc<dyn QueueStore>, _concurrency: usize) -> Self {
        Self { store }
    }

    /// Hash the file, check for local dedup, and enqueue if not already uploaded.
    ///
    /// Returns `Ok(Some(id))` when a new queue entry was created, or
    /// `Ok(None)` when the file was skipped due to local dedup.
    pub fn process_file(
        &self,
        path: PathBuf,
        folder_id: Option<i64>,
    ) -> Result<Option<i64>, QueueError> {
        let path_str = path.display().to_string();

        // 1. Hash the file.
        let hash = hasher::hash_file(&path).map_err(|source| QueueError::Hash {
            path: path_str.clone(),
            source,
        })?;

        // 2. Local dedup check.
        let already_uploaded = self
            .store
            .is_file_uploaded(&hash)
            .map_err(QueueError::Store)?;

        if already_uploaded {
            info!(path = %path_str, hash = %hash, "Skipping already-uploaded file");
            return Ok(None);
        }

        // 3. Gather file size.
        let file_size = std::fs::metadata(&path)
            .map(|m| m.len())
            .map_err(|e| {
                warn!(path = %path_str, error = %e, "Could not read file size; using None");
            })
            .ok();

        // 4. Enqueue.
        let entry = NewQueueEntry {
            file_path: path_str.clone(),
            file_hash: Some(hash.clone()),
            file_size,
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
}

//! Async upload workers.
//!
//! `UploadWorker` runs a continuous loop that:
//!   1. Dequeues pending items (up to `concurrency` at a time).
//!   2. For each item, spawns a `tokio` task that uploads the file and
//!      updates the queue entry accordingly.
//!   3. Sleeps briefly when the queue is empty.
//!   4. Stops cleanly when signalled via `stop()`.
//!
//! ## Required dependency
//!
//! This module uses `async-trait = "0.1"` — add it to `Cargo.toml`:
//!
//! ```toml
//! async-trait = "0.1"
//! ```

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Notify;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::upload::metadata;
use crate::upload::queue::{QueueEntry, QueueStore};

// ─── Uploader trait ──────────────────────────────────────────────────────────

/// Result of a successful asset upload.
#[derive(Debug, Clone)]
pub struct UploadResult {
    /// The Immich asset UUID returned by the server.
    pub asset_id: String,

    /// `true` when the server reported the asset already existed
    /// (server-side duplicate detection).
    pub is_duplicate: bool,
}

/// An item to send to the server's bulk-duplicate-check endpoint.
#[derive(Debug, Clone)]
pub struct BulkCheckItem {
    /// Client-generated `deviceAssetId`.
    pub id: String,

    /// SHA-1 hex checksum of the file.
    pub checksum: String,
}

/// The server's response for one item in a bulk-duplicate-check.
#[derive(Debug, Clone)]
pub struct BulkCheckResult {
    /// Whether the asset already exists on the server.
    pub exists: bool,

    /// The Immich asset UUID, if it already exists.
    pub asset_id: Option<String>,
}

/// Abstraction over the Immich HTTP API, so the worker can be tested
/// without a real server.
///
/// The concrete implementation lives in `src/api/assets.rs`.
#[async_trait]
pub trait AssetUploader: Send + Sync {
    /// Upload a single file as a new asset.
    async fn upload_asset(
        &self,
        file_path: &Path,
        device_asset_id: &str,
        device_id: &str,
        created_at: DateTime<Utc>,
        modified_at: DateTime<Utc>,
    ) -> anyhow::Result<UploadResult>;

    /// Check whether a batch of assets already exists on the server.
    async fn check_duplicates(
        &self,
        items: Vec<BulkCheckItem>,
    ) -> anyhow::Result<Vec<BulkCheckResult>>;

    /// Get or create an album by name, then add an asset to it.
    async fn add_asset_to_album(&self, _album_name: &str, _asset_id: &str) -> anyhow::Result<()> {
        Ok(()) // Default no-op for mock implementations
    }
}

// ─── Worker configuration ────────────────────────────────────────────────────

/// Configuration for the upload worker pool.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Maximum number of simultaneous upload tasks.
    pub concurrency: usize,

    /// Maximum retry attempts before marking an entry as `"failed"`.
    pub max_retries: u32,

    /// Base delay for exponential backoff (doubles each retry, capped at 32 s).
    pub base_retry_delay: Duration,

    /// The device identifier sent to Immich with every upload.
    /// Format: `"immichsync-{hostname}"`.
    pub device_id: String,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        let hostname = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".to_string());

        Self {
            concurrency: 2,
            max_retries: 5,
            base_retry_delay: Duration::from_secs(1),
            device_id: format!("immichsync-{hostname}"),
        }
    }
}

// ─── UploadWorker ─────────────────────────────────────────────────────────────

/// Manages a pool of async upload workers.
pub struct UploadWorker {
    config: WorkerConfig,
    /// Signalled by `stop()` to ask the run loop to exit.
    shutdown: Arc<Notify>,
    /// Signalled by `pause()` / `resume()` to gate processing.
    paused: Arc<tokio::sync::watch::Sender<bool>>,
    paused_rx: tokio::sync::watch::Receiver<bool>,
}

impl UploadWorker {
    /// Create a new worker pool with the given configuration.
    pub fn new(config: WorkerConfig) -> Self {
        let (pause_tx, pause_rx) = tokio::sync::watch::channel(false);
        Self {
            config,
            shutdown: Arc::new(Notify::new()),
            paused: Arc::new(pause_tx),
            paused_rx: pause_rx,
        }
    }

    /// Signal all workers to stop after finishing their current upload.
    pub fn stop(&self) {
        self.shutdown.notify_waiters();
    }

    /// Pause upload processing.
    pub fn pause(&self) {
        let _ = self.paused.send(true);
        info!("Upload worker paused");
    }

    /// Resume upload processing after a pause.
    pub fn resume(&self) {
        let _ = self.paused.send(false);
        info!("Upload worker resumed");
    }

    /// Run the worker loop.
    ///
    /// This is an `async` function that never returns unless `stop()` is
    /// called. Spawn it with `tokio::spawn`.
    ///
    /// `store`   — the queue/upload-record backend
    /// `uploader` — the Immich API client
    pub async fn run(&self, store: Arc<dyn QueueStore>, uploader: Arc<dyn AssetUploader>) {
        info!(
            concurrency = self.config.concurrency,
            device_id = %self.config.device_id,
            "Upload worker loop starting"
        );

        let shutdown = self.shutdown.clone();
        let mut paused_rx = self.paused_rx.clone();

        loop {
            // ── Check for shutdown ────────────────────────────────────────
            // Use a non-blocking check so we can also check it inside the
            // sleep branch below.
            // We rely on `shutdown.notified()` being selectably pollable.

            // ── Pause gate ───────────────────────────────────────────────
            if *paused_rx.borrow() {
                debug!("Upload worker is paused; waiting for resume");
                tokio::select! {
                    _ = paused_rx.changed() => {
                        // Re-check at top of loop.
                        continue;
                    }
                    _ = shutdown.notified() => {
                        info!("Upload worker stopping (received shutdown while paused)");
                        return;
                    }
                }
            }

            // ── Dequeue ──────────────────────────────────────────────────
            let items = match store.dequeue_pending(self.config.concurrency) {
                Ok(v) => v,
                Err(e) => {
                    error!(error = %e, "Failed to dequeue pending uploads");
                    // Brief sleep before retrying to avoid tight error loops.
                    tokio::select! {
                        _ = sleep(Duration::from_secs(5)) => {}
                        _ = shutdown.notified() => {
                            info!("Upload worker stopping during dequeue error backoff");
                            return;
                        }
                    }
                    continue;
                }
            };

            if items.is_empty() {
                // Nothing to do — wait a moment before polling again.
                debug!("Upload queue empty; sleeping 2 s");
                tokio::select! {
                    _ = sleep(Duration::from_secs(2)) => {}
                    _ = shutdown.notified() => {
                        info!("Upload worker stopping (queue empty)");
                        return;
                    }
                }
                continue;
            }

            // ── Spawn one task per dequeued item ─────────────────────────
            let mut handles = Vec::with_capacity(items.len());

            for item in items {
                let store = store.clone();
                let uploader = uploader.clone();
                let config = self.config.clone();

                let handle = tokio::spawn(async move {
                    process_item(item, store, uploader, &config).await;
                });

                handles.push(handle);
            }

            // Wait for all spawned tasks before dequeuing the next batch,
            // so we don't exceed the concurrency limit.
            for handle in handles {
                if let Err(e) = handle.await {
                    error!(error = %e, "Upload task panicked");
                }
            }

            // Check shutdown before looping.
            // `try_recv`-style: if there's a pending notification, exit.
            // We achieve this with a zero-duration select.
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    info!("Upload worker stopping after batch");
                    return;
                }
                _ = async {} => {}  // immediate branch
            }
        }
    }
}

// ─── Per-item processing ──────────────────────────────────────────────────────

/// Process a single queue entry: upload or retry as appropriate.
async fn process_item(
    entry: QueueEntry,
    store: Arc<dyn QueueStore>,
    uploader: Arc<dyn AssetUploader>,
    config: &WorkerConfig,
) {
    let path = std::path::PathBuf::from(&entry.file_path);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());

    info!(
        id = entry.id,
        path = %entry.file_path,
        retry = entry.retry_count,
        "Processing upload queue entry"
    );

    // Mark as uploading.
    if let Err(e) = store.update_status(entry.id, "uploading", None) {
        error!(id = entry.id, error = %e, "Failed to mark entry as uploading");
        return;
    }

    // Build device_asset_id: "{sha1_of_full_path}-{filename}"
    // Use the stored hash if available, otherwise hash the path string.
    let path_hash = entry
        .file_hash
        .clone()
        .unwrap_or_else(|| sha1_of_string(&entry.file_path));
    let device_asset_id = format!("{}-{}", path_hash, file_name);

    // Extract file timestamps (EXIF preferred, filesystem fallback).
    let file_meta = match metadata::extract_metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            warn!(
                id = entry.id,
                path = %entry.file_path,
                error = %e,
                "Failed to extract metadata; using current time"
            );
            metadata::FileMetadata {
                created_at: Utc::now(),
                modified_at: Utc::now(),
            }
        }
    };

    // On retry, ask the server whether it already has this asset before
    // re-uploading. Catches the case where a prior attempt completed
    // server-side but the connection died before the client got the
    // response — without this pre-check we'd re-upload the whole file
    // (potentially 50+ GB) for nothing. Skipped on the first attempt
    // because the local DB dedup table already covers that path.
    if entry.retry_count > 0 {
        if let Some(checksum) = entry.file_hash.as_deref() {
            let check = uploader
                .check_duplicates(vec![BulkCheckItem {
                    id: device_asset_id.clone(),
                    checksum: checksum.to_string(),
                }])
                .await;
            match check {
                Ok(results) => {
                    if let Some(r) = results.first() {
                        if r.exists {
                            info!(
                                id = entry.id,
                                attempt = entry.retry_count,
                                asset_id = ?r.asset_id,
                                "Pre-check: server already has this asset, skipping re-upload"
                            );
                            if let Err(e) = store.mark_completed(entry.id, r.asset_id.as_deref()) {
                                error!(
                                    id = entry.id,
                                    error = %e,
                                    "Failed to mark entry completed after server-side pre-check"
                                );
                            }
                            return;
                        }
                    }
                }
                Err(e) => {
                    // Pre-check is best-effort — if it fails, fall through and
                    // attempt the upload normally.
                    warn!(
                        id = entry.id,
                        error = %e,
                        "Pre-check before retry failed; proceeding with full upload"
                    );
                }
            }
        }
    }

    // Attempt the upload.
    let result = uploader
        .upload_asset(
            &path,
            &device_asset_id,
            &config.device_id,
            file_meta.created_at,
            file_meta.modified_at,
        )
        .await;

    match result {
        Ok(upload_result) => {
            info!(
                id = entry.id,
                asset_id = %upload_result.asset_id,
                is_duplicate = upload_result.is_duplicate,
                "Upload succeeded"
            );

            // Record in uploaded_files.
            // Strip the \\?\ extended-path prefix so DB queries match cleanly.
            let clean_path = entry
                .file_path
                .strip_prefix(r"\\?\")
                .unwrap_or(&entry.file_path);
            if let Some(hash) = &entry.file_hash {
                let size = entry.file_size.unwrap_or(0);
                if let Err(e) = store.record_upload(
                    clean_path,
                    hash,
                    size,
                    &upload_result.asset_id,
                    &device_asset_id,
                    "", // server_url is not in QueueEntry; the caller may patch this
                ) {
                    warn!(
                        id = entry.id,
                        error = %e,
                        "Failed to record upload in uploaded_files"
                    );
                }
            }

            // Album auto-creation: look up folder and resolve album name.
            if let Some(folder_id) = entry.folder_id {
                if let Ok(Some(folder)) = store.get_folder(folder_id) {
                    let album_name = resolve_album_name(&folder, &path, &file_meta);
                    if let Some(name) = album_name {
                        if let Err(e) = uploader
                            .add_asset_to_album(&name, &upload_result.asset_id)
                            .await
                        {
                            warn!(
                                id = entry.id,
                                album = %name,
                                error = %e,
                                "Failed to add asset to album"
                            );
                        } else {
                            debug!(id = entry.id, album = %name, "Asset added to album");
                        }
                    }
                }
            }

            // Post-upload action: trash or delete the source file.
            if let Some(folder_id) = entry.folder_id {
                if let Ok(Some(folder)) = store.get_folder(folder_id) {
                    match folder.post_upload {
                        crate::db::PostUpload::Trash => {
                            if let Err(e) = trash_file(&path, &folder.path) {
                                warn!(
                                    id = entry.id,
                                    error = %e,
                                    "Failed to trash file after upload"
                                );
                            } else {
                                debug!(id = entry.id, "File moved to trash after upload");
                            }
                        }
                        crate::db::PostUpload::Delete => {
                            if let Err(e) = std::fs::remove_file(&path) {
                                warn!(
                                    id = entry.id,
                                    error = %e,
                                    "Failed to delete file after upload"
                                );
                            } else {
                                debug!(id = entry.id, "File deleted after upload");
                            }
                        }
                        crate::db::PostUpload::Keep => {}
                    }
                }
            }

            if let Err(e) = store.mark_completed(entry.id, Some(&upload_result.asset_id)) {
                error!(id = entry.id, error = %e, "Failed to mark entry completed");
            }
        }

        Err(e) => {
            let new_retry = entry.retry_count + 1;
            let error_msg = e.to_string();

            warn!(
                id = entry.id,
                path = %entry.file_path,
                attempt = new_retry,
                max = config.max_retries,
                error = %error_msg,
                "Upload failed"
            );

            if new_retry >= config.max_retries {
                // Exhausted retries — mark as failed permanently.
                error!(
                    id = entry.id,
                    path = %entry.file_path,
                    "Upload permanently failed after {} retries",
                    config.max_retries
                );

                // We set the retry_count via the error path; the DB
                // implementation is expected to increment it on status update.
                if let Err(db_err) = store.update_status(entry.id, "failed", Some(&error_msg)) {
                    error!(id = entry.id, error = %db_err, "Failed to mark entry as failed");
                }
            } else {
                // Schedule for retry with exponential backoff.
                let delay = backoff_delay(new_retry, config.base_retry_delay);
                debug!(
                    id = entry.id,
                    delay_secs = delay.as_secs(),
                    "Scheduling retry with backoff"
                );

                sleep(delay).await;

                // Return to pending so the next dequeue loop picks it up.
                // The retry_count increment is handled by the DB layer via
                // a dedicated "increment retry" update; here we just set
                // status back to pending.
                if let Err(db_err) = store.update_status(entry.id, "pending", Some(&error_msg)) {
                    error!(id = entry.id, error = %db_err, "Failed to reset entry to pending");
                }
            }
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Compute exponential backoff delay for a given retry attempt.
///
/// Sequence (base = 1 s): 1 s, 2 s, 4 s, 8 s, 16 s, 32 s (capped).
fn backoff_delay(retry: u32, base: Duration) -> Duration {
    const MAX_DELAY_SECS: u64 = 32;
    let multiplier = 1u64
        .checked_shl(retry.saturating_sub(1))
        .unwrap_or(u64::MAX);
    let secs = (base.as_secs() * multiplier).min(MAX_DELAY_SECS);
    Duration::from_secs(secs)
}

/// Resolve the album name for a file based on the folder's album mode.
fn resolve_album_name(
    folder: &crate::db::WatchedFolder,
    file_path: &std::path::Path,
    file_meta: &metadata::FileMetadata,
) -> Option<String> {
    use crate::db::AlbumMode;

    match folder.album_mode {
        AlbumMode::None => None,
        AlbumMode::Folder => {
            // Use the watched folder root's directory name.
            std::path::Path::new(&folder.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        }
        AlbumMode::Subfolder => {
            // Use the file's immediate parent directory name.
            file_path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
        }
        AlbumMode::Date => {
            // Use YYYY-MM based on the file's creation date.
            Some(file_meta.created_at.format("%Y-%m").to_string())
        }
        AlbumMode::Fixed => {
            // Use the user-specified album name.
            folder.album_name.clone()
        }
    }
}

/// SHA-1 of an arbitrary string (used to derive the path hash for
/// `device_asset_id` when the file hash is unavailable).
fn sha1_of_string(s: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

// ─── Trash helpers ────────────────────────────────────────────────────────────

/// The hidden directory name used for post-upload trash.
pub const TRASH_DIR_NAME: &str = ".immichsync-trash";

/// Move a file into `.immichsync-trash/` under the watched folder root,
/// preserving relative subfolder structure.
///
/// E.g. if `folder_root` is `C:\Photos` and `file_path` is
/// `C:\Photos\Vacation\img.jpg`, the file moves to
/// `C:\Photos\.immichsync-trash\Vacation\img.jpg`.
pub fn trash_file(file_path: &std::path::Path, folder_root: &str) -> anyhow::Result<()> {
    let root = std::path::Path::new(folder_root);
    let trash_root = root.join(TRASH_DIR_NAME);

    // Compute relative path from folder root.
    let relative = file_path.strip_prefix(root).unwrap_or_else(|_| {
        file_path
            .file_name()
            .map(std::path::Path::new)
            .unwrap_or(file_path)
    });

    let dest = trash_root.join(relative);

    // Create parent directories.
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Try rename first (same volume), fall back to copy+delete.
    if std::fs::rename(file_path, &dest).is_err() {
        std::fs::copy(file_path, &dest)?;
        std::fs::remove_file(file_path)?;
    }

    // Set the trash root directory as hidden (Windows).
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = trash_root
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            windows::Win32::Storage::FileSystem::SetFileAttributesW(
                windows::core::PCWSTR(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN,
            )
            .ok();
        }
    }

    Ok(())
}

/// Delete files in `.immichsync-trash/` directories that are older than
/// `retention_days`.  Removes empty directories afterwards.
///
/// Called on startup and periodically from the app's main loop.
pub fn cleanup_trash(folders: &[crate::db::WatchedFolder], retention_days: u32) {
    use std::time::{Duration, SystemTime};

    let max_age = Duration::from_secs(retention_days as u64 * 86400);
    let now = SystemTime::now();

    for folder in folders {
        if folder.post_upload != crate::db::PostUpload::Trash {
            continue;
        }

        let trash_root = std::path::Path::new(&folder.path).join(TRASH_DIR_NAME);
        if !trash_root.exists() {
            continue;
        }

        // Walk recursively and delete old files.
        let mut dirs_to_check = Vec::new();
        walk_and_delete(&trash_root, now, max_age, &mut dirs_to_check);

        // Remove empty directories (deepest first).
        dirs_to_check.sort_by_key(|b| std::cmp::Reverse(b.components().count()));
        for dir in dirs_to_check {
            if let Ok(mut entries) = std::fs::read_dir(&dir) {
                if entries.next().is_none() {
                    let _ = std::fs::remove_dir(&dir);
                }
            }
        }
    }
}

/// Recursively walk a directory, deleting files older than `max_age` and
/// collecting subdirectory paths for later empty-dir cleanup.
fn walk_and_delete(
    dir: &std::path::Path,
    now: std::time::SystemTime,
    max_age: std::time::Duration,
    dirs: &mut Vec<std::path::PathBuf>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(path = %dir.display(), error = %e, "Cannot read trash directory");
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            dirs.push(path.clone());
            walk_and_delete(&path, now, max_age, dirs);
        } else if path.is_file() {
            let should_delete = match path.metadata().and_then(|m| m.modified()) {
                Ok(modified) => now.duration_since(modified).unwrap_or_default() > max_age,
                Err(_) => false,
            };
            if should_delete {
                if let Err(e) = std::fs::remove_file(&path) {
                    warn!(path = %path.display(), error = %e, "Failed to delete expired trash file");
                } else {
                    debug!(path = %path.display(), "Deleted expired trash file");
                }
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use crate::upload::queue::{NewQueueEntry, QueueStats};

    // ── Mock QueueStore ───────────────────────────────────────────────────

    #[derive(Default)]
    struct MockStore {
        entries: Mutex<Vec<QueueEntry>>,
        uploaded: Mutex<HashMap<String, String>>,
        next_id: Mutex<i64>,
    }

    impl QueueStore for MockStore {
        fn enqueue(&self, entry: NewQueueEntry) -> anyhow::Result<i64> {
            let mut id_guard = self.next_id.lock().unwrap();
            *id_guard += 1;
            let id = *id_guard;
            self.entries.lock().unwrap().push(QueueEntry {
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
            Ok(self
                .entries
                .lock()
                .unwrap()
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
                if status == "pending" {
                    e.retry_count += 1;
                }
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
            Ok(self.uploaded.lock().unwrap().contains_key(hash))
        }

        fn record_upload(
            &self,
            file_path: &str,
            hash: &str,
            _size: u64,
            asset_id: &str,
            _device_asset_id: &str,
            _server_url: &str,
        ) -> anyhow::Result<()> {
            self.uploaded
                .lock()
                .unwrap()
                .insert(hash.to_string(), asset_id.to_string());
            let _ = file_path;
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

    // ── Mock AssetUploader ────────────────────────────────────────────────

    struct SucceedingUploader;

    #[async_trait]
    impl AssetUploader for SucceedingUploader {
        async fn upload_asset(
            &self,
            _file_path: &Path,
            device_asset_id: &str,
            _device_id: &str,
            _created_at: DateTime<Utc>,
            _modified_at: DateTime<Utc>,
        ) -> anyhow::Result<UploadResult> {
            Ok(UploadResult {
                asset_id: format!("asset-{}", device_asset_id),
                is_duplicate: false,
            })
        }

        async fn check_duplicates(
            &self,
            items: Vec<BulkCheckItem>,
        ) -> anyhow::Result<Vec<BulkCheckResult>> {
            Ok(items
                .into_iter()
                .map(|_| BulkCheckResult {
                    exists: false,
                    asset_id: None,
                })
                .collect())
        }
    }

    struct FailingUploader;

    #[async_trait]
    impl AssetUploader for FailingUploader {
        async fn upload_asset(
            &self,
            _file_path: &Path,
            _device_asset_id: &str,
            _device_id: &str,
            _created_at: DateTime<Utc>,
            _modified_at: DateTime<Utc>,
        ) -> anyhow::Result<UploadResult> {
            Err(anyhow::anyhow!("simulated upload failure"))
        }

        async fn check_duplicates(
            &self,
            items: Vec<BulkCheckItem>,
        ) -> anyhow::Result<Vec<BulkCheckResult>> {
            Ok(items
                .into_iter()
                .map(|_| BulkCheckResult {
                    exists: false,
                    asset_id: None,
                })
                .collect())
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────

    #[test]
    fn backoff_delay_sequence() {
        let base = Duration::from_secs(1);
        assert_eq!(backoff_delay(1, base), Duration::from_secs(1));
        assert_eq!(backoff_delay(2, base), Duration::from_secs(2));
        assert_eq!(backoff_delay(3, base), Duration::from_secs(4));
        assert_eq!(backoff_delay(4, base), Duration::from_secs(8));
        assert_eq!(backoff_delay(5, base), Duration::from_secs(16));
        assert_eq!(backoff_delay(6, base), Duration::from_secs(32));
        // Capped at 32 s.
        assert_eq!(backoff_delay(10, base), Duration::from_secs(32));
    }

    #[test]
    fn sha1_of_string_is_deterministic() {
        let h1 = sha1_of_string("hello");
        let h2 = sha1_of_string("hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 40);
    }

    #[tokio::test]
    async fn worker_uploads_file_and_marks_completed() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"photo data").unwrap();
        tmp.flush().unwrap();

        let store = Arc::new(MockStore::default());
        let uploader = Arc::new(SucceedingUploader);

        // Pre-populate the queue.
        let hash = crate::upload::hasher::hash_file(tmp.path()).unwrap();
        store
            .enqueue(NewQueueEntry {
                file_path: tmp.path().display().to_string(),
                file_hash: Some(hash.clone()),
                file_size: Some(10),
                folder_id: None,
            })
            .unwrap();

        let entry = store
            .dequeue_pending(1)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();

        let config = WorkerConfig {
            concurrency: 1,
            max_retries: 3,
            base_retry_delay: Duration::from_millis(1),
            device_id: "immichsync-test".to_string(),
        };

        process_item(entry, store.clone(), uploader, &config).await;

        let stats = store.get_queue_stats().unwrap();
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.pending, 0);
        assert!(store.is_file_uploaded(&hash).unwrap());
    }

    #[tokio::test]
    async fn worker_retries_on_failure_and_eventually_fails() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"photo data").unwrap();
        tmp.flush().unwrap();

        let store = Arc::new(MockStore::default());
        let uploader = Arc::new(FailingUploader);

        let hash = crate::upload::hasher::hash_file(tmp.path()).unwrap();
        store
            .enqueue(NewQueueEntry {
                file_path: tmp.path().display().to_string(),
                file_hash: Some(hash),
                file_size: Some(10),
                folder_id: None,
            })
            .unwrap();

        let config = WorkerConfig {
            concurrency: 1,
            max_retries: 2,
            base_retry_delay: Duration::from_millis(1), // fast for tests
            device_id: "immichsync-test".to_string(),
        };

        // Process until the entry is marked "failed".
        // With max_retries=2 and retry_count starting at 0, we need
        // 2 failures: first sets retry_count=1 (pending), second sets failed.
        for _ in 0..2 {
            let entry = {
                let entries = store.entries.lock().unwrap();
                entries
                    .iter()
                    .find(|e| e.status == "pending" || e.status == "uploading")
                    .cloned()
            };
            if let Some(mut e) = entry {
                e.status = "pending".to_string();
                process_item(e, store.clone(), uploader.clone(), &config).await;
            }
        }

        let stats = store.get_queue_stats().unwrap();
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.completed, 0);
    }
}

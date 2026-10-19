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
    /// The `deviceAssetId` echoed back. Production correlates results
    /// positionally with its request, so the field is never read in either
    /// build mode; it is kept on the struct to round-trip the API shape.
    #[allow(dead_code)]
    pub id: String,

    /// Whether the asset already exists on the server.
    pub exists: bool,

    /// The Immich asset UUID, if it already exists.
    pub asset_id: Option<String>,
}

/// Abstraction over the Immich HTTP API, so the worker can be tested
/// without a real server.
///
/// The concrete implementation lives in `src/api/assets.rs`.
// async-trait's generated signatures stack #[must_use] on a boxed future,
// which clippy 1.99's double_must_use flags; the code is the macro's.
#[allow(clippy::double_must_use)]
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

    /// Create a new album, never reusing one by name; returns its id. The
    /// Photos mirror uses this so it can't adopt an album a person made.
    async fn create_mirror_album(&self, _name: &str) -> anyhow::Result<String> {
        anyhow::bail!("album creation is not supported by this uploader")
    }

    /// Add one asset to an album by id.
    async fn add_to_album_by_id(
        &self,
        _album_id: &str,
        _asset_id: &str,
    ) -> Result<(), AlbumCallError> {
        Ok(())
    }

    /// Upload the still of a Live Photo, linked to its already-uploaded
    /// motion video (`livePhotoVideoId`). The default (for mocks that don't
    /// care) is a plain upload.
    #[allow(clippy::too_many_arguments)]
    async fn upload_live_still(
        &self,
        file_path: &Path,
        device_asset_id: &str,
        device_id: &str,
        created_at: DateTime<Utc>,
        modified_at: DateTime<Utc>,
        _live_photo_video_id: &str,
    ) -> anyhow::Result<UploadResult> {
        self.upload_asset(
            file_path,
            device_asset_id,
            device_id,
            created_at,
            modified_at,
        )
        .await
    }

    /// Rename an album by id.
    async fn rename_mirror_album(
        &self,
        _album_id: &str,
        _name: &str,
    ) -> Result<(), AlbumCallError> {
        Ok(())
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

    /// The Photos library source, when this build and config have one.
    /// Entries with `source = photos` fail cleanly without it.
    pub library: Option<Arc<LibrarySource>>,
}

// ─── Photos library source (ADR 0002) ────────────────────────────────────────

/// One original exported from the Photos library to a temp file.
#[derive(Debug)]
pub struct LibraryExport {
    /// The temp file, named after the asset's original filename (Immich
    /// takes the upload's filename from it). Deleted once the entry settles.
    pub path: std::path::PathBuf,
    /// Which resource this is: `"photo"` or `"video"`.
    pub resource: &'static str,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
    /// For a Live Photo: its original paired video (`.pairedVideo`), in the
    /// same per-export directory. Uploaded first; the still links to it
    /// with `livePhotoVideoId` (#83).
    pub paired_video: Option<std::path::PathBuf>,
    /// The Photos user albums this asset is in (smart and shared albums are
    /// never listed). Mirrored to Immich when album sync is on (#115).
    pub albums: Vec<LibraryAlbum>,
}

/// One Photos user album an exported asset belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryAlbum {
    /// `PHAssetCollection.localIdentifier`: stable across renames, so the
    /// Immich album follows a rename instead of being duplicated.
    pub local_id: String,
    /// The Immich name, folders flattened (`watch::photos::album_title`).
    pub title: String,
}

/// Why an export produced no file. Built by the macOS exporter only.
#[derive(Debug, thiserror::Error)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum LibraryExportError {
    /// The original exists only in iCloud. Downloading is a later slice
    /// (ADR 0002 §iCloud); until then the entry is deferred, not retried.
    #[error("the original is only in iCloud")]
    CloudOnly,
    /// The asset is no longer in the library (deleted, or access revoked).
    #[error("the asset is no longer in the Photos library")]
    Gone,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Exports Photos library originals. Blocking: the worker calls it from
/// `spawn_blocking`. The macOS implementation is `platform::photos`.
pub trait LibraryExporter: Send + Sync {
    fn export_original(&self, local_id: &str) -> Result<LibraryExport, LibraryExportError>;
}

/// What the worker needs to upload `source = photos` entries.
pub struct LibrarySource {
    pub exporter: Arc<dyn LibraryExporter>,
    /// Which Immich albums a library upload goes into (#115).
    pub albums: AlbumPolicy,
    /// Serialises album placement, so two workers placing photos from the
    /// same new Photos album create one Immich album, not two.
    pub album_lock: tokio::sync::Mutex<()>,
}

impl LibrarySource {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn new(exporter: Arc<dyn LibraryExporter>, albums: AlbumPolicy) -> Self {
        Self {
            exporter,
            albums,
            album_lock: tokio::sync::Mutex::new(()),
        }
    }
}

impl std::fmt::Debug for LibrarySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibrarySource")
            .field("albums", &self.albums)
            .finish_non_exhaustive()
    }
}

/// Album placement for library uploads, from `[photos]` (#115).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlbumPolicy {
    /// Mirror the asset's Photos user albums as Immich albums.
    pub mirror: bool,
    /// Also add every upload to this album (found or created by name).
    pub all_in: Option<String>,
}

/// Why an album call by id failed.
#[derive(Debug, thiserror::Error)]
pub enum AlbumCallError {
    /// The album no longer exists on the server, or this key can't see it.
    #[error("the album is gone")]
    Gone,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// `deviceAssetId` for a library resource: stable across exports, so a
/// re-export of the same asset is the same asset to Immich.
pub fn photos_device_asset_id(local_id: &str, resource: &str) -> String {
    format!("photos-{local_id}-{resource}")
}

/// Deletes the exported temp file (and its per-export directory) on drop,
/// however the entry ends.
struct TempExport(Vec<std::path::PathBuf>);

impl Drop for TempExport {
    fn drop(&mut self) {
        for path in &self.0 {
            if let Err(e) = std::fs::remove_file(path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(path = %path.display(), error = %e, "Failed to delete exported temp file");
                }
            }
        }
        for dir in self.0.iter().filter_map(|p| p.parent()) {
            let _ = std::fs::remove_dir(dir);
        }
    }
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
            library: None,
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

    /// Attach the Photos library source (see [`UploadPipeline::with_library`]).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn set_library(&mut self, library: LibrarySource) {
        self.config.library = Some(Arc::new(library));
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
    if entry.source == crate::upload::queue::QueueSource::Photos {
        return process_library_item(entry, store, uploader, config).await;
    }

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

    // Placeholder re-check: the file may have been evicted to the cloud
    // between enqueue and now (OneDrive / SeaDrive can auto-evict under
    // storage pressure). Opening the file for upload would trigger a full
    // cloud hydration ... gigabytes of bandwidth + CPU for a file the
    // watcher will re-skip on its next pass anyway. Mark the entry failed
    // with a recognisable marker; the watcher's reconciliation loop will
    // re-enqueue it once the file is local again.
    // Entries queued before #96 may sit inside a package (an app's library),
    // `.Trashes` or similar. Neither upload them nor let the post-upload
    // action move them out of there.
    if crate::watch::filter::in_skipped_dir(&path) {
        info!(id = entry.id, path = %entry.file_path,
            "Queued file is inside a skipped directory (package, trash, housekeeping); not uploading");
        if let Err(e) = store.update_status(
            entry.id,
            "failed",
            Some("inside a skipped directory (app library, trash or volume housekeeping)"),
        ) {
            error!(id = entry.id, error = %e, "Failed to mark skipped entry");
        }
        return;
    }

    if let Ok(meta) = std::fs::metadata(&path) {
        if crate::watch::filter::is_online_file(&meta) {
            info!(
                id = entry.id,
                path = %entry.file_path,
                "File became cloud placeholder; deferring upload until it's local again"
            );
            if let Err(e) = store.update_status(
                entry.id,
                "failed",
                Some("file became a cloud placeholder; will re-enqueue when local"),
            ) {
                error!(id = entry.id, error = %e, "Failed to mark deferred entry");
            }
            return;
        }
    }

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
                // Re-stat for mtime ... the fast-path dedup index needs it.
                // Best-effort; on failure we record `0` and the next scan
                // falls through to the SHA-1 layer for this file.
                let mtime = std::fs::metadata(&path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                if let Err(e) = store.record_upload(
                    clean_path,
                    hash,
                    size,
                    mtime,
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

            // Post-upload action: trash or delete the source file, but only
            // if it is still the file that was hashed and uploaded (#102):
            // one replaced between the hash and the end of the upload (a
            // camera reusing IMG_0001.JPG) must never be removed.
            if let Some(folder_id) = entry.folder_id {
                if let Ok(Some(folder)) = store.get_folder(folder_id) {
                    let action = if folder.post_upload == crate::db::PostUpload::Keep {
                        crate::db::PostUpload::Keep
                    } else if file_is_still_the_upload(&path, &entry) {
                        folder.post_upload
                    } else {
                        warn!(
                            id = entry.id,
                            path = %path.display(),
                            "File changed since it was uploaded; leaving it in place"
                        );
                        crate::db::PostUpload::Keep
                    };
                    match action {
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

        Err(e) => fail_or_retry(&entry, store.as_ref(), config, &e.to_string()).await,
    }
}

/// An attempt failed: back off and return the entry to `pending`, or mark
/// it `failed` once retries are exhausted.
async fn fail_or_retry(
    entry: &QueueEntry,
    store: &dyn QueueStore,
    config: &WorkerConfig,
    error_msg: &str,
) {
    let new_retry = entry.retry_count + 1;
    let error_msg = error_msg.to_string();

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

/// Upload one Photos library asset: export its original to a temp file,
/// then hash, dedup, upload and album it like a folder file. The temp file
/// is deleted however the entry ends; a retry exports again.
async fn process_library_item(
    entry: QueueEntry,
    store: Arc<dyn QueueStore>,
    uploader: Arc<dyn AssetUploader>,
    config: &WorkerConfig,
) {
    let Some(local_id) = entry.photos_local_id.clone() else {
        let _ = store.update_status(
            entry.id,
            "failed",
            Some("photos entry has no localIdentifier"),
        );
        return;
    };
    let Some(library) = config.library.clone() else {
        warn!(id = entry.id, %local_id, "Photos entry queued but this build has no library source");
        let _ = store.update_status(
            entry.id,
            "failed",
            Some("the Photos library source is not available in this build"),
        );
        return;
    };

    info!(id = entry.id, %local_id, retry = entry.retry_count, "Processing Photos library entry");
    if let Err(e) = store.update_status(entry.id, "uploading", None) {
        error!(id = entry.id, error = %e, "Failed to mark entry as uploading");
        return;
    }

    let exporter = library.exporter.clone();
    let id_for_export = local_id.clone();
    let exported =
        tokio::task::spawn_blocking(move || exporter.export_original(&id_for_export)).await;
    let export = match exported {
        Ok(Ok(x)) => x,
        Ok(Err(LibraryExportError::CloudOnly)) => {
            info!(id = entry.id, %local_id, "Photos original is only in iCloud; skipped (download is a later slice)");
            let _ = store.update_status(
                entry.id,
                "failed",
                Some("deferred: the original is only in iCloud"),
            );
            return;
        }
        Ok(Err(LibraryExportError::Gone)) => {
            info!(id = entry.id, %local_id, "Photos asset is gone; dropping entry");
            let _ = store.update_status(
                entry.id,
                "failed",
                Some("the asset is no longer in the Photos library"),
            );
            return;
        }
        Ok(Err(LibraryExportError::Other(e))) => {
            return fail_or_retry(
                &entry,
                store.as_ref(),
                config,
                &format!("export failed: {e:#}"),
            )
            .await;
        }
        Err(e) => {
            return fail_or_retry(&entry, store.as_ref(), config, &format!("export task: {e}"))
                .await;
        }
    };
    // Deletes the export however this ends. Dropped explicitly before any
    // retry backoff, so a private original never waits in temp; the retry
    // exports again.
    let cleanup = TempExport(
        std::iter::once(export.path.clone())
            .chain(export.paired_video.clone())
            .collect(),
    );
    let label = format!("photos:{local_id}/{}", export.resource);

    let hash = match crate::upload::hasher::hash_file(&export.path) {
        Ok(h) => h,
        Err(e) => {
            drop(cleanup);
            return fail_or_retry(&entry, store.as_ref(), config, &format!("hash failed: {e}"))
                .await;
        }
    };

    // Layer 2 spans sources: the same original already uploaded from a
    // watched folder (or an earlier export) is not uploaded again.
    if store.is_file_uploaded(&hash).unwrap_or(false) {
        info!(id = entry.id, %local_id, "Photos original already uploaded (same SHA-1); skipping");
        // Still put the existing asset into this photo's albums.
        let existing = store.uploaded_asset_id(&hash).ok().flatten();
        if let Some(asset_id) = existing.as_deref() {
            place_in_albums(
                asset_id,
                &export.albums,
                &library,
                store.as_ref(),
                uploader.as_ref(),
            )
            .await;
        }
        let _ = store.record_photos_asset(
            &local_id,
            export.resource,
            &hash,
            existing.as_deref().unwrap_or(""),
            None,
        );
        let _ = store.mark_completed(entry.id, None);
        return;
    }

    // A Live Photo's motion video goes up first; the still links to it, and
    // is never marked done before it (#83). Only now, once the still is known
    // not to be in Immich already: a video uploaded for a still that's
    // already there would sit unlinked in the timeline.
    let live_video_id = match export.paired_video.as_deref() {
        None => None,
        Some(video) => {
            match upload_paired_video(
                &local_id,
                video,
                &export,
                store.as_ref(),
                uploader.as_ref(),
                config,
            )
            .await
            {
                Ok(id) => Some(id),
                Err(e) => {
                    drop(cleanup);
                    return fail_or_retry(
                        &entry,
                        store.as_ref(),
                        config,
                        &format!("Live Photo video: {e:#}"),
                    )
                    .await;
                }
            }
        }
    };

    let device_asset_id = photos_device_asset_id(&local_id, export.resource);
    let result = match live_video_id.as_deref() {
        Some(video_id) => {
            uploader
                .upload_live_still(
                    &export.path,
                    &device_asset_id,
                    &config.device_id,
                    export.created_at,
                    export.modified_at,
                    video_id,
                )
                .await
        }
        None => {
            uploader
                .upload_asset(
                    &export.path,
                    &device_asset_id,
                    &config.device_id,
                    export.created_at,
                    export.modified_at,
                )
                .await
        }
    };

    match result {
        Ok(up) => {
            info!(id = entry.id, %local_id, asset_id = %up.asset_id, is_duplicate = up.is_duplicate, "Photos upload succeeded");
            let size = std::fs::metadata(&export.path)
                .map(|m| m.len())
                .unwrap_or(0);
            if let Err(e) =
                store.record_upload(&label, &hash, size, 0, &up.asset_id, &device_asset_id, "")
            {
                warn!(id = entry.id, error = %e, "Failed to record upload in uploaded_files");
            }
            let modified = export.modified_at.to_rfc3339();
            if let Err(e) = store.record_photos_asset(
                &local_id,
                export.resource,
                &hash,
                &up.asset_id,
                Some(&modified),
            ) {
                warn!(id = entry.id, error = %e, "Failed to record Photos asset");
            }
            place_in_albums(
                &up.asset_id,
                &export.albums,
                &library,
                store.as_ref(),
                uploader.as_ref(),
            )
            .await;
            if let Err(e) = store.mark_completed(entry.id, Some(&up.asset_id)) {
                error!(id = entry.id, error = %e, "Failed to mark entry completed");
            }
        }
        Err(e) => {
            drop(cleanup);
            fail_or_retry(&entry, store.as_ref(), config, &e.to_string()).await
        }
    }
}

/// Make sure a Live Photo's motion video is on the server and return its
/// Immich id: reused if those bytes were uploaded before (a retry after the
/// still failed), otherwise uploaded and recorded as resource `paired_video`.
async fn upload_paired_video(
    local_id: &str,
    video: &Path,
    export: &LibraryExport,
    store: &dyn QueueStore,
    uploader: &dyn AssetUploader,
    config: &WorkerConfig,
) -> anyhow::Result<String> {
    const RESOURCE: &str = "paired_video";
    let hash = crate::upload::hasher::hash_file(video)?;
    if let Some(existing) = store.uploaded_asset_id(&hash)? {
        debug!(%local_id, asset_id = %existing, "Live Photo video already uploaded");
        return Ok(existing);
    }
    let device_asset_id = photos_device_asset_id(local_id, RESOURCE);
    let up = uploader
        .upload_asset(
            video,
            &device_asset_id,
            &config.device_id,
            export.created_at,
            export.modified_at,
        )
        .await?;
    let size = std::fs::metadata(video).map(|m| m.len()).unwrap_or(0);
    let label = format!("photos:{local_id}/{RESOURCE}");
    store.record_upload(&label, &hash, size, 0, &up.asset_id, &device_asset_id, "")?;
    store.record_photos_asset(local_id, RESOURCE, &hash, &up.asset_id, None)?;
    info!(%local_id, asset_id = %up.asset_id, "Live Photo video uploaded");
    Ok(up.asset_id)
}

/// Put an uploaded library asset into its Immich albums per `library.albums`:
/// each Photos user album's mirror (created on first need, renamed when the
/// Photos album was, recreated if deleted on the server), then the optional
/// catch-all. Failures are logged; the upload itself already succeeded.
pub(crate) async fn place_in_albums(
    asset_id: &str,
    albums: &[LibraryAlbum],
    library: &LibrarySource,
    store: &dyn QueueStore,
    uploader: &dyn AssetUploader,
) {
    let policy = &library.albums;
    let mirror = policy.mirror && !albums.is_empty();
    if !mirror && policy.all_in.is_none() {
        return;
    }
    // Both kinds can create an album on first need (the catch-all by name),
    // so both run under the lock.
    let _serial = library.album_lock.lock().await;
    if mirror {
        for album in albums {
            if let Err(e) = place_in_mirror(asset_id, album, store, uploader).await {
                warn!(album = %album.title, error = %e, "Failed to add asset to mirrored album");
            }
        }
    }
    if let Some(name) = policy.all_in.as_deref() {
        if let Err(e) = uploader.add_asset_to_album(name, asset_id).await {
            warn!(album = %name, error = %e, "Failed to add asset to the catch-all album");
        }
    }
}

/// Add `asset_id` to the Immich album mirroring `album`, creating it on first
/// need and recreating it once if it was deleted on the server.
async fn place_in_mirror(
    asset_id: &str,
    album: &LibraryAlbum,
    store: &dyn QueueStore,
    uploader: &dyn AssetUploader,
) -> anyhow::Result<()> {
    let mut target = match store.mirrored_album(&album.local_id)? {
        Some(m) if m.name != album.title => {
            match uploader
                .rename_mirror_album(&m.immich_album_id, &album.title)
                .await
            {
                Ok(()) => {
                    info!(from = %m.name, to = %album.title, "Mirrored album renamed");
                    store.record_mirrored_album(
                        &album.local_id,
                        &m.immich_album_id,
                        &album.title,
                    )?;
                    Some(m.immich_album_id)
                }
                Err(AlbumCallError::Gone) => None,
                Err(AlbumCallError::Other(e)) => {
                    // Keep using it under the old name; retried next time.
                    warn!(album = %m.name, error = %e, "Failed to rename mirrored album");
                    Some(m.immich_album_id)
                }
            }
        }
        Some(m) => Some(m.immich_album_id),
        None => None,
    };

    for attempt in 0..2 {
        let album_id = match target.take() {
            Some(id) => id,
            None => {
                let id = uploader.create_mirror_album(&album.title).await?;
                info!(album = %album.title, album_id = %id, "Created Immich album for a Photos album");
                store.record_mirrored_album(&album.local_id, &id, &album.title)?;
                id
            }
        };
        match uploader.add_to_album_by_id(&album_id, asset_id).await {
            Ok(()) => return Ok(()),
            Err(AlbumCallError::Gone) if attempt == 0 => {
                info!(album = %album.title, "Mirrored album was deleted on the server; recreating");
                store.forget_mirrored_album(&album.local_id)?;
            }
            Err(AlbumCallError::Gone) => anyhow::bail!("the recreated album is gone too"),
            Err(AlbumCallError::Other(e)) => return Err(e),
        }
    }
    Ok(())
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

/// Post-upload "Move to Trash".
///
/// macOS: the Finder Trash (`platform::trash`), so the user can put files
/// back from Finder (#95). Elsewhere: `.immichsync-trash/` under the
/// watched folder, purged after `trash_retention_days`.
pub fn trash_file(file_path: &std::path::Path, folder_root: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let _ = folder_root;
        crate::platform::trash::move_to_trash(file_path)?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    trash_into_folder(file_path, folder_root)
}

/// Move a file into `.immichsync-trash/` under the watched folder root,
/// preserving relative subfolder structure.
///
/// E.g. if `folder_root` is `C:\Photos` and `file_path` is
/// `C:\Photos\Vacation\img.jpg`, the file moves to
/// `C:\Photos\.immichsync-trash\Vacation\img.jpg`.
#[cfg(not(target_os = "macos"))]
fn trash_into_folder(file_path: &std::path::Path, folder_root: &str) -> anyhow::Result<()> {
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

    // Never replace an earlier trashed file with the same relative path (a
    // camera reusing IMG_0001.JPG, #98): that copy is the user's safety net.
    let landed = move_no_clobber(file_path, &dest)?;
    if landed != dest {
        debug!(from = %file_path.display(), to = %landed.display(),
            "Trash name taken; kept both");
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

/// Move `src` to `dest`, or to `name (1).ext`, `name (2).ext`, … if `dest`
/// is taken. Returns where the file landed.
///
/// `std::fs::rename` and `copy` both silently replace an existing target, so
/// each candidate is first claimed with `create_new` (atomic: fails if the
/// name exists) and the move then lands on our own placeholder. Rename is
/// tried first (same volume); otherwise the bytes are copied into the
/// placeholder and `src` removed. On failure the placeholder is cleaned up.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn move_no_clobber(
    src: &std::path::Path,
    dest: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    use std::io::ErrorKind;

    let parent = dest.parent().unwrap_or_else(|| std::path::Path::new("."));
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = dest
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    for n in 0..10_000u32 {
        let candidate = if n == 0 {
            dest.to_path_buf()
        } else {
            parent.join(format!("{stem} ({n}){ext}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
        let moved = std::fs::rename(src, &candidate).or_else(|_| {
            std::fs::copy(src, &candidate)?;
            std::fs::remove_file(src)
        });
        return match moved {
            Ok(()) => Ok(candidate),
            Err(e) => {
                let _ = std::fs::remove_file(&candidate);
                Err(e)
            }
        };
    }
    Err(std::io::Error::other(
        "no free trash name after 10000 tries",
    ))
}

/// The post-upload guard: the file still has the size and SHA-1 it was
/// queued with. The mtime shortcut is deliberately skipped (`file_mtime: 0`),
/// because the upload's own record re-stats the file and would vouch for a
/// replacement; this re-hash costs one read, only for Trash/Delete folders.
/// An entry without a hash can't be verified and is kept.
fn file_is_still_the_upload(path: &std::path::Path, entry: &QueueEntry) -> bool {
    let (Some(hash), Some(size)) = (entry.file_hash.as_deref(), entry.file_size) else {
        return false;
    };
    let rec = crate::db::UploadedRecord {
        file_path: entry.file_path.clone(),
        file_hash: hash.to_string(),
        file_size: size as i64,
        file_mtime: 0,
    };
    still_matches_upload(path, &rec).unwrap_or(false)
}

/// True when the file at `path` is still the one that was uploaded: same size
/// and mtime as recorded, or (for rows without an mtime, or when only the
/// mtime moved) the same SHA-1. Guards trash/delete of already-uploaded
/// files, so a file edited or replaced since its upload (a camera reusing
/// `IMG_0001.JPG`, an edit saved over an export) is never removed (#95).
pub fn still_matches_upload(
    path: &std::path::Path,
    rec: &crate::db::UploadedRecord,
) -> std::io::Result<bool> {
    let meta = std::fs::metadata(path)?;
    if meta.len() as i64 != rec.file_size {
        return Ok(false);
    }
    if rec.file_mtime != 0 && crate::upload::queue::mtime_secs(&meta) == rec.file_mtime {
        return Ok(true);
    }
    match crate::upload::hasher::hash_file(path) {
        Ok(h) => Ok(h.eq_ignore_ascii_case(&rec.file_hash)),
        Err(e) => Err(std::io::Error::other(e.to_string())),
    }
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
        dirs_to_check.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
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

    // ── move_no_clobber (#98) ────────────────────────────────────────────────

    #[test]
    fn trashing_the_same_relative_path_twice_keeps_both() {
        let dir = tempfile::tempdir().unwrap();
        let trash = dir.path().join(".immichsync-trash/DCIM/IMG_0001.JPG");
        std::fs::create_dir_all(trash.parent().unwrap()).unwrap();

        let first = dir.path().join("first.jpg");
        std::fs::write(&first, b"first card").unwrap();
        let a = move_no_clobber(&first, &trash).unwrap();

        let second = dir.path().join("second.jpg");
        std::fs::write(&second, b"second card").unwrap();
        let b = move_no_clobber(&second, &trash).unwrap();

        let third = dir.path().join("third.jpg");
        std::fs::write(&third, b"third card").unwrap();
        let c = move_no_clobber(&third, &trash).unwrap();

        assert_eq!(a, trash);
        assert_eq!(b.file_name().unwrap(), "IMG_0001 (1).JPG");
        assert_eq!(c.file_name().unwrap(), "IMG_0001 (2).JPG");
        assert_eq!(std::fs::read(&a).unwrap(), b"first card");
        assert_eq!(std::fs::read(&b).unwrap(), b"second card");
        assert_eq!(std::fs::read(&c).unwrap(), b"third card");
        assert!(!first.exists() && !second.exists() && !third.exists());
    }

    #[test]
    fn failed_move_leaves_no_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("x.jpg");
        let missing = dir.path().join("does-not-exist.jpg");
        assert!(move_no_clobber(&missing, &dest).is_err());
        assert!(!dest.exists(), "placeholder must be cleaned up");
    }

    // ── still_matches_upload (#95) ───────────────────────────────────────────

    fn rec_for(path: &std::path::Path) -> crate::db::UploadedRecord {
        let meta = std::fs::metadata(path).unwrap();
        crate::db::UploadedRecord {
            file_path: path.to_string_lossy().into_owned(),
            file_hash: crate::upload::hasher::hash_file(path).unwrap(),
            file_size: meta.len() as i64,
            file_mtime: crate::upload::queue::mtime_secs(&meta),
        }
    }

    #[test]
    fn unchanged_file_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("IMG_0001.JPG");
        std::fs::write(&p, vec![7u8; 4096]).unwrap();
        let rec = rec_for(&p);
        assert!(still_matches_upload(&p, &rec).unwrap());
    }

    #[test]
    fn replaced_file_with_new_size_does_not_match() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("IMG_0001.JPG");
        std::fs::write(&p, vec![7u8; 4096]).unwrap();
        let rec = rec_for(&p);
        std::fs::write(&p, vec![9u8; 5000]).unwrap(); // camera reused the name
        assert!(!still_matches_upload(&p, &rec).unwrap());
    }

    #[test]
    fn same_size_different_bytes_falls_back_to_hash() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("edit.jpg");
        std::fs::write(&p, vec![1u8; 4096]).unwrap();
        let mut rec = rec_for(&p);
        std::fs::write(&p, vec![2u8; 4096]).unwrap();
        rec.file_mtime = 0; // legacy row: no mtime, so the hash decides
        assert!(!still_matches_upload(&p, &rec).unwrap());
        rec.file_mtime = 1; // mtime differs: also falls through to the hash
        assert!(!still_matches_upload(&p, &rec).unwrap());
    }
    use std::collections::HashMap;
    use std::sync::Mutex;

    use crate::upload::queue::{NewQueueEntry, QueueStats};

    // ── Mock QueueStore ───────────────────────────────────────────────────

    #[derive(Default)]
    struct MockStore {
        entries: Mutex<Vec<QueueEntry>>,
        uploaded: Mutex<HashMap<String, String>>,
        next_id: Mutex<i64>,
        photos_assets: Mutex<Vec<(String, String, String)>>,
        mirrored: Mutex<HashMap<String, crate::db::MirroredAlbum>>,
    }

    impl QueueStore for MockStore {
        fn uploaded_asset_id(&self, hash: &str) -> anyhow::Result<Option<String>> {
            Ok(self.uploaded.lock().unwrap().get(hash).cloned())
        }

        fn mirrored_album(
            &self,
            collection_id: &str,
        ) -> anyhow::Result<Option<crate::db::MirroredAlbum>> {
            Ok(self.mirrored.lock().unwrap().get(collection_id).cloned())
        }

        fn record_mirrored_album(
            &self,
            collection_id: &str,
            immich_album_id: &str,
            name: &str,
        ) -> anyhow::Result<()> {
            self.mirrored.lock().unwrap().insert(
                collection_id.to_string(),
                crate::db::MirroredAlbum {
                    immich_album_id: immich_album_id.to_string(),
                    name: name.to_string(),
                },
            );
            Ok(())
        }

        fn forget_mirrored_album(&self, collection_id: &str) -> anyhow::Result<()> {
            self.mirrored.lock().unwrap().remove(collection_id);
            Ok(())
        }

        fn enqueue_photos(&self, local_id: &str) -> anyhow::Result<i64> {
            let mut id_guard = self.next_id.lock().unwrap();
            *id_guard += 1;
            let id = *id_guard;
            self.entries.lock().unwrap().push(QueueEntry {
                id,
                file_path: format!("photos:{local_id}"),
                file_hash: None,
                file_size: None,
                folder_id: None,
                status: "pending".to_string(),
                retry_count: 0,
                error_message: None,
                queued_at: Utc::now(),
                completed_at: None,
                source: crate::upload::queue::QueueSource::Photos,
                photos_local_id: Some(local_id.to_string()),
            });
            Ok(id)
        }

        fn record_photos_asset(
            &self,
            local_id: &str,
            resource: &str,
            _sha1: &str,
            immich_asset_id: &str,
            _modified_at: Option<&str>,
        ) -> anyhow::Result<()> {
            self.photos_assets.lock().unwrap().push((
                local_id.to_string(),
                resource.to_string(),
                immich_asset_id.to_string(),
            ));
            Ok(())
        }

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
                source: crate::upload::queue::QueueSource::Folder,
                photos_local_id: None,
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
            _mtime: i64,
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
                .map(|i| BulkCheckResult {
                    id: i.id,
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
                .map(|i| BulkCheckResult {
                    id: i.id,
                    exists: false,
                    asset_id: None,
                })
                .collect())
        }
    }

    /// Records what was uploaded and which album it went to.
    #[derive(Default)]
    struct RecordingUploader {
        uploads: Mutex<Vec<(String, String)>>,
        albums: Mutex<Vec<(String, String)>>,
        /// `(still file name, livePhotoVideoId)` for each linked still.
        live_links: Mutex<Vec<(String, String)>>,
        /// File names whose upload fails.
        fail: Vec<&'static str>,
    }

    #[async_trait]
    impl AssetUploader for RecordingUploader {
        async fn upload_asset(
            &self,
            file_path: &Path,
            device_asset_id: &str,
            _device_id: &str,
            _created_at: DateTime<Utc>,
            _modified_at: DateTime<Utc>,
        ) -> anyhow::Result<UploadResult> {
            assert!(file_path.exists(), "uploaded file must exist during upload");
            let name = file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if self.fail.contains(&name.as_str()) {
                anyhow::bail!("simulated failure uploading {name}");
            }
            let mut uploads = self.uploads.lock().unwrap();
            uploads.push((name, device_asset_id.to_string()));
            Ok(UploadResult {
                asset_id: format!("imm-{}", uploads.len()),
                is_duplicate: false,
            })
        }

        async fn upload_live_still(
            &self,
            file_path: &Path,
            device_asset_id: &str,
            device_id: &str,
            created_at: DateTime<Utc>,
            modified_at: DateTime<Utc>,
            live_photo_video_id: &str,
        ) -> anyhow::Result<UploadResult> {
            let name = file_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            self.live_links
                .lock()
                .unwrap()
                .push((name, live_photo_video_id.to_string()));
            self.upload_asset(
                file_path,
                device_asset_id,
                device_id,
                created_at,
                modified_at,
            )
            .await
        }

        async fn check_duplicates(
            &self,
            _items: Vec<BulkCheckItem>,
        ) -> anyhow::Result<Vec<BulkCheckResult>> {
            Ok(Vec::new())
        }

        async fn add_asset_to_album(&self, album: &str, asset_id: &str) -> anyhow::Result<()> {
            self.albums
                .lock()
                .unwrap()
                .push((album.to_string(), asset_id.to_string()));
            Ok(())
        }
    }

    /// Exports a fixed outcome, writing a real temp file on success.
    struct FakeExporter {
        dir: std::path::PathBuf,
        outcome: fn() -> Option<LibraryExportError>,
        exported: Mutex<Vec<std::path::PathBuf>>,
        albums: fn() -> Vec<LibraryAlbum>,
        /// Export a Live Photo: a still plus `IMG_0001.MOV`.
        paired: bool,
    }

    impl LibraryExporter for FakeExporter {
        fn export_original(&self, local_id: &str) -> Result<LibraryExport, LibraryExportError> {
            if let Some(e) = (self.outcome)() {
                return Err(e);
            }
            let sub = self.dir.join(local_id.replace('/', "_"));
            std::fs::create_dir_all(&sub).unwrap();
            let path = sub.join("IMG_0001.HEIC");
            std::fs::write(&path, format!("original bytes of {local_id}")).unwrap();
            self.exported.lock().unwrap().push(path.clone());
            let paired_video = self.paired.then(|| {
                let v = sub.join("IMG_0001.MOV");
                std::fs::write(&v, format!("motion bytes of {local_id}")).unwrap();
                self.exported.lock().unwrap().push(v.clone());
                v
            });
            Ok(LibraryExport {
                path,
                resource: "photo",
                created_at: Utc::now(),
                modified_at: Utc::now(),
                albums: (self.albums)(),
                paired_video,
            })
        }
    }

    fn library_config(exporter: Arc<FakeExporter>) -> WorkerConfig {
        WorkerConfig {
            concurrency: 1,
            max_retries: 3,
            base_retry_delay: Duration::from_millis(1),
            device_id: "immichsync-test".to_string(),
            library: Some(Arc::new(LibrarySource::new(
                exporter,
                AlbumPolicy {
                    mirror: false,
                    all_in: Some("immichsync test".to_string()),
                },
            ))),
        }
    }

    async fn run_one_photos_entry(
        store: &Arc<MockStore>,
        uploader: Arc<dyn AssetUploader>,
        config: &WorkerConfig,
        local_id: &str,
    ) -> QueueEntry {
        let id = store.enqueue_photos(local_id).unwrap();
        let entry = store.dequeue_pending(1).unwrap().pop().unwrap();
        process_item(entry, store.clone(), uploader, config).await;
        let entries = store.entries.lock().unwrap();
        entries.iter().find(|e| e.id == id).unwrap().clone()
    }

    /// A fake Immich album API: records creates, adds and renames; albums
    /// in `gone` answer as deleted; `create_delay` widens the race window.
    #[derive(Default)]
    struct AlbumServer {
        created: Mutex<Vec<String>>,
        added: Mutex<Vec<(String, String)>>,
        renamed: Mutex<Vec<(String, String)>>,
        by_name: Mutex<Vec<(String, String)>>,
        gone: Mutex<std::collections::HashSet<String>>,
        create_delay: Duration,
        /// Catch-all albums made by name (get-or-create, like the client).
        by_name_created: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl AssetUploader for AlbumServer {
        async fn upload_asset(
            &self,
            _file_path: &Path,
            _device_asset_id: &str,
            _device_id: &str,
            _created_at: DateTime<Utc>,
            _modified_at: DateTime<Utc>,
        ) -> anyhow::Result<UploadResult> {
            Ok(UploadResult {
                asset_id: "imm-1".to_string(),
                is_duplicate: false,
            })
        }

        async fn check_duplicates(
            &self,
            _items: Vec<BulkCheckItem>,
        ) -> anyhow::Result<Vec<BulkCheckResult>> {
            Ok(Vec::new())
        }

        async fn add_asset_to_album(&self, album: &str, asset_id: &str) -> anyhow::Result<()> {
            // Get-or-create by name, with the same check-then-create gap the
            // real client has.
            let exists = self
                .by_name_created
                .lock()
                .unwrap()
                .iter()
                .any(|n| n == album);
            if !exists {
                tokio::time::sleep(self.create_delay).await;
                self.by_name_created.lock().unwrap().push(album.to_string());
            }
            self.by_name
                .lock()
                .unwrap()
                .push((album.to_string(), asset_id.to_string()));
            Ok(())
        }

        async fn create_mirror_album(&self, name: &str) -> anyhow::Result<String> {
            tokio::time::sleep(self.create_delay).await;
            let mut created = self.created.lock().unwrap();
            created.push(name.to_string());
            Ok(format!("alb-{}", created.len()))
        }

        async fn add_to_album_by_id(
            &self,
            album_id: &str,
            asset_id: &str,
        ) -> Result<(), AlbumCallError> {
            if self.gone.lock().unwrap().contains(album_id) {
                return Err(AlbumCallError::Gone);
            }
            self.added
                .lock()
                .unwrap()
                .push((album_id.to_string(), asset_id.to_string()));
            Ok(())
        }

        async fn rename_mirror_album(
            &self,
            album_id: &str,
            name: &str,
        ) -> Result<(), AlbumCallError> {
            if self.gone.lock().unwrap().contains(album_id) {
                return Err(AlbumCallError::Gone);
            }
            self.renamed
                .lock()
                .unwrap()
                .push((album_id.to_string(), name.to_string()));
            Ok(())
        }
    }

    struct NoExport;
    impl LibraryExporter for NoExport {
        fn export_original(&self, _: &str) -> Result<LibraryExport, LibraryExportError> {
            Err(LibraryExportError::Gone)
        }
    }

    fn source(mirror: bool, all_in: Option<&str>) -> LibrarySource {
        LibrarySource::new(
            Arc::new(NoExport),
            AlbumPolicy {
                mirror,
                all_in: all_in.map(str::to_string),
            },
        )
    }

    fn album(id: &str, title: &str) -> LibraryAlbum {
        LibraryAlbum {
            local_id: id.to_string(),
            title: title.to_string(),
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────

    fn live_exporter(dir: &Path) -> Arc<FakeExporter> {
        Arc::new(FakeExporter {
            dir: dir.to_path_buf(),
            outcome: || None,
            exported: Mutex::new(Vec::new()),
            albums: || vec![album("C1", "Hawaii")],
            paired: true,
        })
    }

    #[tokio::test]
    async fn a_live_photo_uploads_its_video_first_then_the_linked_still() {
        let dir = tempfile::tempdir().unwrap();
        let exporter = live_exporter(dir.path());
        let store = Arc::new(MockStore::default());
        let uploader = Arc::new(RecordingUploader::default());
        let config = library_config(exporter.clone());

        let done = run_one_photos_entry(&store, uploader.clone(), &config, "LIVE").await;

        assert_eq!(done.status, "completed", "{:?}", done.error_message);
        assert_eq!(
            *uploader.uploads.lock().unwrap(),
            vec![
                (
                    "IMG_0001.MOV".to_string(),
                    "photos-LIVE-paired_video".to_string()
                ),
                ("IMG_0001.HEIC".to_string(), "photos-LIVE-photo".to_string()),
            ],
            "video first, then the still"
        );
        assert_eq!(
            *uploader.live_links.lock().unwrap(),
            vec![("IMG_0001.HEIC".to_string(), "imm-1".to_string())],
            "the still links to the video's Immich id"
        );
        // Only the still goes into albums (Immich hides the motion part).
        assert_eq!(
            *uploader.albums.lock().unwrap(),
            vec![("immichsync test".to_string(), "imm-2".to_string())]
        );
        let rows = store.photos_assets.lock().unwrap().clone();
        assert!(rows.contains(&("LIVE".into(), "paired_video".into(), "imm-1".into())));
        assert!(rows.contains(&("LIVE".into(), "photo".into(), "imm-2".into())));
        for p in exporter.exported.lock().unwrap().iter() {
            assert!(!p.exists(), "{} must be deleted", p.display());
        }
    }

    /// The still is never marked done before its video is.
    #[tokio::test]
    async fn a_failed_video_upload_leaves_the_still_unuploaded() {
        let dir = tempfile::tempdir().unwrap();
        let exporter = live_exporter(dir.path());
        let store = Arc::new(MockStore::default());
        let uploader = Arc::new(RecordingUploader {
            fail: vec!["IMG_0001.MOV"],
            ..RecordingUploader::default()
        });

        let done = run_one_photos_entry(
            &store,
            uploader.clone(),
            &library_config(exporter.clone()),
            "LIVE",
        )
        .await;

        assert_ne!(done.status, "completed");
        assert!(
            uploader.uploads.lock().unwrap().is_empty(),
            "no still without its video"
        );
        for p in exporter.exported.lock().unwrap().iter() {
            assert!(!p.exists(), "{} must be deleted", p.display());
        }
    }

    /// If the still is already in Immich (same SHA-1, e.g. uploaded from a
    /// watched folder), its motion video must not go up on its own: it would
    /// sit in the timeline as an unlinked, visible video (Pia's review on
    /// #138). Nothing is uploaded; the existing still joins its albums.
    #[tokio::test]
    async fn a_live_photo_whose_still_is_already_uploaded_uploads_no_video() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MockStore::default());
        let probe = dir.path().join("probe");
        std::fs::write(&probe, "original bytes of LIVE").unwrap();
        let still_hash = crate::upload::hasher::hash_file(&probe).unwrap();
        store
            .uploaded
            .lock()
            .unwrap()
            .insert(still_hash, "imm-still".to_string());
        let uploader = Arc::new(RecordingUploader::default());
        let exporter = live_exporter(dir.path());

        let done = run_one_photos_entry(
            &store,
            uploader.clone(),
            &library_config(exporter.clone()),
            "LIVE",
        )
        .await;

        assert_eq!(done.status, "completed");
        assert!(
            uploader.uploads.lock().unwrap().is_empty(),
            "no unlinked video"
        );
        assert_eq!(
            *uploader.albums.lock().unwrap(),
            vec![("immichsync test".to_string(), "imm-still".to_string())]
        );
        for p in exporter.exported.lock().unwrap().iter() {
            assert!(!p.exists(), "{} must be deleted", p.display());
        }
    }

    /// A retry after the still failed doesn't upload the video again: the
    /// still links to the video already on the server.
    #[tokio::test]
    async fn a_retry_reuses_the_uploaded_video() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MockStore::default());
        // The first attempt's video upload is on record.
        let probe = dir.path().join("probe");
        std::fs::write(&probe, "motion bytes of LIVE").unwrap();
        let vhash = crate::upload::hasher::hash_file(&probe).unwrap();
        store
            .uploaded
            .lock()
            .unwrap()
            .insert(vhash, "imm-video".to_string());
        let uploader = Arc::new(RecordingUploader::default());

        let done = run_one_photos_entry(
            &store,
            uploader.clone(),
            &library_config(live_exporter(dir.path())),
            "LIVE",
        )
        .await;

        assert_eq!(done.status, "completed");
        assert_eq!(
            *uploader.uploads.lock().unwrap(),
            vec![("IMG_0001.HEIC".to_string(), "photos-LIVE-photo".to_string())]
        );
        assert_eq!(
            *uploader.live_links.lock().unwrap(),
            vec![("IMG_0001.HEIC".to_string(), "imm-video".to_string())]
        );
    }

    #[tokio::test]
    async fn a_photo_in_two_albums_lands_in_both_mirrors_created_once() {
        let store = MockStore::default();
        let server = AlbumServer::default();
        let lib = source(true, None);
        let two = [album("C1", "Hawaii"), album("C2", "Trips / Beach")];

        place_in_albums("imm-1", &two, &lib, &store, &server).await;
        // A second photo in one of the same albums reuses its mirror.
        place_in_albums("imm-2", &two[..1], &lib, &store, &server).await;

        assert_eq!(
            *server.created.lock().unwrap(),
            vec!["Hawaii", "Trips / Beach"]
        );
        assert_eq!(
            *server.added.lock().unwrap(),
            vec![
                ("alb-1".into(), "imm-1".into()),
                ("alb-2".into(), "imm-1".into()),
                ("alb-1".into(), "imm-2".into()),
            ]
        );
        assert!(
            server.by_name.lock().unwrap().is_empty(),
            "no catch-all configured"
        );
        assert_eq!(
            store.mirrored.lock().unwrap()["C2"].immich_album_id,
            "alb-2"
        );
    }

    #[tokio::test]
    async fn a_renamed_photos_album_renames_its_mirror_instead_of_making_another() {
        let store = MockStore::default();
        store
            .record_mirrored_album("C1", "alb-9", "Hawaii")
            .unwrap();
        let server = AlbumServer::default();

        place_in_albums(
            "imm-1",
            &[album("C1", "Hawaii 2026")],
            &source(true, None),
            &store,
            &server,
        )
        .await;

        assert!(server.created.lock().unwrap().is_empty());
        assert_eq!(
            *server.renamed.lock().unwrap(),
            vec![("alb-9".into(), "Hawaii 2026".into())]
        );
        assert_eq!(
            *server.added.lock().unwrap(),
            vec![("alb-9".into(), "imm-1".into())]
        );
        assert_eq!(store.mirrored.lock().unwrap()["C1"].name, "Hawaii 2026");
    }

    #[tokio::test]
    async fn a_mirror_deleted_on_the_server_is_recreated() {
        let store = MockStore::default();
        store
            .record_mirrored_album("C1", "alb-old", "Hawaii")
            .unwrap();
        let server = AlbumServer::default();
        server.gone.lock().unwrap().insert("alb-old".into());

        place_in_albums(
            "imm-1",
            &[album("C1", "Hawaii")],
            &source(true, None),
            &store,
            &server,
        )
        .await;

        assert_eq!(*server.created.lock().unwrap(), vec!["Hawaii"]);
        assert_eq!(
            *server.added.lock().unwrap(),
            vec![("alb-1".into(), "imm-1".into())]
        );
        assert_eq!(
            store.mirrored.lock().unwrap()["C1"].immich_album_id,
            "alb-1"
        );
    }

    #[tokio::test]
    async fn the_catch_all_combines_with_mirroring_and_none_means_no_album_calls() {
        let store = MockStore::default();
        let server = AlbumServer::default();
        place_in_albums(
            "imm-1",
            &[album("C1", "Hawaii")],
            &source(true, Some("Everything")),
            &store,
            &server,
        )
        .await;
        assert_eq!(server.added.lock().unwrap().len(), 1);
        assert_eq!(
            *server.by_name.lock().unwrap(),
            vec![("Everything".into(), "imm-1".into())]
        );

        let quiet = AlbumServer::default();
        place_in_albums(
            "imm-1",
            &[album("C1", "Hawaii")],
            &source(false, None),
            &MockStore::default(),
            &quiet,
        )
        .await;
        assert!(quiet.created.lock().unwrap().is_empty());
        assert!(quiet.added.lock().unwrap().is_empty());
        assert!(quiet.by_name.lock().unwrap().is_empty());
    }

    /// Two workers placing photos from the same new Photos album at once
    /// must create one Immich album, not two.
    #[tokio::test]
    async fn concurrent_placements_create_a_new_album_once() {
        let store = Arc::new(MockStore::default());
        let server = Arc::new(AlbumServer {
            create_delay: Duration::from_millis(50),
            ..AlbumServer::default()
        });
        let lib = Arc::new(source(true, None));
        let run = |asset: &'static str| {
            let (store, server, lib) = (store.clone(), server.clone(), lib.clone());
            tokio::spawn(async move {
                place_in_albums(
                    asset,
                    &[album("C1", "Hawaii")],
                    &lib,
                    store.as_ref(),
                    server.as_ref(),
                )
                .await
            })
        };
        let (a, b) = (run("imm-1"), run("imm-2"));
        a.await.unwrap();
        b.await.unwrap();
        assert_eq!(server.created.lock().unwrap().len(), 1);
        assert_eq!(server.added.lock().unwrap().len(), 2);
    }

    /// The catch-all album is found-or-created by name; two workers placing
    /// at once must not create it twice (Pia's review on #132).
    #[tokio::test]
    async fn concurrent_placements_create_the_catch_all_once() {
        let store = Arc::new(MockStore::default());
        let server = Arc::new(AlbumServer {
            create_delay: Duration::from_millis(50),
            ..AlbumServer::default()
        });
        let lib = Arc::new(source(false, Some("Everything")));
        let run = |asset: &'static str| {
            let (store, server, lib) = (store.clone(), server.clone(), lib.clone());
            tokio::spawn(async move {
                place_in_albums(asset, &[], &lib, store.as_ref(), server.as_ref()).await
            })
        };
        let (a, b) = (run("imm-1"), run("imm-2"));
        a.await.unwrap();
        b.await.unwrap();
        assert_eq!(server.by_name_created.lock().unwrap().len(), 1);
        assert_eq!(server.by_name.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_hash_duplicate_still_joins_its_albums_with_the_existing_asset() {
        let dir = tempfile::tempdir().unwrap();
        let exporter = Arc::new(FakeExporter {
            dir: dir.path().to_path_buf(),
            outcome: || None,
            exported: Mutex::new(Vec::new()),
            albums: || vec![album("C1", "Hawaii")],
            paired: false,
        });
        // Same bytes as the export, already uploaded from a watched folder.
        let store = Arc::new(MockStore::default());
        let probe = dir.path().join("probe");
        std::fs::write(&probe, "original bytes of DUP2").unwrap();
        let hash = crate::upload::hasher::hash_file(&probe).unwrap();
        store
            .uploaded
            .lock()
            .unwrap()
            .insert(hash, "folder-asset".to_string());
        let server = Arc::new(AlbumServer::default());
        let mut config = library_config(exporter.clone());
        config.library = Some(Arc::new(LibrarySource::new(
            exporter,
            AlbumPolicy {
                mirror: true,
                all_in: None,
            },
        )));

        let done = run_one_photos_entry(&store, server.clone(), &config, "DUP2").await;

        assert_eq!(done.status, "completed");
        assert_eq!(
            *server.added.lock().unwrap(),
            vec![("alb-1".into(), "folder-asset".into())]
        );
    }

    fn folder_entry(path: &std::path::Path, hash: Option<String>, size: Option<u64>) -> QueueEntry {
        QueueEntry {
            id: 1,
            file_path: path.to_string_lossy().into_owned(),
            file_hash: hash,
            file_size: size,
            folder_id: Some(1),
            status: "uploading".to_string(),
            retry_count: 0,
            error_message: None,
            queued_at: Utc::now(),
            completed_at: None,
            source: crate::upload::queue::QueueSource::Folder,
            photos_local_id: None,
        }
    }

    /// #102: the post-upload trash/delete only acts on the exact file that
    /// was hashed: a same-size replacement, or an unhashed entry, is kept.
    #[test]
    fn post_upload_action_requires_the_uploaded_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("IMG_0001.JPG");
        std::fs::write(&f, b"first photo").unwrap();
        let hash = crate::upload::hasher::hash_file(&f).unwrap();
        let entry = folder_entry(&f, Some(hash), Some(11));
        assert!(file_is_still_the_upload(&f, &entry));

        std::fs::write(&f, b"other photo").unwrap(); // same size, new bytes
        assert!(!file_is_still_the_upload(&f, &entry));

        assert!(!file_is_still_the_upload(
            &f,
            &folder_entry(&f, None, Some(11))
        ));
        std::fs::remove_file(&f).unwrap();
        assert!(
            !file_is_still_the_upload(&f, &entry),
            "a vanished file isn't touched"
        );
    }

    #[test]
    fn photos_device_asset_id_is_stable_per_resource() {
        assert_eq!(
            photos_device_asset_id("ABC-1/L0/001", "photo"),
            "photos-ABC-1/L0/001-photo"
        );
        assert_ne!(
            photos_device_asset_id("A", "photo"),
            photos_device_asset_id("A", "video")
        );
    }

    #[tokio::test]
    async fn photos_entry_exports_uploads_albums_and_deletes_the_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let exporter = Arc::new(FakeExporter {
            dir: dir.path().to_path_buf(),
            outcome: || None,
            exported: Mutex::new(Vec::new()),
            albums: Vec::new,
            paired: false,
        });
        let store = Arc::new(MockStore::default());
        let uploader = Arc::new(RecordingUploader::default());
        let config = library_config(exporter.clone());

        let done = run_one_photos_entry(&store, uploader.clone(), &config, "ABC/L0/001").await;

        assert_eq!(done.status, "completed", "{:?}", done.error_message);
        assert_eq!(
            *uploader.uploads.lock().unwrap(),
            vec![(
                "IMG_0001.HEIC".to_string(),
                "photos-ABC/L0/001-photo".to_string()
            )]
        );
        assert_eq!(
            *uploader.albums.lock().unwrap(),
            vec![("immichsync test".to_string(), "imm-1".to_string())]
        );
        assert_eq!(
            *store.photos_assets.lock().unwrap(),
            vec![(
                "ABC/L0/001".to_string(),
                "photo".to_string(),
                "imm-1".to_string()
            )]
        );
        let exported = exporter.exported.lock().unwrap();
        assert_eq!(exported.len(), 1);
        assert!(!exported[0].exists(), "temp export must be deleted");
        assert!(
            !exported[0].parent().unwrap().exists(),
            "per-export dir must be removed"
        );
    }

    #[tokio::test]
    async fn photos_entry_already_uploaded_by_hash_is_not_uploaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let exporter = Arc::new(FakeExporter {
            dir: dir.path().to_path_buf(),
            outcome: || None,
            exported: Mutex::new(Vec::new()),
            albums: Vec::new,
            paired: false,
        });
        let store = Arc::new(MockStore::default());
        // Same bytes the fake exporter writes, already uploaded from a folder.
        let probe = dir.path().join("probe");
        std::fs::write(&probe, "original bytes of DUP").unwrap();
        let hash = crate::upload::hasher::hash_file(&probe).unwrap();
        store
            .uploaded
            .lock()
            .unwrap()
            .insert(hash, "folder-asset".to_string());
        let uploader = Arc::new(RecordingUploader::default());

        let done =
            run_one_photos_entry(&store, uploader.clone(), &library_config(exporter), "DUP").await;

        assert_eq!(done.status, "completed");
        assert!(uploader.uploads.lock().unwrap().is_empty());
    }

    /// A failed upload backs off before retrying; the private export must
    /// not sit in temp for that wait (the retry exports again anyway).
    #[tokio::test]
    async fn a_failed_photos_upload_deletes_the_export_before_backing_off() {
        let dir = tempfile::tempdir().unwrap();
        let exporter = Arc::new(FakeExporter {
            dir: dir.path().to_path_buf(),
            outcome: || None,
            exported: Mutex::new(Vec::new()),
            albums: Vec::new,
            paired: false,
        });
        let store = Arc::new(MockStore::default());
        let mut config = library_config(exporter.clone());
        // backoff_delay works in whole seconds, so the base must be >= 1 s.
        config.base_retry_delay = Duration::from_secs(1);

        store.enqueue_photos("RETRY").unwrap();
        let entry = store.dequeue_pending(1).unwrap().pop().unwrap();
        let task = {
            let store = store.clone();
            tokio::spawn(async move {
                process_item(entry, store, Arc::new(FailingUploader), &config).await;
            })
        };

        tokio::time::sleep(Duration::from_millis(100)).await;
        let path = exporter.exported.lock().unwrap()[0].clone();
        assert!(!task.is_finished(), "still in the retry backoff");
        assert!(!path.exists(), "export must be gone during the backoff");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn icloud_only_original_is_deferred_not_uploaded_or_retried() {
        let dir = tempfile::tempdir().unwrap();
        let exporter = Arc::new(FakeExporter {
            dir: dir.path().to_path_buf(),
            outcome: || Some(LibraryExportError::CloudOnly),
            exported: Mutex::new(Vec::new()),
            albums: Vec::new,
            paired: false,
        });
        let store = Arc::new(MockStore::default());
        let uploader = Arc::new(RecordingUploader::default());

        let done =
            run_one_photos_entry(&store, uploader.clone(), &library_config(exporter), "CLOUD")
                .await;

        assert_eq!(done.status, "failed");
        assert_eq!(done.retry_count, 0, "a deferred entry is not retried");
        assert!(done.error_message.unwrap().starts_with("deferred:"));
        assert!(uploader.uploads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn photos_entry_without_a_library_source_fails_cleanly() {
        let store = Arc::new(MockStore::default());
        let uploader = Arc::new(RecordingUploader::default());
        let config = WorkerConfig {
            library: None,
            base_retry_delay: Duration::from_millis(1),
            ..WorkerConfig::default()
        };

        let done = run_one_photos_entry(&store, uploader.clone(), &config, "X").await;

        assert_eq!(done.status, "failed");
        assert!(uploader.uploads.lock().unwrap().is_empty());
    }

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
            library: None,
        };

        process_item(entry, store.clone(), uploader, &config).await;

        let stats = store.get_queue_stats().unwrap();
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.pending, 0);
        assert!(store.is_file_uploaded(&hash).unwrap());
    }

    /// #96: an entry queued before the filter learned about packages must
    /// not be uploaded from (or later trashed out of) an app library.
    #[tokio::test]
    async fn worker_skips_entries_inside_skipped_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join(".Trashes").join("501").join("IMG_0002.JPG");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, b"deleted on the card").unwrap();

        let store = Arc::new(MockStore::default());
        store
            .enqueue(NewQueueEntry {
                file_path: inside.display().to_string(),
                file_hash: None,
                file_size: Some(19),
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
            library: None,
        };

        process_item(entry, store.clone(), Arc::new(SucceedingUploader), &config).await;

        let stats = store.get_queue_stats().unwrap();
        assert_eq!(stats.completed, 0, "must not upload");
        assert_eq!(stats.failed, 1);
        assert!(inside.exists(), "must not move or delete the file");
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
            library: None,
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

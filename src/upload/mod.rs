//! Upload pipeline orchestrator.
//!
//! `UploadPipeline` is the single entry point for the rest of the
//! application. It wires together:
//!   - [`UploadQueue`]  — hash, dedup, enqueue
//!   - [`UploadWorker`] — async workers that drain the queue
//!
//! ## Lifecycle
//!
//! ```text
//! UploadPipeline::new(...)
//!   -> pipeline.start()           // spawns the worker loop
//!   -> pipeline.submit(path, ..)  // called by the watch engine
//!   -> pipeline.pause() / resume()
//!   -> pipeline.stop()            // graceful shutdown
//! ```

pub mod hasher;
pub mod metadata;
pub mod queue;
pub mod worker;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use thiserror::Error;
use tokio::sync::watch;
use tracing::{debug, info};

use crate::config::{ServerConfig, UploadConfig};
use queue::{QueueStats, QueueStore, UploadQueue};
use worker::{AssetUploader, UploadWorker, WorkerConfig};

// ─── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("Pipeline is not started; call start() first")]
    NotStarted,

    #[error("Queue error: {0}")]
    Queue(#[from] queue::QueueError),

    #[error("Worker already running")]
    AlreadyRunning,
}

// ─── UploadPipeline ───────────────────────────────────────────────────────────

/// Top-level upload pipeline.
///
/// Holds handles to the queue and worker, and exposes a clean public API
/// to the rest of the application.
pub struct UploadPipeline {
    queue: UploadQueue,
    worker: Arc<UploadWorker>,

    store: Arc<dyn QueueStore>,
    uploader: Arc<dyn AssetUploader>,

    /// `true` = paused.  Sent into the worker via a `watch` channel.
    _pause_tx: watch::Sender<bool>,

    /// `None` before `start()` is called, `Some(handle)` after.
    worker_handle: Option<tokio::task::JoinHandle<()>>,
}

impl UploadPipeline {
    /// Create a new pipeline.
    ///
    /// `store`   — the queue / upload-record backend (e.g. SQLite via `db::Db`)
    /// `server`  — Immich server connection settings (for building the uploader)
    /// `upload`  — upload tuning parameters
    /// `uploader` — the Immich HTTP client
    pub fn new(
        store: Arc<dyn QueueStore>,
        server: &ServerConfig,
        upload: &UploadConfig,
        uploader: Arc<dyn AssetUploader>,
    ) -> Self {
        let worker_config = WorkerConfig {
            concurrency: upload.concurrency as usize,
            max_retries: upload.max_retries,
            base_retry_delay: std::time::Duration::from_secs(1),
            device_id: server.device_id.clone(),
        };

        let (pause_tx, _pause_rx) = watch::channel(false);

        let queue = UploadQueue::new(store.clone(), upload.concurrency as usize);
        let worker = Arc::new(UploadWorker::new(worker_config));

        Self {
            queue,
            worker,
            store,
            uploader,
            _pause_tx: pause_tx,
            worker_handle: None,
        }
    }

    /// Spawn the background worker loop.
    ///
    /// Must be called before [`submit`] will result in any uploads.
    /// Calling `start()` a second time returns `Err(PipelineError::AlreadyRunning)`.
    pub fn start(&mut self) -> Result<(), PipelineError> {
        if self.worker_handle.is_some() {
            return Err(PipelineError::AlreadyRunning);
        }

        let worker = self.worker.clone();
        let store = self.store.clone();
        let uploader = self.uploader.clone();

        let handle = tokio::spawn(async move {
            worker.run(store, uploader).await;
        });

        self.worker_handle = Some(handle);
        info!("Upload pipeline started");
        Ok(())
    }

    /// Hash, dedup-check, and enqueue a file for upload.
    ///
    /// Returns `Ok(Some(id))` when a new queue entry was created, or
    /// `Ok(None)` when the file was skipped (already uploaded).
    pub fn submit(
        &self,
        path: PathBuf,
        folder_id: Option<i64>,
    ) -> Result<Option<i64>, PipelineError> {
        debug!(path = %path.display(), folder_id = ?folder_id, "Submitting file to upload pipeline");
        Ok(self.queue.process_file(path, folder_id)?)
    }

    /// Pause upload processing.
    ///
    /// In-flight uploads are allowed to complete; no new uploads are started
    /// until [`resume`] is called.
    pub fn pause(&self) {
        self.worker.pause();
        let _ = self._pause_tx.send(true);
    }

    /// Resume a paused upload pipeline.
    pub fn resume(&self) {
        self.worker.resume();
        let _ = self._pause_tx.send(false);
    }

    /// Return current queue statistics.
    pub fn stats(&self) -> Result<QueueStats> {
        self.queue.get_stats().map_err(anyhow::Error::from)
    }

    /// Signal the worker loop to stop and wait for it to finish.
    ///
    /// After calling `stop()`, the pipeline should not be used again.
    pub async fn stop(&mut self) {
        info!("Stopping upload pipeline");
        self.worker.stop();

        if let Some(handle) = self.worker_handle.take() {
            // Cap the wait at 3s. Windows Task Manager / WM_CLOSE allows
            // ~5s before escalating to TerminateProcess; staying under
            // that lets the process exit cleanly when the user clicks
            // "End task". In-flight uploads abandoned here will be reset
            // from "uploading" back to "pending" by reset_stale_uploading
            // on next startup, so nothing is lost — just retried.
            let timeout = std::time::Duration::from_secs(3);
            match tokio::time::timeout(timeout, handle).await {
                Ok(Ok(())) => {
                    info!("Upload worker stopped cleanly");
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "Upload worker task panicked");
                }
                Err(_) => {
                    tracing::warn!(
                        "Upload worker did not stop within {:?}; abandoning (in-flight uploads will resume on next launch)",
                        timeout
                    );
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
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use tokio::time::sleep;

    use queue::{NewQueueEntry, QueueEntry};
    use worker::{BulkCheckItem, BulkCheckResult, UploadResult};

    // ── Minimal in-memory store ───────────────────────────────────────────

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

    // ── Mock uploader ─────────────────────────────────────────────────────

    struct InstantUploader;

    #[async_trait]
    impl AssetUploader for InstantUploader {
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

    fn make_pipeline() -> (UploadPipeline, Arc<MockStore>) {
        let store = Arc::new(MockStore::default());
        let uploader: Arc<dyn AssetUploader> = Arc::new(InstantUploader);
        let server = ServerConfig::default();
        let upload = UploadConfig::default();
        let pipeline = UploadPipeline::new(store.clone(), &server, &upload, uploader);
        (pipeline, store)
    }

    #[test]
    fn submit_enqueues_new_file() {
        use std::io::Write;
        let (pipeline, store) = make_pipeline();
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"photo bytes").unwrap();
        tmp.flush().unwrap();

        let id = pipeline.submit(tmp.path().to_path_buf(), None).unwrap();
        assert!(id.is_some());

        let stats = pipeline.stats().unwrap();
        assert_eq!(stats.pending, 1);
        let _ = store; // keep store alive
    }

    #[test]
    fn submit_skips_already_uploaded() {
        use std::io::Write;
        let (pipeline, store) = make_pipeline();
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"photo bytes").unwrap();
        tmp.flush().unwrap();

        let hash = hasher::hash_file(tmp.path()).unwrap();
        store.uploaded.lock().unwrap().insert(hash, true);

        let id = pipeline.submit(tmp.path().to_path_buf(), None).unwrap();
        assert!(id.is_none());
    }

    #[test]
    fn double_start_returns_error() {
        let (mut pipeline, _store) = make_pipeline();

        // Build a minimal tokio runtime just for the spawn.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            pipeline.start().unwrap();
            let result = pipeline.start();
            assert!(matches!(result, Err(PipelineError::AlreadyRunning)));
            pipeline.stop().await;
        });
    }

    #[tokio::test]
    async fn pipeline_start_and_stop() {
        let (mut pipeline, _store) = make_pipeline();
        pipeline.start().unwrap();
        // Let the worker tick at least once.
        sleep(Duration::from_millis(50)).await;
        pipeline.stop().await;
    }
}

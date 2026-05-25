// Folder watcher (notify-based)

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::watch::filter::FileFilter;

/// Events emitted by any watcher and consumed by the upload pipeline.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// File has been confirmed fully written and is ready for upload.
    FileReady(PathBuf),
    /// A non-fatal error occurred inside the watcher.
    Error(String),
}

/// Debounce window — filesystem events are coalesced for this long before
/// being dispatched to the write-completion checker.
const DEBOUNCE_WINDOW: Duration = Duration::from_secs(2);

/// How long we wait between the two size-stability samples when checking
/// whether a file write is complete.
const STABILITY_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Maximum time we will wait for a file to become stable before giving up.
const MAX_WRITE_WAIT: Duration = Duration::from_secs(30);

/// A local-folder watcher that uses the platform's native filesystem
/// notification API (via `notify`) with a debounce layer on top.
///
/// The watcher monitors a single directory tree recursively.  For each
/// create / modify event that passes the [`FileFilter`] it runs a write-
/// completion check before emitting a [`WatchEvent::FileReady`] on the
/// provided channel.
pub struct FolderWatcher {
    path: PathBuf,
    filter: Arc<FileFilter>,
    event_tx: mpsc::Sender<WatchEvent>,
    /// Tokio runtime handle — used to spawn async tasks from the notify callback thread.
    rt_handle: tokio::runtime::Handle,
    /// The debouncer handle — kept alive so the watcher keeps running.
    /// `None` before `start()` or after `stop()`.
    debouncer: Arc<Mutex<Option<Debouncer<notify::RecommendedWatcher, RecommendedCache>>>>,
}

impl FolderWatcher {
    /// Create a new `FolderWatcher`.
    ///
    /// # Parameters
    /// - `path`     — directory to watch (recursively)
    /// - `filter`   — file filter applied to every event
    /// - `event_tx` — channel on which ready files are reported
    pub fn new(
        path: PathBuf,
        filter: FileFilter,
        event_tx: mpsc::Sender<WatchEvent>,
        rt_handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            path,
            filter: Arc::new(filter),
            event_tx,
            rt_handle,
            debouncer: Arc::new(Mutex::new(None)),
        }
    }

    /// Start watching the configured directory.
    ///
    /// This method creates the debouncer + underlying watcher and registers
    /// the path for recursive monitoring.  The call returns quickly; all
    /// event processing happens on a background thread managed by `notify`.
    pub fn start(&self) -> anyhow::Result<()> {
        let filter = Arc::clone(&self.filter);
        let tx = self.event_tx.clone();
        let watch_path = self.path.clone();
        let rt = self.rt_handle.clone();

        // The debouncer callback runs on a thread owned by notify — NOT a
        // tokio thread. We use the runtime handle to spawn async tasks from
        // the callback so the write-completion check runs on the tokio pool.
        let handler = move |result: DebounceEventResult| {
            match result {
                Ok(events) => {
                    for event in events {
                        let kind = &event.event.kind;
                        if !matches!(
                            kind,
                            notify::EventKind::Create(_) | notify::EventKind::Modify(_)
                        ) {
                            continue;
                        }

                        for path in event.event.paths.clone() {
                            // Only consider regular files
                            if !path.is_file() {
                                continue;
                            }

                            if !filter.should_include(&path) {
                                debug!("Filtered out: {:?}", path);
                                continue;
                            }

                            let tx2 = tx.clone();
                            rt.spawn(async move {
                                write_complete_and_send(path, tx2).await;
                            });
                        }
                    }
                }
                Err(errors) => {
                    for e in errors {
                        error!("Watch error: {}", e);
                        // Best-effort: send error event; ignore channel closed.
                        let _ = tx.try_send(WatchEvent::Error(e.to_string()));
                    }
                }
            }
        };

        let mut debouncer = new_debouncer(DEBOUNCE_WINDOW, None, handler)
            .map_err(|e| anyhow::anyhow!("Failed to create debouncer: {}", e))?;

        debouncer
            .watch(&watch_path, RecursiveMode::Recursive)
            .map_err(|e| anyhow::anyhow!("Failed to watch {:?}: {}", watch_path, e))?;

        info!("Folder watcher started: {:?}", watch_path);

        *self.debouncer.lock().unwrap() = Some(debouncer);
        Ok(())
    }

    /// Stop watching.
    ///
    /// Drops the debouncer (and the underlying watcher), which unregisters
    /// the path from the OS notification subsystem.
    pub fn stop(&self) {
        if let Ok(mut guard) = self.debouncer.lock() {
            if guard.take().is_some() {
                info!("Folder watcher stopped: {:?}", self.path);
            }
        }
    }
}

/// Perform write-completion detection and, if successful, send
/// [`WatchEvent::FileReady`] on `tx`.
///
/// Detection algorithm:
/// 1. Sample the file size.
/// 2. Wait [`STABILITY_POLL_INTERVAL`] (500 ms).
/// 3. Sample the file size again.  If they differ the file is still being
///    written — wait and retry with exponential backoff.
/// 4. Attempt a non-exclusive read open.  If it fails (locked) the file is
///    still being held by the writer — wait and retry.
/// 5. If stable and readable → emit [`WatchEvent::FileReady`].
/// 6. If we exceed [`MAX_WRITE_WAIT`] without stabilising → emit
///    [`WatchEvent::Error`] describing the timeout.
pub(crate) async fn write_complete_and_send(path: PathBuf, tx: mpsc::Sender<WatchEvent>) {
    let start = tokio::time::Instant::now();
    let mut backoff = STABILITY_POLL_INTERVAL;

    loop {
        if start.elapsed() >= MAX_WRITE_WAIT {
            warn!(
                "Write-completion timeout for {:?} after {:?}",
                path, MAX_WRITE_WAIT
            );
            let _ = tx
                .send(WatchEvent::Error(format!(
                    "Write-completion timeout: {:?}",
                    path
                )))
                .await;
            return;
        }

        // Size-stability check — first sample
        let size1 = match tokio::fs::metadata(&path).await {
            Ok(m) => m.len(),
            Err(e) => {
                // File may have been deleted between the event and now.
                debug!("Cannot stat {:?}: {}; skipping", path, e);
                return;
            }
        };

        tokio::time::sleep(STABILITY_POLL_INTERVAL).await;

        // Size-stability check — second sample
        let size2 = match tokio::fs::metadata(&path).await {
            Ok(m) => m.len(),
            Err(e) => {
                debug!("Cannot stat {:?} on second check: {}; skipping", path, e);
                return;
            }
        };

        if size1 != size2 {
            debug!(
                "File {:?} still growing ({} -> {}); backing off {:?}",
                path, size1, size2, backoff
            );
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
            continue;
        }

        // Non-exclusive read open
        match tokio::fs::File::open(&path).await {
            Ok(_) => {
                // File is stable and readable.
                debug!("File ready for upload: {:?}", path);
                if let Err(e) = tx.send(WatchEvent::FileReady(path.clone())).await {
                    warn!("Event channel closed while sending FileReady: {}", e);
                }
                return;
            }
            Err(e) => {
                debug!(
                    "File {:?} not yet readable ({}); backing off {:?}",
                    path, e, backoff
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn test_write_complete_sends_file_ready() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test_image.jpg");
        // Write enough content to exceed the 1 KB min-size filter
        let mut f = std::fs::File::create(&file_path).unwrap();
        f.write_all(&vec![0xFFu8; 4096]).unwrap();
        drop(f);

        let (tx, mut rx) = mpsc::channel(8);
        write_complete_and_send(file_path.clone(), tx).await;

        let event = rx.try_recv().expect("Expected an event");
        match event {
            WatchEvent::FileReady(p) => assert_eq!(p, file_path),
            other => panic!("Expected FileReady, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_write_complete_missing_file() {
        let (tx, mut rx) = mpsc::channel(8);
        let missing = PathBuf::from("/nonexistent/path/image.jpg");
        // Should return without sending anything because metadata() will fail
        write_complete_and_send(missing, tx).await;
        assert!(rx.try_recv().is_err(), "No event expected for missing file");
    }
}

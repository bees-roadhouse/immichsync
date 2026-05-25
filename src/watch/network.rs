// Network share watcher with fallback

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{PollWatcher, RecursiveMode, Watcher};
use notify_debouncer_full::{
    new_debouncer, new_debouncer_opt, DebounceEventResult, Debouncer, RecommendedCache,
};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::watch::filter::FileFilter;
use crate::watch::folder::{write_complete_and_send, WatchEvent};

/// How long to wait for the health-check file event before falling back to polling.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Debounce window used for both the native and poll watchers.
const DEBOUNCE_WINDOW: Duration = Duration::from_secs(2);

/// Name of the temporary file created during the native-watcher health check.
const HEALTH_CHECK_FILENAME: &str = ".immichsync_watchcheck";

/// Which underlying watch strategy is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    Native,
    Poll,
}

/// Type-erases the two possible debouncer variants behind a single trait so
/// `NetworkWatcher` does not need to be generic.
trait AnyDebouncer: Send {
    fn stop(&mut self);
}

// The inner debouncer is held purely for its Drop impl (unregisters the OS
// watcher). Nothing reads the field directly.
#[allow(dead_code)]
struct NativeDebouncer(Debouncer<notify::RecommendedWatcher, RecommendedCache>);
#[allow(dead_code)]
struct PollDebouncer(Debouncer<PollWatcher, RecommendedCache>);

impl AnyDebouncer for NativeDebouncer {
    fn stop(&mut self) {
        // Debouncer stops when dropped; nothing explicit needed here.
    }
}

impl AnyDebouncer for PollDebouncer {
    fn stop(&mut self) {}
}

/// Watcher for network shares (UNC paths, mapped drives, NFS mounts, etc.).
///
/// Starts with the platform's native watcher (`RecommendedWatcher`, which on
/// Windows is `ReadDirectoryChangesW`).  Before committing to native mode it
/// performs a health check:
///
/// 1. Start the native watcher.
/// 2. Create a temporary file inside the watched directory.
/// 3. Wait up to 5 seconds for the watcher to emit an event for that file.
/// 4. If the event arrives → stay in native mode.
/// 5. If the event does *not* arrive → switch to `PollWatcher` with the
///    configured poll interval.
///
/// This transparently handles filesystems where `ReadDirectoryChangesW` fails
/// silently (NFS, some exotic SMB configurations).
pub struct NetworkWatcher {
    path: PathBuf,
    filter: Arc<FileFilter>,
    event_tx: mpsc::Sender<WatchEvent>,
    poll_interval: Duration,
    /// Tokio runtime handle — used to spawn async tasks from the notify callback thread.
    rt_handle: tokio::runtime::Handle,
    /// The live debouncer; `None` when stopped.
    debouncer: Arc<Mutex<Option<Box<dyn AnyDebouncer>>>>,
}

impl NetworkWatcher {
    /// Create a new `NetworkWatcher`.
    ///
    /// # Parameters
    /// - `path`          — network share path to watch (recursively)
    /// - `filter`        — file filter applied to every event
    /// - `event_tx`      — channel on which ready files are reported
    /// - `poll_interval` — polling interval used if native watching fails
    pub fn new(
        path: PathBuf,
        filter: FileFilter,
        event_tx: mpsc::Sender<WatchEvent>,
        poll_interval: Duration,
        rt_handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            path,
            filter: Arc::new(filter),
            event_tx,
            poll_interval,
            rt_handle,
            debouncer: Arc::new(Mutex::new(None)),
        }
    }

    /// Start watching the configured path.
    ///
    /// Performs the native/poll health check synchronously (blocking the
    /// caller for up to [`HEALTH_CHECK_TIMEOUT`]).  After the check, the
    /// chosen watcher runs on a background thread managed by `notify`.
    pub fn start(&self) -> anyhow::Result<()> {
        let mode = self.run_health_check()?;

        match mode {
            WatchMode::Native => {
                info!(
                    "Network watcher using native (ReadDirectoryChangesW) mode: {:?}",
                    self.path
                );
                self.start_native()?;
            }
            WatchMode::Poll => {
                info!(
                    "Network watcher using poll mode (interval={:?}): {:?}",
                    self.poll_interval, self.path
                );
                self.start_poll()?;
            }
        }

        Ok(())
    }

    /// Stop watching and release all resources.
    pub fn stop(&self) {
        let mut guard = self.debouncer.lock().unwrap();
        if let Some(mut d) = guard.take() {
            d.stop();
            info!("Network watcher stopped: {:?}", self.path);
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Determine whether the native watcher can reliably see events on this
    /// path by creating a temp file and waiting for the notification.
    fn run_health_check(&self) -> anyhow::Result<WatchMode> {
        use std::sync::mpsc as std_mpsc;

        let health_file = self.path.join(HEALTH_CHECK_FILENAME);
        let watch_path = self.path.clone();

        // A one-shot channel: once the health-check event fires we send a
        // signal; subsequent sends are ignored.
        let (probe_tx, probe_rx) = std_mpsc::channel::<()>();
        let probe_tx = Mutex::new(Some(probe_tx));

        let probe_handler = move |result: notify::Result<notify::Event>| {
            if let Ok(event) = result {
                if matches!(
                    event.kind,
                    notify::EventKind::Create(_) | notify::EventKind::Modify(_)
                ) {
                    let is_probe = event.paths.iter().any(|p| {
                        p.file_name()
                            .map(|n| n == HEALTH_CHECK_FILENAME)
                            .unwrap_or(false)
                    });
                    if is_probe {
                        if let Ok(mut guard) = probe_tx.lock() {
                            if let Some(tx) = guard.take() {
                                let _ = tx.send(());
                            }
                        }
                    }
                }
            }
        };

        let mut probe_watcher =
            notify::RecommendedWatcher::new(probe_handler, notify::Config::default())
                .map_err(|e| anyhow::anyhow!("Failed to create probe watcher: {}", e))?;

        probe_watcher
            .watch(&watch_path, RecursiveMode::NonRecursive)
            .map_err(|e| {
                anyhow::anyhow!("Failed to register probe path {:?}: {}", watch_path, e)
            })?;

        // Create the health-check file.
        if let Err(e) = std::fs::write(&health_file, b"immichsync") {
            warn!(
                "Could not create health-check file in {:?}: {}; assuming poll mode",
                self.path, e
            );
            return Ok(WatchMode::Poll);
        }

        // Wait for the event or timeout.
        let mode = match probe_rx.recv_timeout(HEALTH_CHECK_TIMEOUT) {
            Ok(_) => WatchMode::Native,
            Err(_) => {
                warn!(
                    "No filesystem event received within {:?} for {:?}; switching to poll mode",
                    HEALTH_CHECK_TIMEOUT, self.path
                );
                WatchMode::Poll
            }
        };

        // Clean up: remove probe file and the probe watcher.
        let _ = std::fs::remove_file(&health_file);
        drop(probe_watcher);

        Ok(mode)
    }

    fn start_native(&self) -> anyhow::Result<()> {
        let filter = Arc::clone(&self.filter);
        let tx = self.event_tx.clone();
        let watch_path = self.path.clone();

        let handler = build_handler(filter, tx, self.rt_handle.clone());

        let mut debouncer = new_debouncer(DEBOUNCE_WINDOW, None, handler)
            .map_err(|e| anyhow::anyhow!("Failed to create native debouncer: {}", e))?;

        debouncer
            .watch(&watch_path, RecursiveMode::Recursive)
            .map_err(|e| anyhow::anyhow!("Failed to watch {:?}: {}", watch_path, e))?;

        *self.debouncer.lock().unwrap() = Some(Box::new(NativeDebouncer(debouncer)));
        Ok(())
    }

    fn start_poll(&self) -> anyhow::Result<()> {
        let filter = Arc::clone(&self.filter);
        let tx = self.event_tx.clone();
        let watch_path = self.path.clone();
        let poll_interval = self.poll_interval;

        let handler = build_handler(filter, tx, self.rt_handle.clone());

        // `new_debouncer_opt` lets us supply custom notify::Config so we can
        // specify the poll interval, and infers T = PollWatcher from the
        // explicit type annotation.
        let poll_config = notify::Config::default().with_poll_interval(poll_interval);
        let mut debouncer: Debouncer<PollWatcher, RecommendedCache> = new_debouncer_opt(
            DEBOUNCE_WINDOW,
            None,
            handler,
            RecommendedCache::default(),
            poll_config,
        )
        .map_err(|e| anyhow::anyhow!("Failed to create poll debouncer: {}", e))?;

        debouncer
            .watch(&watch_path, RecursiveMode::Recursive)
            .map_err(|e| anyhow::anyhow!("Failed to watch {:?}: {}", watch_path, e))?;

        *self.debouncer.lock().unwrap() = Some(Box::new(PollDebouncer(debouncer)));
        Ok(())
    }
}

/// Build the shared debounce event handler for both native and poll modes.
///
/// The handler runs on a notify-owned thread, so we need the tokio runtime
/// handle to spawn async write-completion tasks.
fn build_handler(
    filter: Arc<FileFilter>,
    tx: mpsc::Sender<WatchEvent>,
    rt: tokio::runtime::Handle,
) -> impl FnMut(DebounceEventResult) + Send + 'static {
    move |result: DebounceEventResult| match result {
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
                    if !path.is_file() {
                        continue;
                    }

                    // Never forward the transient health-check file.
                    if path
                        .file_name()
                        .map(|n| n == HEALTH_CHECK_FILENAME)
                        .unwrap_or(false)
                    {
                        continue;
                    }

                    if !filter.should_include(&path) {
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
                error!("Network watch error: {}", e);
                let _ = tx.try_send(WatchEvent::Error(e.to_string()));
            }
        }
    }
}

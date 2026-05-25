// Watch engine orchestrator

pub mod device;
pub mod filter;
pub mod folder;
pub mod network;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::watch::filter::FileFilter;
use crate::watch::folder::{FolderWatcher, WatchEvent};
use crate::watch::network::NetworkWatcher;

/// Default poll interval for network watchers.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Channel buffer size for the unified event stream.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Errors that can be returned by the watch engine.
#[derive(Debug, Error)]
pub enum WatchError {
    #[error("Path is already being watched: {0:?}")]
    AlreadyWatching(PathBuf),

    #[error("Path is not being watched: {0:?}")]
    NotWatching(PathBuf),

    #[error("Failed to start watcher for {path:?}: {source}")]
    StartFailed {
        path: PathBuf,
        #[source]
        source: anyhow::Error,
    },
}

/// Internal representation of an active watcher.
///
/// Each variant holds the concrete watcher object so it can be stopped and
/// dropped when the path is removed.
enum ActiveWatcher {
    Folder(FolderWatcher),
    Network(NetworkWatcher),
}

impl ActiveWatcher {
    fn stop(&self) {
        match self {
            ActiveWatcher::Folder(w) => w.stop(),
            ActiveWatcher::Network(w) => w.stop(),
        }
    }
}

/// The watch engine orchestrates multiple folder and network watchers.
///
/// All watchers share a single outbound event channel.  Consumers receive a
/// unified stream of [`WatchEvent`] values without needing to know which
/// watcher produced them.
///
/// Typical usage:
/// 1. Create a `WatchEngine` with [`WatchEngine::new`].
/// 2. Take the event receiver with [`WatchEngine::event_receiver`].
/// 3. Add folders with [`WatchEngine::add_folder`].
/// 4. Drive the event loop by reading from the receiver.
pub struct WatchEngine {
    /// Active watchers keyed by the watched path.
    watchers: HashMap<PathBuf, ActiveWatcher>,
    /// Sending half of the unified event channel.
    event_tx: mpsc::Sender<WatchEvent>,
    /// Receiving half of the unified event channel.
    ///
    /// `Option` so we can hand it to the caller via [`event_receiver`] while
    /// keeping `event_tx` alive inside the engine.
    event_rx: Option<mpsc::Receiver<WatchEvent>>,
}

impl WatchEngine {
    /// Create a new, empty watch engine.
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            watchers: HashMap::new(),
            event_tx,
            event_rx: Some(event_rx),
        }
    }

    /// Take the unified event receiver.
    ///
    /// May only be called once; subsequent calls return `None`.
    pub fn event_receiver(&mut self) -> Option<mpsc::Receiver<WatchEvent>> {
        self.event_rx.take()
    }

    /// Add a folder to the watch list and start monitoring it.
    ///
    /// # Parameters
    /// - `path`       — the directory to watch (must exist)
    /// - `filter`     — file filter to apply to events from this folder
    /// - `is_network` — if `true`, a [`NetworkWatcher`] with health-check
    ///                  fallback is used; if `false`, a [`FolderWatcher`]
    ///                  (native, debounced) is used
    ///
    /// # Errors
    ///
    /// Returns [`WatchError::AlreadyWatching`] if the path is already
    /// registered.  Returns [`WatchError::StartFailed`] if the underlying
    /// watcher cannot be started (e.g. the path does not exist or is not
    /// accessible).
    pub fn add_folder(
        &mut self,
        path: PathBuf,
        filter: FileFilter,
        is_network: bool,
        rt_handle: tokio::runtime::Handle,
    ) -> Result<(), WatchError> {
        // Normalise the path (resolve `..`, trailing slashes, etc.)
        let path = match std::fs::canonicalize(&path) {
            Ok(p) => p,
            Err(_) => path, // best-effort; watcher will fail if the path is bad
        };

        if self.watchers.contains_key(&path) {
            return Err(WatchError::AlreadyWatching(path));
        }

        let tx = self.event_tx.clone();

        let watcher: ActiveWatcher = if is_network {
            let nw = NetworkWatcher::new(
                path.clone(),
                filter,
                tx,
                DEFAULT_POLL_INTERVAL,
                rt_handle,
            );
            nw.start().map_err(|e| WatchError::StartFailed {
                path: path.clone(),
                source: e,
            })?;
            ActiveWatcher::Network(nw)
        } else {
            let fw = FolderWatcher::new(path.clone(), filter, tx, rt_handle);
            fw.start().map_err(|e| WatchError::StartFailed {
                path: path.clone(),
                source: e,
            })?;
            ActiveWatcher::Folder(fw)
        };

        info!("Watch engine: added {:?} (network={})", path, is_network);
        self.watchers.insert(path, watcher);
        Ok(())
    }

    /// Remove a folder from the watch list and stop its watcher.
    ///
    /// # Errors
    ///
    /// Returns [`WatchError::NotWatching`] if the path is not currently
    /// registered.
    pub fn remove_folder(&mut self, path: &Path) -> Result<(), WatchError> {
        // Try the exact path first; if that fails, try the canonicalised form.
        let key = if self.watchers.contains_key(path) {
            path.to_path_buf()
        } else {
            match std::fs::canonicalize(path) {
                Ok(p) if self.watchers.contains_key(&p) => p,
                _ => return Err(WatchError::NotWatching(path.to_path_buf())),
            }
        };

        if let Some(watcher) = self.watchers.remove(&key) {
            watcher.stop();
            info!("Watch engine: removed {:?}", key);
            Ok(())
        } else {
            Err(WatchError::NotWatching(path.to_path_buf()))
        }
    }

    /// Stop all active watchers without removing them from the internal map.
    ///
    /// After calling this the engine is effectively idle.  Call
    /// [`add_folder`][Self::add_folder] to start watching again.
    pub fn stop_all(&mut self) {
        for (path, watcher) in &self.watchers {
            watcher.stop();
            info!("Watch engine: stopped {:?}", path);
        }
        self.watchers.clear();
    }

    /// Return the number of currently active watchers.
    pub fn watcher_count(&self) -> usize {
        self.watchers.len()
    }

    /// Return the set of canonicalized paths currently being watched.
    ///
    /// Used by the reconcile pass that compares the engine's view of watched
    /// folders against the DB's view, to detect drift introduced by the
    /// Settings subprocess mutating the DB without the main process getting
    /// a config-update message until the subprocess exits.
    pub fn watched_paths(&self) -> std::collections::HashSet<PathBuf> {
        self.watchers.keys().cloned().collect()
    }

    /// Return `true` if the given path is currently being watched.
    pub fn is_watching(&self, path: &Path) -> bool {
        if self.watchers.contains_key(path) {
            return true;
        }
        // Also check canonicalised form.
        if let Ok(p) = std::fs::canonicalize(path) {
            return self.watchers.contains_key(&p);
        }
        false
    }
}

impl Default for WatchEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WatchEngine {
    fn drop(&mut self) {
        if !self.watchers.is_empty() {
            warn!(
                "WatchEngine dropped with {} active watcher(s); stopping all",
                self.watchers.len()
            );
            self.stop_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rt_handle() -> tokio::runtime::Handle {
        // Create a runtime for tests; the handle outlives the runtime's drop
        // because watchers keep it alive internally.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.handle().clone()
    }

    #[test]
    fn test_engine_starts_empty() {
        let engine = WatchEngine::new();
        assert_eq!(engine.watcher_count(), 0);
    }

    #[test]
    fn test_event_receiver_can_only_be_taken_once() {
        let mut engine = WatchEngine::new();
        assert!(engine.event_receiver().is_some());
        assert!(engine.event_receiver().is_none());
    }

    #[test]
    fn test_add_and_remove_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = WatchEngine::new();
        let handle = test_rt_handle();

        engine
            .add_folder(dir.path().to_path_buf(), FileFilter::new(), false, handle.clone())
            .expect("add_folder should succeed");

        assert_eq!(engine.watcher_count(), 1);
        assert!(engine.is_watching(dir.path()));

        engine
            .remove_folder(dir.path())
            .expect("remove_folder should succeed");

        assert_eq!(engine.watcher_count(), 0);
        assert!(!engine.is_watching(dir.path()));
    }

    #[test]
    fn test_add_duplicate_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = WatchEngine::new();
        let handle = test_rt_handle();

        engine
            .add_folder(dir.path().to_path_buf(), FileFilter::new(), false, handle.clone())
            .expect("first add should succeed");

        let result = engine.add_folder(dir.path().to_path_buf(), FileFilter::new(), false, handle);
        assert!(
            matches!(result, Err(WatchError::AlreadyWatching(_))),
            "Expected AlreadyWatching error"
        );
    }

    #[test]
    fn test_remove_not_watching_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = WatchEngine::new();
        let result = engine.remove_folder(dir.path());
        assert!(
            matches!(result, Err(WatchError::NotWatching(_))),
            "Expected NotWatching error"
        );
    }

    #[test]
    fn test_stop_all_clears_watchers() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let mut engine = WatchEngine::new();
        let handle = test_rt_handle();

        engine
            .add_folder(dir1.path().to_path_buf(), FileFilter::new(), false, handle.clone())
            .unwrap();
        engine
            .add_folder(dir2.path().to_path_buf(), FileFilter::new(), false, handle)
            .unwrap();

        assert_eq!(engine.watcher_count(), 2);
        engine.stop_all();
        assert_eq!(engine.watcher_count(), 0);
    }
}

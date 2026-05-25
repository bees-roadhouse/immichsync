// Watch engine orchestrator

pub mod filter;
pub mod folder;
pub mod network;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// How often the background health monitor probes each active watcher.
const HEALTH_PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// Consecutive probe misses before a watcher is considered dead and the
/// engine attempts to restart it.
const PROBE_MISS_THRESHOLD: u32 = 2;

/// Maximum number of consecutive restart attempts before a watcher is
/// permanently failed.
const MAX_RESTART_ATTEMPTS: u32 = 3;

/// Errors that can be returned by the watch engine.
#[derive(Debug, Error)]
pub enum WatchError {
    #[error("Path is already being watched: {0:?}")]
    AlreadyWatching(PathBuf),

    /// Returned by `remove_folder` (which is currently only exercised by
    /// tests) when the caller asks to drop a path that isn't tracked.
    #[error("Path is not being watched: {0:?}")]
    #[cfg_attr(not(test), allow(dead_code))]
    NotWatching(PathBuf),

    #[error("Failed to start watcher for {path:?}: {source}")]
    StartFailed {
        path: PathBuf,
        #[source]
        source: anyhow::Error,
    },
}

/// Health of an individual watcher slot, as seen by the health monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherHealth {
    /// Last probe (or no probe yet) reports the watcher is responsive.
    Healthy,
    /// One or more recent probes missed, but we haven't crossed
    /// [`PROBE_MISS_THRESHOLD`] yet.
    Degraded,
    /// The watcher exceeded [`MAX_RESTART_ATTEMPTS`] consecutive restart
    /// failures and is no longer being polled.
    Failed,
}

/// Aggregate health across all watchers, used by the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineHealth {
    /// Every watcher is healthy (or there are no watchers at all).
    Healthy,
    /// At least one watcher is degraded; none are permanently failed.
    Degraded,
    /// At least one watcher is permanently failed.
    Failed,
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

/// Per-slot state owned by the engine and shared with the health monitor.
///
/// The watcher itself, the config needed to re-create it, the probe state,
/// and the restart counter all live together so the monitor can fully drive
/// the auto-restart cycle without round-tripping through the engine.
struct WatcherSlot {
    /// Currently-active watcher; `None` while a restart is in flight or
    /// after the slot has been permanently failed.
    watcher: Option<ActiveWatcher>,
    /// Config used to (re-)create the watcher.
    config: WatcherConfig,
    /// Consecutive probe misses since the last successful probe.
    consecutive_misses: u32,
    /// Consecutive failed restart attempts since the last successful start.
    consecutive_restart_failures: u32,
    /// Current health, derived from the counters above.
    health: WatcherHealth,
    /// Last time we ran a probe against this slot (whether it succeeded
    /// or not). Used to space probes when the engine starts many watchers
    /// at the same time.
    #[allow(dead_code)]
    last_probe_at: Option<Instant>,
}

/// Everything needed to recreate a watcher after a failure.
#[derive(Clone)]
struct WatcherConfig {
    /// Canonicalised path, used as the slot key.
    canonical_path: PathBuf,
    /// The path the caller originally requested; preserved so we re-run
    /// `fs::canonicalize` on restart and pick up any new resolution.
    original_path: PathBuf,
    filter: FileFilter,
    is_network: bool,
    rt_handle: tokio::runtime::Handle,
    /// Poll interval to use when `is_network` is `true`.
    poll_interval: Duration,
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
    /// Active watchers keyed by canonical path. Held behind `Arc<Mutex>` so
    /// the background health monitor can probe and restart them without
    /// blocking the main thread.
    slots: Arc<Mutex<HashMap<PathBuf, WatcherSlot>>>,
    /// Sending half of the unified event channel.
    event_tx: mpsc::Sender<WatchEvent>,
    /// Receiving half of the unified event channel.
    ///
    /// `Option` so we can hand it to the caller via [`event_receiver`] while
    /// keeping `event_tx` alive inside the engine.
    event_rx: Option<mpsc::Receiver<WatchEvent>>,
    /// Shutdown signal for the health monitor task. Replaced on every
    /// monitor (re)start; dropped on engine drop.
    monitor_stop: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the background health monitor has been spawned.
    monitor_started: bool,
}

impl WatchEngine {
    /// Create a new, empty watch engine.
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            slots: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
            event_rx: Some(event_rx),
            monitor_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            monitor_started: false,
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
    ///   fallback is used; if `false`, a [`FolderWatcher`]
    ///   (native, debounced) is used
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
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());

        {
            let slots = self.slots.lock().unwrap();
            if slots.contains_key(&canonical) {
                return Err(WatchError::AlreadyWatching(canonical));
            }
        }

        let config = WatcherConfig {
            canonical_path: canonical.clone(),
            original_path: path,
            filter,
            is_network,
            rt_handle,
            poll_interval: DEFAULT_POLL_INTERVAL,
        };

        let watcher =
            start_watcher(&config, self.event_tx.clone()).map_err(|e| WatchError::StartFailed {
                path: canonical.clone(),
                source: e,
            })?;

        let slot = WatcherSlot {
            watcher: Some(watcher),
            config: config.clone(),
            consecutive_misses: 0,
            consecutive_restart_failures: 0,
            health: WatcherHealth::Healthy,
            last_probe_at: None,
        };

        info!(
            "Watch engine: added {:?} (network={})",
            canonical, is_network
        );
        self.slots.lock().unwrap().insert(canonical, slot);

        // Start the health monitor lazily on the first added watcher so the
        // engine creates no background tasks while empty.
        if !self.monitor_started {
            self.start_health_monitor(config.rt_handle.clone());
            self.monitor_started = true;
        }

        Ok(())
    }

    /// Remove a folder from the watch list and stop its watcher.
    ///
    /// # Errors
    ///
    /// Returns [`WatchError::NotWatching`] if the path is not currently
    /// registered.
    ///
    /// Production today never narrows the watched set; `stop_all` + rebuild
    /// is the only teardown path. Kept for the test suite that covers the
    /// engine's add/remove invariants.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn remove_folder(&mut self, path: &Path) -> Result<(), WatchError> {
        let mut slots = self.slots.lock().unwrap();

        // Try the exact path first; if that fails, try the canonicalised form.
        let key = if slots.contains_key(path) {
            path.to_path_buf()
        } else {
            match std::fs::canonicalize(path) {
                Ok(p) if slots.contains_key(&p) => p,
                _ => return Err(WatchError::NotWatching(path.to_path_buf())),
            }
        };

        if let Some(slot) = slots.remove(&key) {
            if let Some(w) = slot.watcher {
                w.stop();
            }
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
        // Signal the health monitor to exit before we tear watchers down so
        // it doesn't race with us by triggering a restart in flight.
        self.monitor_stop
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let mut slots = self.slots.lock().unwrap();
        for (path, slot) in slots.iter() {
            if let Some(w) = &slot.watcher {
                w.stop();
                info!("Watch engine: stopped {:?}", path);
            }
        }
        slots.clear();

        // Reset the monitor flag so a future `add_folder` re-spawns the task
        // with a fresh stop signal.
        self.monitor_started = false;
        self.monitor_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    }

    /// Return the number of currently active watchers.
    pub fn watcher_count(&self) -> usize {
        self.slots.lock().unwrap().len()
    }

    /// Return the set of canonicalized paths currently being watched.
    ///
    /// Used by the reconcile pass that compares the engine's view of watched
    /// folders against the DB's view, to detect drift introduced by the
    /// Settings subprocess mutating the DB without the main process getting
    /// a config-update message until the subprocess exits.
    pub fn watched_paths(&self) -> HashSet<PathBuf> {
        self.slots.lock().unwrap().keys().cloned().collect()
    }

    /// Return `true` if the given path is currently being watched.
    ///
    /// Production reconciles via `watched_paths`; this single-path lookup is
    /// only used by the engine's test suite.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_watching(&self, path: &Path) -> bool {
        let slots = self.slots.lock().unwrap();
        if slots.contains_key(path) {
            return true;
        }
        // Also check canonicalised form.
        if let Ok(p) = std::fs::canonicalize(path) {
            return slots.contains_key(&p);
        }
        false
    }

    /// Aggregate engine health, derived from the per-slot health values.
    pub fn health(&self) -> EngineHealth {
        let slots = self.slots.lock().unwrap();
        let mut any_failed = false;
        let mut any_degraded = false;
        for slot in slots.values() {
            match slot.health {
                WatcherHealth::Failed => any_failed = true,
                WatcherHealth::Degraded => any_degraded = true,
                WatcherHealth::Healthy => {}
            }
        }
        if any_failed {
            EngineHealth::Failed
        } else if any_degraded {
            EngineHealth::Degraded
        } else {
            EngineHealth::Healthy
        }
    }

    /// Snapshot the (path, health) pair for every active slot. Intended for
    /// debugging and surfacing in the tray menu / logs.
    #[allow(dead_code)] // used by tests + future tray detail menu
    pub fn watcher_health_snapshot(&self) -> Vec<(PathBuf, WatcherHealth)> {
        self.slots
            .lock()
            .unwrap()
            .iter()
            .map(|(p, s)| (p.clone(), s.health))
            .collect()
    }

    /// Spawn the background health monitor task.
    ///
    /// The task wakes every [`HEALTH_PROBE_INTERVAL`], probes each
    /// non-failed watcher, updates the per-slot health state, and triggers
    /// an in-place restart when [`PROBE_MISS_THRESHOLD`] is crossed.
    fn start_health_monitor(&self, rt_handle: tokio::runtime::Handle) {
        let slots = Arc::clone(&self.slots);
        let event_tx = self.event_tx.clone();
        let stop = Arc::clone(&self.monitor_stop);
        let interval = HEALTH_PROBE_INTERVAL;

        rt_handle.spawn(async move {
            run_health_monitor(slots, event_tx, stop, interval).await;
        });
    }
}

impl Default for WatchEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WatchEngine {
    fn drop(&mut self) {
        // Tell the monitor to exit before we tear watchers down.
        self.monitor_stop
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let mut slots = self.slots.lock().unwrap();
        if !slots.is_empty() {
            warn!(
                "WatchEngine dropped with {} active watcher(s); stopping all",
                slots.len()
            );
            for (_, slot) in slots.drain() {
                if let Some(w) = slot.watcher {
                    w.stop();
                }
            }
        }
    }
}

/// Construct a concrete watcher from a `WatcherConfig` and start it.
fn start_watcher(
    config: &WatcherConfig,
    event_tx: mpsc::Sender<WatchEvent>,
) -> anyhow::Result<ActiveWatcher> {
    if config.is_network {
        let nw = NetworkWatcher::new(
            config.canonical_path.clone(),
            config.filter.clone(),
            event_tx,
            config.poll_interval,
            config.rt_handle.clone(),
        );
        nw.start()?;
        Ok(ActiveWatcher::Network(nw))
    } else {
        let fw = FolderWatcher::new(
            config.canonical_path.clone(),
            config.filter.clone(),
            event_tx,
            config.rt_handle.clone(),
        );
        fw.start()?;
        Ok(ActiveWatcher::Folder(fw))
    }
}

/// Per-tick worker: probe each slot and react to misses.
///
/// Pulled out so tests can drive a single tick deterministically.
async fn health_monitor_tick(
    slots: &Arc<Mutex<HashMap<PathBuf, WatcherSlot>>>,
    event_tx: &mpsc::Sender<WatchEvent>,
) {
    // Snapshot the paths to probe. We don't hold the lock across the await.
    let paths: Vec<PathBuf> = {
        let guard = slots.lock().unwrap();
        guard
            .iter()
            .filter(|(_, s)| s.health != WatcherHealth::Failed && s.watcher.is_some())
            .map(|(p, _)| p.clone())
            .collect()
    };

    for path in paths {
        // Pull the watcher out long enough to probe it. The slot stays in
        // the map; only the watcher field is borrowed (via an Arc-like
        // pattern using the lock).
        let probe_result = {
            // Re-lock briefly to clone the Arc-equivalent: since FolderWatcher
            // / NetworkWatcher fields are themselves `Arc<Mutex<...>>` we
            // can hand the probe future a reference indirectly. The cleanest
            // path is to invoke the probe under a short critical section
            // that takes the watcher out, probes, and puts it back. But
            // probes await, so we instead clone the watcher's inner Arcs
            // via a dedicated `probe()` indirection.
            let guard = slots.lock().unwrap();
            match guard.get(&path).and_then(|s| s.watcher.as_ref()) {
                Some(w) => match w {
                    ActiveWatcher::Folder(fw) => ProbeHandle::Folder(fw.shallow_clone()),
                    ActiveWatcher::Network(nw) => ProbeHandle::Network(nw.shallow_clone()),
                },
                None => continue,
            }
        };

        let probe = match probe_result {
            ProbeHandle::Folder(h) => h.probe().await,
            ProbeHandle::Network(h) => h.probe().await,
        };

        // Re-acquire the slot to update state.
        let action = {
            let mut guard = slots.lock().unwrap();
            let Some(slot) = guard.get_mut(&path) else {
                continue;
            };
            slot.last_probe_at = Some(Instant::now());

            match probe {
                Ok(true) => {
                    // Probe succeeded — reset miss counter.
                    if slot.consecutive_misses > 0 {
                        info!(
                            "Watcher {:?}: probe succeeded, clearing miss counter ({} -> 0)",
                            path, slot.consecutive_misses
                        );
                    }
                    slot.consecutive_misses = 0;
                    slot.consecutive_restart_failures = 0;
                    slot.health = WatcherHealth::Healthy;
                    SlotAction::None
                }
                Ok(false) | Err(_) => {
                    slot.consecutive_misses += 1;
                    if let Err(e) = &probe {
                        warn!(
                            "Watcher {:?}: probe error (miss #{}): {}",
                            path, slot.consecutive_misses, e
                        );
                    } else {
                        warn!(
                            "Watcher {:?}: probe timed out (miss #{})",
                            path, slot.consecutive_misses
                        );
                    }

                    if slot.consecutive_misses >= PROBE_MISS_THRESHOLD {
                        slot.health = WatcherHealth::Degraded;
                        SlotAction::Restart
                    } else {
                        slot.health = WatcherHealth::Degraded;
                        SlotAction::None
                    }
                }
            }
        };

        if matches!(action, SlotAction::Restart) {
            restart_slot(slots, &path, event_tx).await;
        }
    }
}

/// Background loop driving [`health_monitor_tick`].
async fn run_health_monitor(
    slots: Arc<Mutex<HashMap<PathBuf, WatcherSlot>>>,
    event_tx: mpsc::Sender<WatchEvent>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    interval: Duration,
) {
    // Wait one full interval before the first probe so freshly-added
    // watchers have time to settle.
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the first immediate tick that `interval` emits.
    ticker.tick().await;

    loop {
        ticker.tick().await;
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            info!("Watch engine: health monitor stopping");
            return;
        }
        health_monitor_tick(&slots, &event_tx).await;
    }
}

/// What to do after evaluating a slot's probe result.
enum SlotAction {
    None,
    Restart,
}

/// Type-erased clone handle for probing. The underlying watchers' state is
/// already behind `Arc`s, so a shallow clone of the struct is enough to call
/// `probe()` without holding the slot lock.
enum ProbeHandle {
    Folder(FolderWatcher),
    Network(NetworkWatcher),
}

/// Stop the current watcher in the slot, re-create it from the saved
/// config, and update the slot. Bumps the restart-failure counter and
/// marks the slot `Failed` after [`MAX_RESTART_ATTEMPTS`].
async fn restart_slot(
    slots: &Arc<Mutex<HashMap<PathBuf, WatcherSlot>>>,
    path: &PathBuf,
    event_tx: &mpsc::Sender<WatchEvent>,
) {
    // Take the current watcher out and snapshot the config under the lock.
    let config = {
        let mut guard = slots.lock().unwrap();
        let Some(slot) = guard.get_mut(path) else {
            return;
        };
        if slot.consecutive_restart_failures >= MAX_RESTART_ATTEMPTS {
            // Should not be reached because tick() filters Failed slots, but
            // guard anyway.
            return;
        }
        if let Some(w) = slot.watcher.take() {
            w.stop();
        }
        slot.config.clone()
    };

    warn!(
        "Watch engine: restarting dead watcher for {:?} (network={})",
        path, config.is_network
    );

    // Re-canonicalise in case the path resolution changed (network share
    // re-mounted at a different target, etc.).
    let mut fresh_config = config.clone();
    if let Ok(canon) = std::fs::canonicalize(&config.original_path) {
        fresh_config.canonical_path = canon;
    }

    let result = tokio::task::spawn_blocking({
        let fresh_config = fresh_config.clone();
        let event_tx = event_tx.clone();
        move || start_watcher(&fresh_config, event_tx)
    })
    .await;

    let started = match result {
        Ok(Ok(w)) => Some(w),
        Ok(Err(e)) => {
            warn!("Watch engine: restart failed for {:?}: {}", path, e);
            None
        }
        Err(join_err) => {
            warn!(
                "Watch engine: restart task panicked for {:?}: {}",
                path, join_err
            );
            None
        }
    };

    {
        let mut guard = slots.lock().unwrap();
        let Some(slot) = guard.get_mut(path) else {
            // Slot was removed while we were restarting; drop the new watcher.
            if let Some(w) = started {
                w.stop();
            }
            return;
        };

        match started {
            Some(w) => {
                slot.watcher = Some(w);
                slot.consecutive_misses = 0;
                slot.consecutive_restart_failures = 0;
                slot.health = WatcherHealth::Degraded; // verified by next probe
                info!("Watch engine: restarted watcher for {:?}", path);
            }
            None => {
                slot.consecutive_restart_failures += 1;
                if slot.consecutive_restart_failures >= MAX_RESTART_ATTEMPTS {
                    slot.health = WatcherHealth::Failed;
                    let msg = format!(
                        "Watcher for {:?} permanently failed after {} restart attempts",
                        path, MAX_RESTART_ATTEMPTS
                    );
                    warn!("{}", msg);
                    let _ = event_tx.try_send(WatchEvent::Error(msg));
                } else {
                    slot.health = WatcherHealth::Degraded;
                }
            }
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
        assert_eq!(engine.health(), EngineHealth::Healthy);
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
            .add_folder(
                dir.path().to_path_buf(),
                FileFilter::new(),
                false,
                handle.clone(),
            )
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
            .add_folder(
                dir.path().to_path_buf(),
                FileFilter::new(),
                false,
                handle.clone(),
            )
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
            .add_folder(
                dir1.path().to_path_buf(),
                FileFilter::new(),
                false,
                handle.clone(),
            )
            .unwrap();
        engine
            .add_folder(dir2.path().to_path_buf(), FileFilter::new(), false, handle)
            .unwrap();

        assert_eq!(engine.watcher_count(), 2);
        engine.stop_all();
        assert_eq!(engine.watcher_count(), 0);
    }

    /// Single probe round-trip against a real `FolderWatcher`: we add a
    /// folder, run one tick, and verify the slot stays Healthy.
    #[tokio::test]
    async fn test_probe_succeeds_on_healthy_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = WatchEngine::new();
        let handle = tokio::runtime::Handle::current();

        engine
            .add_folder(
                dir.path().to_path_buf(),
                FileFilter::new(),
                false,
                handle.clone(),
            )
            .unwrap();

        // Drain any startup noise.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let slots = Arc::clone(&engine.slots);
        let (event_tx, _event_rx) = mpsc::channel(8);
        health_monitor_tick(&slots, &event_tx).await;

        let snapshot = engine.watcher_health_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].1, WatcherHealth::Healthy);
        assert_eq!(engine.health(), EngineHealth::Healthy);
    }

    /// Inject N synthetic probe misses and verify we cross into Degraded,
    /// then attempt a restart, then permanently Fail on repeated restart
    /// failures.
    ///
    /// We do this by hand-mutating the slot's counters under the lock, the
    /// same way `health_monitor_tick` would. That keeps the test on the
    /// state machine itself rather than depending on real probe timeouts.
    #[tokio::test]
    async fn test_failure_then_giveup_state_machine() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = WatchEngine::new();
        let handle = tokio::runtime::Handle::current();

        engine
            .add_folder(
                dir.path().to_path_buf(),
                FileFilter::new(),
                false,
                handle.clone(),
            )
            .unwrap();

        let path = std::fs::canonicalize(dir.path()).unwrap();
        let slots = Arc::clone(&engine.slots);

        // Simulate two consecutive probe misses + a failed restart.
        // Inject a config that we know will fail to restart by pointing the
        // saved config at a non-existent path.
        let bogus = std::env::temp_dir().join("immichsync_test_path_that_does_not_exist_xyzzy");
        {
            let mut guard = slots.lock().unwrap();
            let slot = guard.get_mut(&path).unwrap();
            slot.consecutive_misses = PROBE_MISS_THRESHOLD;
            slot.health = WatcherHealth::Degraded;
            slot.config.original_path = bogus.clone();
            slot.config.canonical_path = bogus.clone();
            // Stop the real watcher so the restart path isn't competing.
            if let Some(w) = slot.watcher.take() {
                w.stop();
            }
        }

        let (event_tx, mut event_rx) = mpsc::channel(8);

        // Call restart MAX_RESTART_ATTEMPTS times — each will fail because
        // the path doesn't exist.
        for _ in 0..MAX_RESTART_ATTEMPTS {
            restart_slot(&slots, &path, &event_tx).await;
        }

        let final_health = {
            let guard = slots.lock().unwrap();
            guard.get(&path).unwrap().health
        };
        assert_eq!(
            final_health,
            WatcherHealth::Failed,
            "Slot should be permanently Failed after {} restart attempts",
            MAX_RESTART_ATTEMPTS
        );
        assert_eq!(engine.health(), EngineHealth::Failed);

        // We should have surfaced at least one Error event to the pipeline.
        let mut saw_error = false;
        while let Ok(ev) = event_rx.try_recv() {
            if matches!(ev, WatchEvent::Error(_)) {
                saw_error = true;
            }
        }
        assert!(saw_error, "Expected an Error event after permanent failure");
    }

    /// A successful probe should clear an earlier miss counter and bring
    /// the slot back to Healthy.
    #[tokio::test]
    async fn test_miss_then_recovery_clears_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = WatchEngine::new();
        let handle = tokio::runtime::Handle::current();

        engine
            .add_folder(
                dir.path().to_path_buf(),
                FileFilter::new(),
                false,
                handle.clone(),
            )
            .unwrap();

        let path = std::fs::canonicalize(dir.path()).unwrap();
        let slots = Arc::clone(&engine.slots);

        // Inject one probe miss (below threshold).
        {
            let mut guard = slots.lock().unwrap();
            let slot = guard.get_mut(&path).unwrap();
            slot.consecutive_misses = 1;
            slot.health = WatcherHealth::Degraded;
        }
        assert_eq!(engine.health(), EngineHealth::Degraded);

        // Drive a real probe tick — the slot has a real, healthy watcher,
        // so the probe should succeed and reset the counter.
        let (event_tx, _event_rx) = mpsc::channel(8);
        tokio::time::sleep(Duration::from_millis(100)).await;
        health_monitor_tick(&slots, &event_tx).await;

        let snapshot = engine.watcher_health_snapshot();
        assert_eq!(snapshot[0].1, WatcherHealth::Healthy);
        assert_eq!(engine.health(), EngineHealth::Healthy);
    }
}

// Application state and lifecycle management.
//
// `App` wires together the upload pipeline, watch engine, system tray, and
// settings UI.  The main loop polls everything every 50 ms and drains the
// OS event queue (Win32 messages / AppKit events) through
// `platform::event_loop::pump` — simpler and more reliable for a tray-only
// app than running a full winit event loop.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, trace, warn};

use crate::api::ImmichClient;
use crate::config::Config;
use crate::db::DbStore;
use crate::ui::tray::{TrayAction, TrayApp, TrayState};
use crate::updater::{self, UpdateCheckResult, UpdateInfo};
use crate::upload::queue::{QueueStore, UploadQueue};
use crate::upload::worker::AssetUploader;
use crate::upload::UploadPipeline;
use crate::watch::filter::FileFilter;
use crate::watch::folder::WatchEvent;
use crate::watch::WatchEngine;

/// Top-level application orchestrator.
pub struct App {
    config: Config,
    db: Arc<DbStore>,
    client: Option<ImmichClient>,
    pipeline: Option<UploadPipeline>,
    watch_engine: Option<WatchEngine>,
    tray: Option<TrayApp>,
    tray_rx: Option<std::sync::mpsc::Receiver<TrayAction>>,
    /// Receiver for config updates from the settings window (spawned on another thread).
    settings_rx: Option<std::sync::mpsc::Receiver<Config>>,
    runtime: Arc<tokio::runtime::Runtime>,
    last_stats_update: Instant,
    last_trash_cleanup: Instant,
    /// Last time we pruned the logs directory against the retention cap.
    last_log_prune: Instant,
    /// Paths we've already emitted the "Watch folder missing" warning for
    /// since process start. Subsequent skip events for the same path are
    /// demoted to TRACE so the reconcile loop (which calls `start_watch_engine`
    /// on every drift) doesn't spam the log file once per cycle.
    warned_missing_paths: HashSet<PathBuf>,
    /// Last time we reconciled the WatchEngine's watched-folder set against the DB.
    /// The Settings subprocess mutates the DB independently of the running engine,
    /// so we periodically diff and converge to fix the "Remove without Save still
    /// uploads files" footgun.
    last_folder_reconcile: Instant,
    paused: bool,
    /// Track previous syncing state to detect transitions for notifications.
    was_syncing: bool,
    /// Notification manager.
    notifications: crate::ui::notifications::Notifications,
    /// Tracks whether each UI window type is currently open.
    window_open: WindowOpenTracker,
    /// Receiver for async update check results.
    update_rx: Option<std::sync::mpsc::Receiver<UpdateCheckResult>>,
    /// Receiver for background download completion results.
    update_download_rx: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    /// When the last update check was performed.
    last_update_check: Instant,
    /// Cached update info when an update is available.
    pending_update: Option<UpdateInfo>,
    /// Set to true once the update has been downloaded and applied.
    update_ready: bool,
    /// The "can't read the API key" notification fires once per run (#114).
    key_error_notified: bool,
    /// The "API key rejected, sign in again" notification fires once per
    /// pipeline (#143).
    auth_rejected_notified: bool,
    /// When the last server-reachability ping fired. The tray's Offline state
    /// is driven by this ... a single failed ping flips us to Offline, and
    /// a successful ping (or starting an upload, since uploads imply
    /// reachability) clears it.
    last_ping_at: Instant,
    /// Receiver for async ping results. `Ok(true)` = reachable, anything else
    /// = unreachable.
    ping_rx: Option<std::sync::mpsc::Receiver<bool>>,
    /// Latched offline state. The tray icon and tooltip key off this when
    /// no upload is in flight.
    server_offline: bool,
    /// Mount/unmount events for removable media (`platform::volumes`).
    /// `None` when the monitor failed to start; device auto-watch is then off.
    volume_monitor: Option<crate::platform::volumes::VolumeMonitor>,
    volume_rx: Option<std::sync::mpsc::Receiver<crate::platform::volumes::VolumeEvent>>,
}

/// Tracks whether each type of UI window is currently open, preventing
/// multiple instances of the same window from being spawned.
#[derive(Clone)]
pub struct WindowOpenTracker {
    pub settings: Arc<AtomicBool>,
    /// PID of the open Settings subprocess (0 when none), so a second
    /// Settings click can bring it to the front (#102).
    pub settings_pid: Arc<std::sync::atomic::AtomicU32>,
    pub about: Arc<AtomicBool>,
    pub upload_log: Arc<AtomicBool>,
    pub trash_log: Arc<AtomicBool>,
    pub update: Arc<AtomicBool>,
}

impl WindowOpenTracker {
    fn new() -> Self {
        Self {
            settings: Arc::new(AtomicBool::new(false)),
            settings_pid: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            about: Arc::new(AtomicBool::new(false)),
            upload_log: Arc::new(AtomicBool::new(false)),
            trash_log: Arc::new(AtomicBool::new(false)),
            update: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl App {
    /// Create a new app instance. Call [`init`] then [`run`].
    pub fn new(
        config: Config,
        db: Arc<DbStore>,
        client: Option<ImmichClient>,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Self {
        let notifications = crate::ui::notifications::Notifications::new(&config);
        Self {
            config,
            db,
            client,
            pipeline: None,
            watch_engine: None,
            tray: None,
            tray_rx: None,
            settings_rx: None,
            runtime,
            last_stats_update: Instant::now(),
            last_trash_cleanup: Instant::now(),
            last_log_prune: Instant::now(),
            warned_missing_paths: HashSet::new(),
            last_folder_reconcile: Instant::now(),
            paused: false,
            was_syncing: false,
            notifications,
            window_open: WindowOpenTracker::new(),
            update_rx: None,
            update_download_rx: None,
            last_update_check: Instant::now(),
            pending_update: None,
            update_ready: false,
            key_error_notified: false,
            auth_rejected_notified: false,
            last_ping_at: Instant::now(),
            ping_rx: None,
            server_offline: false,
            volume_monitor: None,
            volume_rx: None,
        }
    }

    /// Initialize the tray icon, upload pipeline, and watch engine.
    ///
    /// Must be called on the main thread before [`run`].
    pub fn init(&mut self) -> anyhow::Result<()> {
        // Register the hidden top-level shutdown window before anything else.
        // Without it, Task Manager → "End task" can't find a window to send
        // WM_CLOSE to and goes straight to TerminateProcess, leaving
        // in-flight uploads orphaned. The window must be created on the
        // same thread that runs the main message pump (this thread).
        if let Err(e) = crate::platform::shutdown::install() {
            warn!(error = %e, "Failed to register shutdown window — Task Manager 'End task' may force-kill");
        }

        // Recover any entries stuck in "uploading" state from a previous crash.
        {
            let db = self.db.inner().lock().unwrap();
            if let Err(e) = db.reset_stale_uploading() {
                warn!(error = %e, "Failed to reset stale uploading entries");
            }

            // Clean up expired trash files on startup.
            if let Ok(folders) = db.get_folders() {
                crate::upload::worker::cleanup_trash(
                    &folders,
                    self.config.upload.trash_retention_days,
                );
            }
        }

        // Create system tray (must be on main thread, after the platform's
        // app object exists — NSApplication on macOS).
        crate::platform::event_loop::init();
        let (tray, tray_rx) =
            TrayApp::new().map_err(|e| anyhow::anyhow!("Failed to create tray: {}", e))?;
        self.tray = Some(tray);
        self.tray_rx = Some(tray_rx);

        // Create and start the upload pipeline if server is configured.
        self.start_pipeline()?;

        // Set up the watch engine and bridge task.
        self.start_watch_engine()?;

        // Removable media: mounts become watched folders (#89). A failure
        // here only disables device auto-watch.
        let (volume_tx, volume_rx) = std::sync::mpsc::channel();
        match crate::platform::volumes::VolumeMonitor::start(volume_tx) {
            Ok(m) => {
                self.volume_monitor = Some(m);
                self.volume_rx = Some(volume_rx);
            }
            Err(e) => {
                warn!(error = %e, "Volume monitor failed to start; removable media won't be auto-watched")
            }
        }

        // Photos library: queue the newest assets if it's on and access was
        // already granted. Never prompts; the prompt belongs to Settings.
        self.queue_photos_library();

        // An update applied with its restart deferred (Later at startup, or
        // a quit before restarting): offer the restart, don't re-download.
        if let Some(version) = updater::pending_restart_version() {
            self.mark_update_ready(Some(&version));
        }

        // Schedule the first automatic update check after a startup delay.
        if crate::platform::SELF_UPDATE_SUPPORTED && self.config.advanced.check_for_updates {
            self.schedule_update_check(true);
        }

        Ok(())
    }

    /// Run the main message loop. Blocks until the user selects Quit.
    ///
    /// Drains the OS event queue each tick (`platform::event_loop::pump`) so
    /// the tray icon's menu works (Win32 messages / AppKit events).
    pub fn run(&mut self) {
        info!("Entering main message loop");

        loop {
            // Pump all pending OS events. This is what makes the tray
            // icon's context menu work — on Windows tray-icon's hidden
            // window needs message dispatch, on macOS the NSStatusItem
            // needs AppKit events. `true` means the OS asked us to quit.
            if crate::platform::event_loop::pump() {
                self.shutdown_and_exit();
            }

            // Process tray menu actions. Collect first to avoid
            // borrow conflict (tray_rx is &self, handle needs &mut self).
            let actions: Vec<TrayAction> = self
                .tray_rx
                .as_ref()
                .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
                .unwrap_or_default();

            let mut quit = false;
            for action in actions {
                if action == TrayAction::Quit {
                    quit = true;
                } else {
                    self.handle_tray_action(action);
                }
            }
            if quit {
                info!("Quit requested from tray");
                self.shutdown_and_exit();
            }

            // Check for config updates from the settings window or a
            // Keychain retry. Cleared once it lands, so `is_some()` means a
            // reload is in flight.
            let reload = self.settings_rx.as_ref().map(|rx| rx.try_recv());
            match reload {
                Some(Ok(new_config)) => {
                    self.settings_rx = None;
                    self.apply_new_config(new_config);
                }
                Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                    // The reload failed (logged where it ran): keep what we
                    // have, and offer Retry again if the key is still unreadable.
                    self.settings_rx = None;
                    let unreadable = self.config.server.unreadable_key.is_some();
                    if let Some(ref mut tray) = self.tray {
                        tray.set_key_unreadable(unreadable);
                    }
                }
                Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => {}
            }

            // Poll for update check results.
            self.poll_update_check();

            // Poll for background download completion.
            self.poll_update_download();

            // Periodic re-check for updates.
            let check_interval = updater::check_interval_from_hours(
                self.config.advanced.update_check_interval_hours,
            );
            if crate::platform::SELF_UPDATE_SUPPORTED
                && self.config.advanced.check_for_updates
                && self.last_update_check.elapsed() >= check_interval
            {
                self.schedule_update_check(false);
            }

            // Periodic server-reachability check. Drives the Offline tray state.
            self.poll_ping_result();
            self.maybe_schedule_ping();

            // Periodically refresh tray with queue statistics.
            self.update_tray_stats();

            // Advance the syncing-state icon animation. Cheap when not
            // syncing (early-returns on the absence of a scheduled deadline).
            if let Some(ref mut tray) = self.tray {
                tray.tick_animation();
            }

            // Periodically reconcile the WatchEngine's view of watched folders
            // against the DB. The Settings subprocess mutates the DB without
            // notifying the main process until it exits — until reconcile fires,
            // a folder removed via Settings would still be watched (and its files
            // uploaded). Cheap when there's no drift; restarts the engine when
            // there is.
            self.poll_volume_events();
            self.reconcile_watch_folders();

            // Periodic log-retention check (hourly). Caps logs/ at 1 GiB by
            // deleting oldest files first; never touches today's file. Cheap
            // when totals are well under the cap (single dir read + sum).
            self.maybe_prune_logs();

            // Periodic trash cleanup (hourly).
            if self.last_trash_cleanup.elapsed() >= Duration::from_secs(3600) {
                self.last_trash_cleanup = Instant::now();
                let db = self.db.inner().lock().unwrap();
                if let Ok(folders) = db.get_folders() {
                    crate::upload::worker::cleanup_trash(
                        &folders,
                        self.config.upload.trash_retention_days,
                    );
                }
            }

            // Sleep to avoid busy-waiting (~20 wakes/sec is plenty).
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // ── internals ────────────────────────────────────────────────────────

    /// Create and start the upload pipeline when there is a client;
    /// otherwise put the reason in the tray. Called at startup and again
    /// when a config reload (Settings, Retry Keychain Access) first
    /// produces a client (#114).
    fn start_pipeline(&mut self) -> anyhow::Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        if let Some(client) = self.client.clone() {
            let client = &client;
            let uploader: Arc<dyn AssetUploader> = Arc::new(client.clone());
            let store: Arc<dyn QueueStore> = self.db.clone();
            #[allow(unused_mut)]
            let mut pipeline =
                UploadPipeline::new(store, &self.config.server, &self.config.upload, uploader);

            // Photos library entries are exported by PhotoKit (ADR 0002).
            // The source is attached whether or not the library is enabled,
            // so entries already queued still settle if it's turned off.
            #[cfg(target_os = "macos")]
            {
                // Before any worker can export: clear what a crash left.
                crate::platform::photos::sweep_stale_exports();
                let photos = &self.config.photos;
                let albums = crate::upload::worker::AlbumPolicy {
                    mirror: photos.sync_albums,
                    all_in: Some(photos.all_in_album.trim().to_string()).filter(|a| !a.is_empty()),
                };
                pipeline = pipeline.with_library(crate::upload::worker::LibrarySource::new(
                    Arc::new(crate::platform::photos::PhotoKitExporter),
                    albums,
                ));
            }

            // Pipeline spawns tokio tasks — enter the runtime context.
            let _guard = self.runtime.enter();
            pipeline
                .start()
                .map_err(|e| anyhow::anyhow!("Failed to start pipeline: {}", e))?;

            self.pipeline = Some(pipeline);

            // Update tray with server info.
            if let Some(ref mut tray) = self.tray {
                let url_display = self
                    .config
                    .server
                    .url
                    .trim_start_matches("https://")
                    .trim_start_matches("http://");
                tray.set_server_status(url_display, true);
            }
        } else if let Some(ref unreadable) = self.config.server.unreadable_key {
            warn!(error = %unreadable.error, "Stored API key unreadable; pipeline not started");
            if let Some(ref mut tray) = self.tray {
                tray.update_state(TrayState::Error(
                    "can't read the API key — see the menu".to_string(),
                ));
                tray.set_key_unreadable(true);
            }
            if !self.key_error_notified {
                self.key_error_notified = true;
                self.notifications
                    .show_toast_raw("ImmichSync isn't uploading", unreadable.user_message());
            }
        } else {
            info!("No server configured; pipeline not started");
            if let Some(ref mut tray) = self.tray {
                tray.update_state(TrayState::Error("Server not configured".to_string()));
            }
        }
        Ok(())
    }

    /// Register watched folders and spawn the watch→pipeline bridge task.
    fn start_watch_engine(&mut self) -> anyhow::Result<()> {
        use std::collections::HashMap;

        let mut engine = WatchEngine::new();
        let event_rx = engine
            .event_receiver()
            .ok_or_else(|| anyhow::anyhow!("event_receiver already taken"))?;

        // Map from canonical watched path → folder DB id, so the bridge task
        // can look up which folder a file event belongs to.
        let mut path_to_folder_id: HashMap<std::path::PathBuf, i64> = HashMap::new();

        // Load watched folders from the database.
        {
            let db = self.db.inner().lock().unwrap();
            // Rows from before #99 may hold `~`, odd case or a symlink.
            let m = db.canonicalize_folder_paths();
            if m.changed > 0 || m.collisions > 0 {
                info!(
                    changed = m.changed,
                    duplicates = m.collisions,
                    "Canonicalised watched-folder paths"
                );
            }
            let folders = db.get_folders()?;

            if folders.is_empty() {
                drop(db);
                // First run: add the user's Pictures folder as a default.
                if let Some(pictures) = crate::platform::known_folders::get_pictures_folder()
                    .ok()
                    .and_then(|p| crate::watch::paths::normalize_folder(&p.to_string_lossy()).ok())
                    .map(std::path::PathBuf::from)
                {
                    info!(path = %pictures.display(), "Adding default Pictures folder");
                    let db = self.db.inner().lock().unwrap();
                    if let Ok(id) =
                        db.add_folder(&pictures.display().to_string(), Some("Pictures"), true)
                    {
                        // Default Pictures folder has no per-folder patterns.
                        let filter = FileFilter::new();
                        let canon =
                            std::fs::canonicalize(&pictures).unwrap_or_else(|_| pictures.clone());
                        if let Err(e) = engine.add_folder(
                            pictures,
                            filter,
                            false,
                            self.runtime.handle().clone(),
                        ) {
                            warn!("Failed to add Pictures watcher: {}", e);
                        } else {
                            path_to_folder_id.insert(canon, id);
                        }
                    }
                }
            } else {
                for folder in &folders {
                    if !folder.enabled {
                        continue;
                    }
                    let path = std::path::PathBuf::from(&folder.path);
                    if !path.exists() {
                        warn_missing_folder_once(&mut self.warned_missing_paths, &path);
                        continue;
                    }
                    let is_network = folder.watch_mode == crate::db::WatchMode::Poll;
                    let filter = FileFilter::for_folder(folder);
                    let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    if let Err(e) =
                        engine.add_folder(path, filter, is_network, self.runtime.handle().clone())
                    {
                        warn!(path = %folder.path, error = %e, "Failed to add watcher");
                    } else {
                        path_to_folder_id.insert(canon, folder.id);
                    }
                }
            }
        }

        info!(count = engine.watcher_count(), "Watch engine started");
        self.watch_engine = Some(engine);

        // Spawn the bridge task that feeds watch events into the upload queue.
        if self.pipeline.is_some() {
            let store: Arc<dyn QueueStore> = self.db.clone();
            let concurrency = self.config.upload.concurrency as usize;
            let folder_map = Arc::new(path_to_folder_id);

            info!(
                "Spawning watch→pipeline bridge task (concurrency={})",
                concurrency
            );
            let bridge_store = store.clone();
            let bridge_map = folder_map.clone();
            self.runtime.spawn(async move {
                bridge_watch_to_pipeline(event_rx, bridge_store, concurrency, bridge_map).await;
            });

            // Run a one-time initial scan of all watched folders to pick up
            // existing files that were added before the watcher started.
            let scan_store = store;
            let scan_map = folder_map;
            self.runtime.spawn(async move {
                initial_scan(scan_store, scan_map).await;
            });
        } else {
            warn!("No upload pipeline — watch events will not be processed. Is the server configured?");
        }

        Ok(())
    }

    /// Dispatch a tray menu action (everything except Quit).
    ///
    /// UI windows (Settings, About, Upload Log) are spawned as child
    /// processes so each gets its own winit EventLoop — winit 0.30 only
    /// allows one EventLoop per process lifetime.
    fn handle_tray_action(&mut self, action: TrayAction) {
        match action {
            TrayAction::Pause => {
                if let Some(ref pipeline) = self.pipeline {
                    pipeline.pause();
                    self.paused = true;
                    if let Some(ref mut tray) = self.tray {
                        tray.update_state(TrayState::Paused);
                    }
                }
            }
            TrayAction::Resume => {
                if let Some(ref pipeline) = self.pipeline {
                    pipeline.resume();
                    self.paused = false;
                    if let Some(ref mut tray) = self.tray {
                        tray.update_state(TrayState::Idle);
                    }
                }
            }
            TrayAction::OpenSettings => {
                if self.window_open.settings.swap(true, Ordering::SeqCst) {
                    let pid = self.window_open.settings_pid.load(Ordering::SeqCst);
                    info!(
                        pid,
                        "Settings window already open; bringing it to the front"
                    );
                    crate::platform::focus_window(pid, crate::ui::settings::WINDOW_TITLE);
                    return;
                }
                let (tx, rx) = std::sync::mpsc::channel();
                self.settings_rx = Some(rx);
                let flag = self.window_open.settings.clone();
                spawn_window_subprocess(
                    "settings",
                    flag,
                    Some(self.window_open.settings_pid.clone()),
                    Some(tx),
                );
            }
            TrayAction::UploadNow => {
                // Re-run the scan of every watched folder (the same restart
                // the folder reconcile does), so anything the watcher missed
                // is queued now (#100).
                info!("Upload Now requested: rescanning watched folders");
                self.restart_watch_engine();
            }
            TrayAction::About => {
                if self.window_open.about.swap(true, Ordering::SeqCst) {
                    info!("About window already open, ignoring");
                    return;
                }
                let flag = self.window_open.about.clone();
                spawn_window_subprocess("about", flag, None, None);
            }
            TrayAction::ViewLog => {
                if self.window_open.upload_log.swap(true, Ordering::SeqCst) {
                    info!("Upload Log window already open, ignoring");
                    return;
                }
                let flag = self.window_open.upload_log.clone();
                spawn_window_subprocess("log", flag, None, None);
            }
            TrayAction::ViewTrash => {
                if self.window_open.trash_log.swap(true, Ordering::SeqCst) {
                    info!("Trash Log window already open, ignoring");
                    return;
                }
                let flag = self.window_open.trash_log.clone();
                spawn_window_subprocess("trash-log", flag, None, None);
            }
            TrayAction::RetryKeychain => {
                // Re-read config.toml on a worker thread: the Keychain read
                // can block on macOS's access prompt. The result arrives
                // through the same channel a Settings save uses.
                if self.window_open.settings.load(Ordering::SeqCst) {
                    info!("Settings is open; it reloads the config on close");
                    return;
                }
                if self.settings_rx.is_some() {
                    info!("A config reload is already in flight; ignoring Retry");
                    return;
                }
                info!("Retrying the stored API key");
                // Greyed out until the reload lands, so clicks while macOS's
                // prompt is up don't stack Keychain reads (#130 review).
                if let Some(ref mut tray) = self.tray {
                    tray.set_key_unreadable(false);
                }
                let (tx, rx) = std::sync::mpsc::channel();
                self.settings_rx = Some(rx);
                std::thread::spawn(move || match Config::load() {
                    Ok(config) => {
                        let _ = tx.send(config);
                    }
                    Err(e) => warn!(error = %e, "Config reload for Keychain retry failed"),
                });
            }
            TrayAction::CheckForUpdates => {
                info!("Manual update check requested");
                if self.update_ready {
                    let version = self
                        .pending_update
                        .as_ref()
                        .map(|i| format!(" v{}", i.new_version))
                        .unwrap_or_default();
                    self.notifications.show_toast_raw(
                        "Update Ready",
                        &format!(
                            "ImmichSync{version} is already installed. Choose Restart to Update."
                        ),
                    );
                }
                self.schedule_update_check(false);
            }
            TrayAction::OpenUpdateDialog => {
                self.open_update_dialog();
            }
            TrayAction::RestartToUpdate => {
                if self.update_ready {
                    info!("User requested restart to apply update");
                    self.shutdown();
                    updater::relaunch_self();
                }
            }
            TrayAction::UpdateAppliedRestart => {
                info!("Update applied from the dialog; restarting gracefully");
                self.update_ready = true;
                self.shutdown();
                updater::relaunch_self();
            }
            TrayAction::UpdateAppliedLater => {
                let version = self
                    .pending_update
                    .as_ref()
                    .map(|i| i.new_version.clone())
                    .or_else(updater::pending_restart_version);
                self.mark_update_ready(version.as_deref());
            }
            TrayAction::Quit => {
                // Handled in run() directly.
            }
        }
    }

    /// Apply a new config received from the settings window.
    fn apply_new_config(&mut self, new_config: Config) {
        info!("Applying config update from settings");

        let server_changed = new_config.server.url != self.config.server.url
            || new_config.server.api_key != self.config.server.api_key;

        let photos_turned_on = new_config.photos.enabled && !self.config.photos.enabled;

        // Apply the new config.
        self.config = new_config;
        if photos_turned_on {
            self.queue_photos_library();
        }
        self.notifications = crate::ui::notifications::Notifications::new(&self.config);

        if server_changed {
            info!("Server config changed, recreating client");

            // Recreate the Immich client with the full upload config (bandwidth
            // limit and inactivity timeout may have changed along with the server).
            if !self.config.server.url.is_empty() && !self.config.server.api_key.is_empty() {
                match ImmichClient::with_bandwidth_limit(
                    &self.config.server.url,
                    &self.config.server.api_key,
                    self.config.upload.bandwidth_limit_kbps,
                    self.config.upload.timeout_secs,
                ) {
                    Ok(c) => {
                        self.client = Some(c);
                        if let Some(ref mut tray) = self.tray {
                            let url_display = self
                                .config
                                .server
                                .url
                                .trim_start_matches("https://")
                                .trim_start_matches("http://");
                            tray.set_server_status(url_display, true);
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to create new Immich client");
                        self.client = None;
                    }
                }
            } else {
                // No URL or no key now: there's nothing to upload with.
                self.client = None;
            }
        }

        // A new key or server: the running pipeline holds the old client, so
        // stop it and let start_pipeline below build one with the new key.
        // Before this, a key changed in Settings (or a sign-in after a 401)
        // didn't reach the uploads until a restart (#143).
        if server_changed {
            if let Some(mut old) = self.pipeline.take() {
                info!("Server or key changed; rebuilding the upload pipeline");
                let rt = self.runtime.clone();
                rt.block_on(async {
                    old.stop().await;
                });
            }
            self.auth_rejected_notified = false;
        }

        // A reload that fixed (or still has) an unreadable key: start the
        // pipeline now if this is the first client, and keep the tray honest.
        if let Some(ref mut tray) = self.tray {
            tray.set_key_unreadable(self.config.server.unreadable_key.is_some());
        }
        let had_pipeline = self.pipeline.is_some();
        if let Err(e) = self.start_pipeline() {
            warn!(error = %e, "Failed to start the upload pipeline after config update");
        } else if !had_pipeline && self.pipeline.is_some() {
            info!("Upload pipeline started after config reload");
            if let Some(ref mut tray) = self.tray {
                tray.update_state(TrayState::Idle);
            }
        }

        // Reload watched folders by stopping and restarting the watch engine.
        if let Some(ref mut engine) = self.watch_engine {
            engine.stop_all();
        }
        self.watch_engine = None;

        if let Err(e) = self.start_watch_engine() {
            warn!(error = %e, "Failed to restart watch engine after config update");
        }
    }

    /// Queue the newest Photos library assets on a background thread (the
    /// PhotoKit fetch blocks). A no-op unless the library is enabled, the
    /// pipeline exists, and access is already granted, so it never raises
    /// the permission prompt. Tracer: capped at `TRACER_CAP` (#80).
    fn queue_photos_library(&self) {
        #[cfg(target_os = "macos")]
        {
            use crate::platform::photos;
            if !self.config.photos.enabled || self.pipeline.is_none() {
                return;
            }
            let access = photos::access();
            if !access.can_read() {
                info!(
                    ?access,
                    "Photos library enabled but not readable; nothing queued"
                );
                return;
            }
            let store: Arc<dyn QueueStore> = self.db.clone();
            std::thread::spawn(move || {
                let ids = photos::newest_asset_ids(crate::watch::photos::TRACER_CAP);
                crate::watch::photos::enqueue_new(store.as_ref(), &ids);
            });
        }
    }

    /// Periodically update the tray icon with queue stats.
    fn update_tray_stats(&mut self) {
        if self.last_stats_update.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_stats_update = Instant::now();

        if self.paused {
            return;
        }

        // Watcher health takes precedence over queue stats: if any watcher
        // is permanently failed (or degraded after probe misses) we want
        // the tray to advertise that, not "Idle".
        let engine_health = self
            .watch_engine
            .as_ref()
            .map(|e| e.health())
            .unwrap_or(crate::watch::EngineHealth::Healthy);

        // A rejected key outranks everything else in the tray: nothing will
        // upload until the user signs in again (#143).
        if self.pipeline.as_ref().is_some_and(|p| p.auth_rejected()) {
            if let Some(ref mut tray) = self.tray {
                tray.update_state(TrayState::Error(
                    "Immich rejected the API key — sign in again in Settings".to_owned(),
                ));
            }
            if !self.auth_rejected_notified {
                self.auth_rejected_notified = true;
                self.notifications.show_toast_raw(
                    "Sign in to Immich again",
                    "Immich rejected ImmichSync's API key (revoked or expired). Uploads are \
                     paused, nothing is lost. Open Settings and sign in again.",
                );
            }
            return;
        }

        if let Some(ref pipeline) = self.pipeline {
            if let Ok(stats) = pipeline.stats() {
                if let Some(ref mut tray) = self.tray {
                    let active = stats.uploading + stats.pending;
                    let is_syncing = active > 0;

                    match engine_health {
                        crate::watch::EngineHealth::Failed => {
                            tray.update_state(TrayState::Error(
                                "one or more watchers are offline".to_owned(),
                            ));
                        }
                        crate::watch::EngineHealth::Degraded => {
                            // Stay on the syncing icon if uploads are
                            // flowing; otherwise advertise the degraded
                            // state via the Error tray channel (yellow
                            // would conflict with Paused).
                            if is_syncing {
                                tray.update_state(TrayState::Syncing {
                                    current: stats.uploading as u32,
                                    total: active as u32,
                                });
                            } else {
                                tray.update_state(TrayState::Error(
                                    "watcher degraded — auto-restart in progress".to_owned(),
                                ));
                            }
                        }
                        crate::watch::EngineHealth::Healthy => {
                            if is_syncing {
                                // Active uploads override the Offline display ...
                                // if we're actually moving bytes, by definition
                                // the server is reachable.
                                tray.update_state(TrayState::Syncing {
                                    current: stats.uploading as u32,
                                    total: active as u32,
                                });
                            } else if self.server_offline {
                                tray.update_state(TrayState::Offline);
                            } else {
                                tray.update_state(TrayState::Idle);
                            }
                        }
                    }

                    // Detect Syncing → Idle transition for notification.
                    if self.was_syncing && !is_syncing && stats.completed > 0 {
                        self.notifications
                            .notify_upload_complete(stats.completed as u32);
                    }
                    self.was_syncing = is_syncing;
                }
            }
        }
    }

    /// Periodically ping the Immich server to detect outages. Drives the
    /// Offline tray state.
    ///
    /// Cadence: every `PING_INTERVAL` (30s). One in-flight ping at a time ...
    /// `ping_rx` being Some means we're waiting on the previous one.
    fn maybe_schedule_ping(&mut self) {
        const PING_INTERVAL: Duration = Duration::from_secs(30);

        if self.ping_rx.is_some() {
            return; // a ping is already in flight
        }
        if self.last_ping_at.elapsed() < PING_INTERVAL {
            return;
        }
        let Some(ref client) = self.client else {
            return; // no client configured ... nothing to ping
        };

        self.last_ping_at = Instant::now();
        let client = client.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.ping_rx = Some(rx);
        self.runtime.spawn(async move {
            let ok = client.ping().await.unwrap_or(false);
            let _ = tx.send(ok);
        });
    }

    /// Poll the in-flight ping channel and update `server_offline`.
    fn poll_ping_result(&mut self) {
        let Some(rx) = self.ping_rx.as_ref() else {
            return;
        };
        let Ok(ok) = rx.try_recv() else {
            return;
        };
        self.ping_rx = None;

        if ok {
            if self.server_offline {
                info!("Server reachable again ... clearing Offline state");
            }
            self.server_offline = false;
        } else if !self.server_offline {
            warn!("Server ping failed ... switching to Offline state");
            self.server_offline = true;
        } else {
            self.server_offline = true;
        }

        // Reflect the new reachability in the tray menu's server line.
        if let Some(ref mut tray) = self.tray {
            let url_display = self
                .config
                .server
                .url
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            if !url_display.is_empty() {
                tray.set_server_status(url_display, !self.server_offline);
            }
        }
    }

    /// Reconcile the WatchEngine's view of watched folders against the DB.
    ///
    /// Background: the Settings window is a separate subprocess. It writes
    /// directly to the DB on `Remove Folder` / `Add Folder` clicks, but the
    /// main process only gets a config-update message when the subprocess
    /// exits. Between those two events, the engine watches a folder the
    /// user has already removed — files added there still upload. Issue #16.
    ///
    /// Fix: every `FOLDER_RECONCILE_INTERVAL`, compare the engine's watched
    /// set against the DB's enabled-folder set. If they differ, restart the
    /// engine to converge. Restart also re-spawns the watch→pipeline bridge
    /// with a refreshed folder map, so per-event folder-id lookup stays
    /// correct after additions.
    fn reconcile_watch_folders(&mut self) {
        const FOLDER_RECONCILE_INTERVAL: Duration = Duration::from_secs(2);

        if self.last_folder_reconcile.elapsed() < FOLDER_RECONCILE_INTERVAL {
            return;
        }
        self.converge_watch_folders();
    }

    /// One reconcile pass: restart the engine if its watched set differs
    /// from the DB's enabled folders whose paths exist.
    fn converge_watch_folders(&mut self) {
        self.last_folder_reconcile = Instant::now();

        let folders = {
            let db = match self.db.inner().lock() {
                Ok(g) => g,
                Err(_) => {
                    warn!("Folder reconcile: DB mutex poisoned, skipping");
                    return;
                }
            };
            match db.get_folders() {
                Ok(f) => f,
                Err(e) => {
                    warn!(error = %e, "Folder reconcile: get_folders failed, skipping");
                    return;
                }
            }
        };

        let engine_paths = match self.watch_engine.as_ref() {
            Some(e) => e.watched_paths(),
            None => HashSet::new(),
        };

        let Some(drift) = compute_watch_drift(&folders, &engine_paths) else {
            return;
        };

        info!(
            removed = drift.removed.len(),
            added = drift.added.len(),
            "Folder reconcile: drift detected, restarting watch engine to converge"
        );

        self.restart_watch_engine();
    }

    /// Stop every watcher and start the engine again, which re-runs the
    /// initial scan. Used by the folder reconcile and by Upload Now.
    fn restart_watch_engine(&mut self) {
        if let Some(ref mut engine) = self.watch_engine {
            engine.stop_all();
        }
        self.watch_engine = None;
        if let Err(e) = self.start_watch_engine() {
            warn!(error = %e, "restart_watch_engine failed");
        }
    }

    /// Turn volume mounts into watched folders and converge the engine at
    /// once rather than on the next reconcile tick.
    ///
    /// A mount whose decision is `Watch` gets an `auto_added` row (once; a
    /// known card keeps its row and its settings). Starting and stopping the
    /// watcher is the reconcile's job: it watches enabled rows whose path
    /// exists, so the card's folder starts when it mounts and stops when it
    /// goes. Unmounts therefore only force that reconcile.
    fn poll_volume_events(&mut self) {
        use crate::platform::volumes::{has_dcim, VolumeEvent};
        use crate::watch::device::{decide_mount, register_mount, MountDecision, Registered};

        let Some(rx) = self.volume_rx.as_ref() else {
            return;
        };
        let events: Vec<VolumeEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        if events.is_empty() {
            return;
        }

        for event in events {
            match event {
                VolumeEvent::Mounted {
                    root,
                    label,
                    id,
                    removable,
                } => {
                    let dcim = has_dcim(&root);
                    match decide_mount(&self.config.devices, &root, dcim, removable) {
                        MountDecision::Skip(reason) => {
                            debug!(root = %root.display(), %id, has_dcim = dcim, removable, reason, "Volume mounted, not watching");
                        }
                        MountDecision::Watch(folder) => {
                            let db = self.db.inner().lock().unwrap();
                            match register_mount(&db, &folder, label.as_deref()) {
                                Ok(Registered::Added(fid)) => {
                                    info!(folder = %folder.display(), %id, folder_id = fid, "Volume mounted, auto-added watch folder")
                                }
                                Ok(Registered::Existing(fid)) => {
                                    info!(folder = %folder.display(), %id, folder_id = fid, "Volume mounted, known folder")
                                }
                                Err(e) => {
                                    warn!(folder = %folder.display(), error = %e, "Volume mounted, failed to add watch folder")
                                }
                            }
                        }
                    }
                }
                VolumeEvent::Unmounted { root } => {
                    info!(root = %root.display(), "Volume unmounted");
                }
            }
        }

        // Converge now: a new row starts watching (with its initial scan),
        // a vanished path stops.
        self.converge_watch_folders();
    }

    /// Prune the logs directory against the retention cap, if enough time
    /// has elapsed since the last prune.
    fn maybe_prune_logs(&mut self) {
        const LOG_PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60); // hourly

        if self.last_log_prune.elapsed() < LOG_PRUNE_INTERVAL {
            return;
        }
        self.last_log_prune = Instant::now();

        let dir = match crate::config::Config::local_data_dir() {
            Ok(d) => d.join("logs"),
            Err(e) => {
                warn!(error = %e, "Log retention: cannot resolve local_data_dir");
                return;
            }
        };
        crate::platform::logs::prune_logs(&dir, crate::platform::logs::DEFAULT_LOG_CAP_BYTES);
    }

    /// Spawn an async update check on the tokio runtime.
    ///
    /// If `with_delay` is true, waits `STARTUP_DELAY` before checking.
    fn schedule_update_check(&mut self, with_delay: bool) {
        // Stamp first, so a skipped periodic check waits a full interval
        // instead of retrying (and logging) on every tick.
        self.last_update_check = Instant::now();
        if let Some(reason) = update_check_skip_reason(
            crate::platform::SELF_UPDATE_SUPPORTED,
            self.update_ready,
            self.update_download_rx.is_some() || self.update_rx.is_some(),
        ) {
            info!(reason, "Skipping update check");
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);

        let repo = self.config.advanced.update_repo.clone();
        let channel = self.config.advanced.update_channel.clone();
        self.runtime.spawn(async move {
            if with_delay {
                tokio::time::sleep(updater::STARTUP_DELAY).await;
            }
            let result = updater::check_for_update(&repo, &channel).await;
            let _ = tx.send(result);
        });
    }

    /// Poll the update check channel for results.
    ///
    /// When an update is found, auto-downloads it in the background instead of
    /// just showing a notification.
    fn poll_update_check(&mut self) {
        let result = match self.update_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            Some(r) => r,
            None => return,
        };
        // Done: no longer in flight (update_check_skip_reason reads this).
        self.update_rx = None;

        match result {
            UpdateCheckResult::Available(info) => {
                info!(
                    current = %info.current_version,
                    new = %info.new_version,
                    "Update available — starting background download"
                );

                // Show a downloading notification.
                self.notifications.show_toast_raw(
                    "Downloading Update",
                    &format!("Downloading ImmichSync v{}...", info.new_version),
                );

                // Kick off background download.
                let (tx, rx) = std::sync::mpsc::channel();
                self.update_download_rx = Some(rx);

                let info_clone = info.clone();
                self.runtime.spawn_blocking(move || {
                    match updater::download_and_apply(&info_clone) {
                        Ok(()) => {
                            let _ = tx.send(Ok(info_clone.new_version));
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e.to_string()));
                        }
                    }
                });

                self.pending_update = Some(info);
            }
            UpdateCheckResult::UpToDate => {
                info!("Already on latest version");
            }
            UpdateCheckResult::Failed(msg) => {
                warn!(error = %msg, "Update check failed");
            }
        }
    }

    /// Poll the background download channel for completion.
    fn poll_update_download(&mut self) {
        let result = match self
            .update_download_rx
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        {
            Some(r) => r,
            None => return,
        };
        // Done: no longer in flight (update_check_skip_reason reads this).
        self.update_download_rx = None;

        match result {
            Ok(version) => {
                info!(version = %version, "Background update downloaded and applied");
                self.update_ready = true;
                updater::note_update_applied();

                // Update tray menu to show "Restart to Update".
                if let Some(ref mut tray) = self.tray {
                    tray.set_restart_to_update(Some(&version));
                }

                self.notifications.show_toast_raw(
                    "Update Ready",
                    &match crate::platform::UPDATE_NOTE {
                        Some(note) => {
                            format!("ImmichSync v{version} is ready — restart to apply. {note}")
                        }
                        None => format!("ImmichSync v{version} is ready — restart to apply."),
                    },
                );
            }
            Err(e) => {
                warn!(error = %e, "Background update download failed");

                // Fall back to showing the manual "Update Available" item.
                if let Some(ref info) = self.pending_update {
                    if let Some(ref mut tray) = self.tray {
                        tray.set_update_available(Some(&info.new_version));
                    }
                }

                self.notifications
                    .notify_error(&format!("Update download failed: {e}"));
            }
        }
    }

    /// Open the update dialog as a subprocess.
    fn open_update_dialog(&mut self) {
        let Some(ref info) = self.pending_update else {
            info!("No pending update to show");
            return;
        };

        if self.window_open.update.swap(true, Ordering::SeqCst) {
            info!("Update dialog already open, ignoring");
            return;
        }

        // Serialize UpdateInfo to a temp JSON file.
        let temp_dir = std::env::temp_dir();
        let info_path = temp_dir.join("immichsync_update_info.json");
        let info_json = match serde_json::to_string(info) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, "Failed to serialize update info");
                self.window_open.update.store(false, Ordering::SeqCst);
                return;
            }
        };
        if let Err(e) = std::fs::write(&info_path, &info_json) {
            warn!(error = %e, "Failed to write update info file");
            self.window_open.update.store(false, Ordering::SeqCst);
            return;
        }

        let flag = self.window_open.update.clone();
        let info_path_str = info_path.display().to_string();
        // The relaunch happens on the main loop, after shutdown(), never on
        // this thread (#127).
        let actions = self.tray.as_ref().map(|t| t.action_sender());

        std::thread::spawn(move || {
            let exe = match crate::platform::install::installed_exe_path() {
                Ok(p) if p.exists() => p,
                _ => match std::env::current_exe() {
                    Ok(e) => e,
                    Err(e) => {
                        warn!(error = %e, "Failed to get current exe path");
                        flag.store(false, Ordering::SeqCst);
                        return;
                    }
                },
            };

            info!("Spawning update dialog subprocess");
            match std::process::Command::new(&exe)
                .args(["--window", "update", "--update-info", &info_path_str])
                .status()
            {
                Ok(status) => {
                    info!(?status, "Update dialog subprocess exited");
                    let action = action_for_update_dialog_exit(status.code());
                    if let (Some(action), Some(tx)) = (action, actions.as_ref()) {
                        let _ = tx.send(action);
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to spawn update dialog");
                }
            }
            flag.store(false, Ordering::SeqCst);
        });
    }

    /// An update is on disk and waits for a restart: show Restart to Update.
    fn mark_update_ready(&mut self, version: Option<&str>) {
        info!(?version, "Update applied; restart deferred");
        self.update_ready = true;
        // Windows (Settings, logs) keep running the previous image until the
        // restart; the dialog applied this update in another process, so
        // this process's flag isn't set yet (#128).
        updater::note_update_applied();
        if let Some(ref mut tray) = self.tray {
            tray.set_restart_to_update(Some(version.unwrap_or("new version")));
        }
    }

    /// Stop all background work, waiting briefly for the upload worker to
    /// quiesce. Used by the relaunch path so the new process can take
    /// ownership of `state.db` without fighting a still-flushing writer.
    fn shutdown(&mut self) {
        info!("Shutting down (graceful)");

        if let Some(mut monitor) = self.volume_monitor.take() {
            monitor.stop();
        }

        if let Some(ref mut engine) = self.watch_engine {
            engine.stop_all();
        }

        if let Some(mut pipeline) = self.pipeline.take() {
            let rt = self.runtime.clone();
            rt.block_on(async {
                pipeline.stop().await;
            });
        }

        info!("Shutdown complete");
    }

    /// Immediate-exit path for user-initiated Quit and WM_QUIT.
    ///
    /// Signals the watch engine and upload worker to stop but does NOT wait
    /// for in-flight uploads. Calls `std::process::exit(0)` so the user sees
    /// the tray disappear instantly instead of the 3s wait pipeline.stop()
    /// would impose. In-flight `"uploading"` queue rows are recovered by
    /// `reset_stale_uploading` on next launch — partial uploads will be
    /// retried, no data is lost.
    fn shutdown_and_exit(&mut self) -> ! {
        info!("Immediate exit requested ... dropping in-flight work");

        if let Some(ref mut engine) = self.watch_engine {
            engine.stop_all();
        }

        if let Some(pipeline) = self.pipeline.take() {
            pipeline.signal_stop();
        }

        // Skip destructors. The single-instance mutex, SQLite handles, and
        // HTTP sockets are all reaped cleanly by the OS on process exit.
        // SQLite's WAL mode + the queue's reset_stale_uploading on next
        // launch make this safe.
        std::process::exit(0);
    }
}

// ─── Watch → Pipeline bridge ─────────────────────────────────────────────────

/// Async task that reads watch events and submits new files to the upload queue.
async fn bridge_watch_to_pipeline(
    mut event_rx: tokio::sync::mpsc::Receiver<WatchEvent>,
    store: Arc<dyn QueueStore>,
    concurrency: usize,
    folder_map: Arc<std::collections::HashMap<std::path::PathBuf, i64>>,
) {
    let queue = UploadQueue::new(store, concurrency);

    while let Some(event) = event_rx.recv().await {
        match event {
            WatchEvent::FileReady(path) => {
                info!(path = %path.display(), "Watch: file ready");
                let folder_id = resolve_folder_id(&path, &folder_map);
                match queue.process_file(path, folder_id) {
                    Ok(Some(id)) => info!(id, "File enqueued"),
                    Ok(None) => info!("File already uploaded, skipping"),
                    Err(e) => warn!(error = %e, "Failed to process file"),
                }
            }
            WatchEvent::FileRemoved(path) => {
                info!(path = %path.display(), "Watch: file removed (ignoring)");
            }
            WatchEvent::Error(msg) => {
                warn!(error = %msg, "Watch engine error");
            }
        }
    }
    info!("Watch→pipeline bridge stopped");
}

/// One-time initial scan of all watched folders.
///
/// Walks each directory tree recursively, filters files using [`FileFilter`],
/// and enqueues any that haven't already been uploaded.  Runs as a background
/// task so it doesn't block the main loop or the real-time watcher.
async fn initial_scan(
    store: Arc<dyn QueueStore>,
    folder_map: Arc<std::collections::HashMap<std::path::PathBuf, i64>>,
) {
    info!("Initial scan: starting");

    let store2 = store.clone();
    let folder_map2 = folder_map.clone();

    // Build each folder's filter the same way the live watcher does
    // (patterns + online-files flag, #97). Read up front so we don't hold
    // the DB mutex while walking the trees.
    let folder_filters: std::collections::HashMap<i64, FileFilter> = {
        let mut map = std::collections::HashMap::new();
        for &id in folder_map.values() {
            let filter = match store.get_folder(id) {
                Ok(Some(f)) => FileFilter::for_folder(&f),
                // Unreadable row: the defaults (online files skipped) are the
                // safe choice.
                _ => FileFilter::new(),
            };
            map.insert(id, filter);
        }
        map
    };

    let result = tokio::task::spawn_blocking(move || {
        let queue = UploadQueue::new(store2, 2);
        let mut scanned: u64 = 0;
        let mut enqueued: u64 = 0;
        let mut skipped: u64 = 0;
        let mut errors: u64 = 0;

        for (folder_path, &folder_id) in folder_map2.as_ref() {
            info!(path = %folder_path.display(), "Initial scan: scanning folder");
            let filter = folder_filters
                .get(&folder_id)
                .cloned()
                .unwrap_or_else(FileFilter::new);
            let mut dirs = vec![folder_path.clone()];

            while let Some(dir) = dirs.pop() {
                let entries = match std::fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(e) => {
                        warn!(path = %dir.display(), error = %e, "Initial scan: cannot read directory");
                        continue;
                    }
                };

                for entry in entries {
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => {
                            warn!(error = %e, "Initial scan: directory entry error");
                            continue;
                        }
                    };

                    let path = entry.path();

                    // Recurse into subdirectories, except trash, volume
                    // housekeeping and packages (#96).
                    if path.is_dir() {
                        if crate::watch::filter::is_skipped_dir(&path) {
                            continue;
                        }
                        dirs.push(path);
                        continue;
                    }

                    if !path.is_file() {
                        continue;
                    }

                    if !filter.should_include(&path) {
                        continue;
                    }

                    scanned += 1;

                    match queue.process_file(path, Some(folder_id)) {
                        Ok(Some(_id)) => enqueued += 1,
                        Ok(None) => skipped += 1,
                        Err(e) => {
                            errors += 1;
                            warn!(error = %e, "Initial scan: failed to process file");
                        }
                    }

                    // Log progress every 100 files.
                    if scanned.is_multiple_of(100) {
                        info!(scanned, enqueued, skipped, "Initial scan: progress");
                    }
                }
            }
        }

        (scanned, enqueued, skipped, errors)
    })
    .await;

    match result {
        Ok((scanned, enqueued, skipped, errors)) => {
            info!(scanned, enqueued, skipped, errors, "Initial scan: complete");
        }
        Err(e) => {
            warn!(error = %e, "Initial scan task panicked");
        }
    }
}

/// Spawn a UI window as a child process.
///
/// Each child process runs `immichsync.exe --window <type>`, giving it its
/// own winit EventLoop.  A background thread waits for the subprocess to
/// exit, then clears the `open_flag` and optionally reloads config from
/// disk (for settings).
fn spawn_window_subprocess(
    window_type: &'static str,
    open_flag: Arc<AtomicBool>,
    pid_slot: Option<Arc<std::sync::atomic::AtomicU32>>,
    config_tx: Option<std::sync::mpsc::Sender<crate::config::Config>>,
) {
    // Prefer the installed exe path so subprocesses always run from the
    // stable install location, falling back to current_exe if unavailable.
    let exe = match crate::platform::install::installed_exe_path() {
        Ok(p) if p.exists() => p,
        _ => match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "Failed to get current exe path");
                open_flag.store(false, Ordering::SeqCst);
                return;
            }
        },
    };
    // After an update is applied, windows run the previous image until the
    // restart, so they match the running tray (#128).
    let exe = updater::window_exe(exe);

    std::thread::spawn(move || {
        info!(window_type, "Spawning window subprocess");
        let status = std::process::Command::new(&exe)
            .args(["--window", window_type])
            .spawn()
            .and_then(|mut child| {
                if let Some(ref slot) = pid_slot {
                    slot.store(child.id(), Ordering::SeqCst);
                }
                child.wait()
            });
        if let Some(ref slot) = pid_slot {
            slot.store(0, Ordering::SeqCst);
        }
        match status {
            Ok(status) => {
                info!(window_type, ?status, "Window subprocess exited");
                // For settings, reload config from disk after the subprocess exits.
                if let Some(tx) = config_tx {
                    if let Ok(config) = crate::config::Config::load() {
                        let _ = tx.send(config);
                    }
                }
            }
            Err(e) => {
                warn!(window_type, error = %e, "Failed to spawn window subprocess");
            }
        }
        open_flag.store(false, Ordering::SeqCst);
    });
}

/// Why an update check should not run now, or `None` to run it.
///
/// Once an update is applied the process still reports the old version, so
/// a check would find the same release and apply it again; on macOS that
/// deletes the parked bundle this process runs from (#126). A check while
/// another check or a download is in flight would race it: two `Available`
/// results start two downloads.
fn update_check_skip_reason(
    supported: bool,
    update_ready: bool,
    busy: bool,
) -> Option<&'static str> {
    if !supported {
        Some("no self-update asset is published for this platform")
    } else if update_ready {
        Some("an update is already applied; waiting for restart")
    } else if busy {
        Some("an update check or download is in progress")
    } else {
        None
    }
}

/// What the main loop should do after the update dialog exits. Only
/// Restart Now restarts; Later and a window close after applying offer
/// Restart to Update; anything else changed nothing (#127).
fn action_for_update_dialog_exit(code: Option<i32>) -> Option<TrayAction> {
    match code {
        Some(crate::ui::update::EXIT_RESTART_NOW) => Some(TrayAction::UpdateAppliedRestart),
        Some(crate::ui::update::EXIT_APPLIED_LATER) => Some(TrayAction::UpdateAppliedLater),
        _ => None,
    }
}

/// Find the watched folder that contains `file_path` by walking up the
/// directory tree and checking against the canonical folder map.
fn resolve_folder_id(
    file_path: &std::path::Path,
    folder_map: &std::collections::HashMap<std::path::PathBuf, i64>,
) -> Option<i64> {
    let canonical = std::fs::canonicalize(file_path).unwrap_or_else(|_| file_path.to_path_buf());
    let mut dir = canonical.parent();
    while let Some(d) = dir {
        if let Some(&id) = folder_map.get(d) {
            return Some(id);
        }
        dir = d.parent();
    }
    None
}

// ─── Watch-folder drift detection ────────────────────────────────────────────

/// What changed between the DB's view of watched folders and the engine's.
///
/// `removed` = paths the engine currently watches that the DB no longer wants.
/// `added`   = paths the DB wants watched that the engine isn't watching yet.
///
/// Returned by [`compute_watch_drift`] when (and only when) the two sets diverge
/// in a way that warrants restarting the engine. A "diverge" that's entirely
/// explained by enabled-but-missing-on-disk DB rows is treated as no drift,
/// because [`App::start_watch_engine`] would just skip those paths again on
/// restart and we'd loop forever. Issue #49.
struct WatchDrift {
    removed: Vec<PathBuf>,
    added: Vec<PathBuf>,
}

/// Compare the DB's enabled+existing folders against the engine's watched set.
///
/// Returns `Some(drift)` when restart is warranted, `None` otherwise. Filters
/// `enabled && path.exists()` so we don't fight `start_watch_engine`'s own
/// skip semantics, which would otherwise produce a permanent drift signal for
/// any stale missing-path row in `watched_folders` (the regression in #49).
fn compute_watch_drift(
    folders: &[crate::db::WatchedFolder],
    engine_paths: &HashSet<PathBuf>,
) -> Option<WatchDrift> {
    let db_paths: HashSet<PathBuf> = folders
        .iter()
        .filter(|f| f.enabled)
        .filter(|f| std::path::Path::new(&f.path).exists())
        .map(|f| {
            let p = PathBuf::from(&f.path);
            std::fs::canonicalize(&p).unwrap_or(p)
        })
        .collect();

    if &db_paths == engine_paths {
        return None;
    }

    let removed: Vec<_> = engine_paths.difference(&db_paths).cloned().collect();
    let added: Vec<_> = db_paths.difference(engine_paths).cloned().collect();
    Some(WatchDrift { removed, added })
}

/// Emit the "Watch folder missing, skipping" log exactly once per (path,
/// process lifetime). Subsequent skip events for the same path drop to TRACE,
/// keeping the log file readable when reconcile cycles repeatedly retry a
/// folder whose path doesn't exist (e.g. an SD card that isn't mounted).
fn warn_missing_folder_once(seen: &mut HashSet<PathBuf>, path: &std::path::Path) {
    let key = path.to_path_buf();
    if seen.insert(key) {
        warn!(
            path = %path.display(),
            "Watch folder missing, skipping (will not warn again this launch unless the path reappears)"
        );
    } else {
        trace!(path = %path.display(), "Watch folder still missing, skipping (throttled)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{AlbumMode, PostUpload, WatchMode, WatchedFolder};

    fn make_folder(id: i64, path: &str, enabled: bool) -> WatchedFolder {
        WatchedFolder {
            id,
            path: path.to_string(),
            label: None,
            enabled,
            watch_mode: WatchMode::Native,
            poll_interval_secs: 30,
            album_mode: AlbumMode::None,
            album_name: None,
            include_patterns: None,
            exclude_patterns: None,
            post_upload: PostUpload::Keep,
            ignore_online_files: true,
            auto_added: false,
            created_at: "2026-05-26T00:00:00Z".to_string(),
            updated_at: "2026-05-26T00:00:00Z".to_string(),
        }
    }

    /// Issue #49 regression test: a DB row whose path doesn't exist on disk
    /// must not produce a drift signal when the engine's watched set already
    /// matches the existing-only subset of enabled DB rows.
    #[test]
    fn missing_path_does_not_register_as_drift() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let existing = tmp.path().to_path_buf();
        let canon = std::fs::canonicalize(&existing).unwrap_or(existing.clone());

        let folders = vec![
            make_folder(1, existing.to_string_lossy().as_ref(), true),
            // Use a clearly-fictional path that won't accidentally exist.
            make_folder(2, "C:\\does-not-exist-49\\fake\\Pictures", true),
        ];

        let mut engine_paths = HashSet::new();
        engine_paths.insert(canon);

        assert!(
            compute_watch_drift(&folders, &engine_paths).is_none(),
            "drift should be None when the only DB-vs-engine diff is a missing-path row"
        );
    }

    /// Inverse of the above: a real new folder added to the DB still triggers
    /// a drift signal so PR #25's Settings → Remove convergence keeps working.
    #[test]
    fn update_checks_stop_once_an_update_is_applied_or_downloading() {
        assert_eq!(update_check_skip_reason(true, false, false), None);
        assert!(update_check_skip_reason(true, true, false).is_some());
        assert!(update_check_skip_reason(true, false, true).is_some());
        assert!(update_check_skip_reason(false, false, false).is_some());
    }

    #[test]
    fn only_restart_now_restarts_after_the_update_dialog() {
        assert!(matches!(
            action_for_update_dialog_exit(Some(0)),
            Some(TrayAction::UpdateAppliedRestart)
        ));
        assert!(matches!(
            action_for_update_dialog_exit(Some(2)),
            Some(TrayAction::UpdateAppliedLater)
        ));
        assert!(action_for_update_dialog_exit(Some(1)).is_none());
        assert!(action_for_update_dialog_exit(None).is_none());
    }

    #[test]
    fn newly_added_real_folder_registers_as_drift() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let folders = vec![
            make_folder(1, a.to_string_lossy().as_ref(), true),
            make_folder(2, b.to_string_lossy().as_ref(), true),
        ];

        // Engine only watches `a`. Adding `b` to the DB should drift.
        let mut engine_paths = HashSet::new();
        engine_paths.insert(std::fs::canonicalize(&a).unwrap_or(a.clone()));

        let drift = compute_watch_drift(&folders, &engine_paths).expect("real add should drift");
        assert_eq!(drift.added.len(), 1);
        assert_eq!(drift.removed.len(), 0);
    }

    /// Disabled rows in the DB must not register as drift even when the path
    /// is fine — Settings → Disable should converge by removing the path from
    /// the engine's set, not by leaving a phantom add.
    #[test]
    fn disabled_folder_is_treated_as_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().to_path_buf();
        let canon = std::fs::canonicalize(&p).unwrap_or(p.clone());

        // DB has one enabled row pointing at `p`.
        let enabled_only = vec![make_folder(1, p.to_string_lossy().as_ref(), true)];
        let mut engine_paths = HashSet::new();
        engine_paths.insert(canon.clone());
        assert!(compute_watch_drift(&enabled_only, &engine_paths).is_none());

        // Flip the row to disabled — drift now says "remove p".
        let disabled = vec![make_folder(1, p.to_string_lossy().as_ref(), false)];
        let drift = compute_watch_drift(&disabled, &engine_paths)
            .expect("disabling a watched folder should drift");
        assert_eq!(drift.removed.len(), 1);
        assert_eq!(drift.added.len(), 0);
    }

    #[test]
    fn warn_missing_folder_once_dedups_by_path() {
        let mut seen = HashSet::new();
        let p = std::path::Path::new("C:\\fake\\Pictures");
        // First call: would emit WARN.
        warn_missing_folder_once(&mut seen, p);
        assert_eq!(seen.len(), 1);
        // Second call same path: no-op (silent TRACE).
        warn_missing_folder_once(&mut seen, p);
        assert_eq!(seen.len(), 1);
        // Different path: emits, set grows.
        warn_missing_folder_once(&mut seen, std::path::Path::new("D:\\fake\\Videos"));
        assert_eq!(seen.len(), 2);
    }
}

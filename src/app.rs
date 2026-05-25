// Application state and lifecycle management.
//
// `App` wires together the upload pipeline, watch engine, system tray, and
// settings UI.  The main loop is a native Win32 message pump — simpler and
// more reliable for a tray-only app than running a full winit event loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

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
}

/// Tracks whether each type of UI window is currently open, preventing
/// multiple instances of the same window from being spawned.
#[derive(Clone)]
pub struct WindowOpenTracker {
    pub settings: Arc<AtomicBool>,
    pub about: Arc<AtomicBool>,
    pub upload_log: Arc<AtomicBool>,
    pub trash_log: Arc<AtomicBool>,
    pub update: Arc<AtomicBool>,
}

impl WindowOpenTracker {
    fn new() -> Self {
        Self {
            settings: Arc::new(AtomicBool::new(false)),
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

        // Create system tray (must be on main thread).
        let (tray, tray_rx) =
            TrayApp::new().map_err(|e| anyhow::anyhow!("Failed to create tray: {}", e))?;
        self.tray = Some(tray);
        self.tray_rx = Some(tray_rx);

        // Create and start the upload pipeline if server is configured.
        if let Some(ref client) = self.client {
            let uploader: Arc<dyn AssetUploader> = Arc::new(client.clone());
            let store: Arc<dyn QueueStore> = self.db.clone();
            let mut pipeline =
                UploadPipeline::new(store, &self.config.server, &self.config.upload, uploader);

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
        } else {
            info!("No server configured; pipeline not started");
            if let Some(ref mut tray) = self.tray {
                tray.update_state(TrayState::Error("Server not configured".to_string()));
            }
        }

        // Set up the watch engine and bridge task.
        self.start_watch_engine()?;

        // Schedule the first automatic update check after a startup delay.
        if self.config.advanced.check_for_updates {
            self.schedule_update_check(true);
        }

        Ok(())
    }

    /// Run the main message loop. Blocks until the user selects Quit.
    ///
    /// Uses a native Win32 message pump so tray-icon's hidden window
    /// receives all its messages correctly (right-click menu, etc.).
    pub fn run(&mut self) {
        info!("Entering main message loop");

        loop {
            // Pump all pending Windows messages. This is what makes the
            // tray icon's context menu work — tray-icon creates a hidden
            // window that needs message dispatch to show the popup menu.
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).into() {
                    if msg.message == WM_QUIT {
                        info!("WM_QUIT received");
                        self.shutdown();
                        return;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
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
                self.shutdown();
                return;
            }

            // Check for config updates from the settings window.
            if let Some(ref rx) = self.settings_rx {
                if let Ok(new_config) = rx.try_recv() {
                    self.apply_new_config(new_config);
                }
            }

            // Poll for update check results.
            self.poll_update_check();

            // Poll for background download completion.
            self.poll_update_download();

            // Periodic re-check for updates.
            let check_interval = updater::check_interval_from_hours(
                self.config.advanced.update_check_interval_hours,
            );
            if self.config.advanced.check_for_updates
                && self.last_update_check.elapsed() >= check_interval
            {
                self.schedule_update_check(false);
            }

            // Periodically refresh tray with queue statistics.
            self.update_tray_stats();

            // Periodically reconcile the WatchEngine's view of watched folders
            // against the DB. The Settings subprocess mutates the DB without
            // notifying the main process until it exits — until reconcile fires,
            // a folder removed via Settings would still be watched (and its files
            // uploaded). Cheap when there's no drift; restarts the engine when
            // there is.
            self.reconcile_watch_folders();

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
            let folders = db.get_folders()?;

            if folders.is_empty() {
                drop(db);
                // First run: add the user's Pictures folder as a default.
                if let Ok(pictures) = crate::platform::known_folders::get_pictures_folder() {
                    if pictures.exists() {
                        info!(path = %pictures.display(), "Adding default Pictures folder");
                        let db = self.db.inner().lock().unwrap();
                        if let Ok(id) =
                            db.add_folder(&pictures.display().to_string(), Some("Pictures"), true)
                        {
                            // Default Pictures folder has no per-folder patterns.
                            let filter = FileFilter::new();
                            let canon = std::fs::canonicalize(&pictures)
                                .unwrap_or_else(|_| pictures.clone());
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
                }
            } else {
                for folder in &folders {
                    if !folder.enabled {
                        continue;
                    }
                    let path = std::path::PathBuf::from(&folder.path);
                    if !path.exists() {
                        warn!(path = %folder.path, "Watch folder missing, skipping");
                        continue;
                    }
                    let is_network = folder.watch_mode == crate::db::WatchMode::Poll;
                    let includes = crate::watch::filter::parse_patterns_json(
                        folder.include_patterns.as_deref(),
                    );
                    let excludes = crate::watch::filter::parse_patterns_json(
                        folder.exclude_patterns.as_deref(),
                    );
                    let filter = FileFilter::new()
                        .with_include_patterns(includes)
                        .with_exclude_patterns(excludes)
                        .with_ignore_online_files(folder.ignore_online_files);
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
                    info!("Settings window already open, ignoring");
                    return;
                }
                let (tx, rx) = std::sync::mpsc::channel();
                self.settings_rx = Some(rx);
                let flag = self.window_open.settings.clone();
                spawn_window_subprocess("settings", flag, Some(tx));
            }
            TrayAction::UploadNow => {
                info!("Upload Now requested");
            }
            TrayAction::About => {
                if self.window_open.about.swap(true, Ordering::SeqCst) {
                    info!("About window already open, ignoring");
                    return;
                }
                let flag = self.window_open.about.clone();
                spawn_window_subprocess("about", flag, None);
            }
            TrayAction::ViewLog => {
                if self.window_open.upload_log.swap(true, Ordering::SeqCst) {
                    info!("Upload Log window already open, ignoring");
                    return;
                }
                let flag = self.window_open.upload_log.clone();
                spawn_window_subprocess("log", flag, None);
            }
            TrayAction::ViewTrash => {
                if self.window_open.trash_log.swap(true, Ordering::SeqCst) {
                    info!("Trash Log window already open, ignoring");
                    return;
                }
                let flag = self.window_open.trash_log.clone();
                spawn_window_subprocess("trash-log", flag, None);
            }
            TrayAction::CheckForUpdates => {
                info!("Manual update check requested");
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

        // Apply the new config.
        self.config = new_config;
        self.notifications = crate::ui::notifications::Notifications::new(&self.config);

        if server_changed {
            info!("Server config changed, recreating client");

            // Recreate the Immich client.
            if !self.config.server.url.is_empty() && !self.config.server.api_key.is_empty() {
                match ImmichClient::new(&self.config.server.url, &self.config.server.api_key) {
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
                                tray.update_state(TrayState::Syncing {
                                    current: stats.uploading as u32,
                                    total: active as u32,
                                });
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
        self.last_folder_reconcile = Instant::now();

        // What the DB says should be watched.
        let db_paths: std::collections::HashSet<std::path::PathBuf> = {
            let db = match self.db.inner().lock() {
                Ok(g) => g,
                Err(_) => {
                    warn!("Folder reconcile: DB mutex poisoned, skipping");
                    return;
                }
            };
            match db.get_folders() {
                Ok(folders) => folders
                    .into_iter()
                    .filter(|f| f.enabled)
                    .map(|f| {
                        let p = std::path::PathBuf::from(&f.path);
                        std::fs::canonicalize(&p).unwrap_or(p)
                    })
                    .collect(),
                Err(e) => {
                    warn!(error = %e, "Folder reconcile: get_folders failed, skipping");
                    return;
                }
            }
        };

        // What the engine actually watches.
        let engine_paths = match self.watch_engine.as_ref() {
            Some(e) => e.watched_paths(),
            None => std::collections::HashSet::new(),
        };

        if db_paths == engine_paths {
            return;
        }

        let removed: Vec<_> = engine_paths.difference(&db_paths).cloned().collect();
        let added: Vec<_> = db_paths.difference(&engine_paths).cloned().collect();
        info!(
            removed = removed.len(),
            added = added.len(),
            "Folder reconcile: drift detected, restarting watch engine to converge"
        );

        if let Some(ref mut engine) = self.watch_engine {
            engine.stop_all();
        }
        self.watch_engine = None;
        if let Err(e) = self.start_watch_engine() {
            warn!(error = %e, "Folder reconcile: restart_watch_engine failed");
        }
    }

    /// Spawn an async update check on the tokio runtime.
    ///
    /// If `with_delay` is true, waits `STARTUP_DELAY` before checking.
    fn schedule_update_check(&mut self, with_delay: bool) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        self.last_update_check = Instant::now();

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

        match result {
            Ok(version) => {
                info!(version = %version, "Background update downloaded and applied");
                self.update_ready = true;

                // Update tray menu to show "Restart to Update".
                if let Some(ref mut tray) = self.tray {
                    tray.set_restart_to_update(Some(&version));
                }

                self.notifications.show_toast_raw(
                    "Update Ready",
                    &format!("ImmichSync v{version} is ready — restart to apply."),
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
                    if status.code() == Some(0) {
                        // Update was applied — relaunch.
                        info!("Update applied, relaunching");
                        updater::relaunch_self();
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to spawn update dialog");
                }
            }
            flag.store(false, Ordering::SeqCst);
        });
    }

    /// Stop all background work.
    fn shutdown(&mut self) {
        info!("Shutting down");

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
    use std::path::PathBuf;

    info!("Initial scan: starting");

    let store2 = store.clone();
    let folder_map2 = folder_map.clone();

    // Capture per-folder `ignore_online_files` flags so the scan filter
    // matches the live watcher's behavior. Read once up front so we don't
    // hold the DB mutex while walking the trees.
    let folder_ignore_online: std::collections::HashMap<i64, bool> = {
        let store_ref = store.clone();
        let mut map = std::collections::HashMap::new();
        for (_path, &id) in folder_map.as_ref() {
            match store_ref.get_folder(id) {
                Ok(Some(f)) => {
                    map.insert(id, f.ignore_online_files);
                }
                _ => {
                    // Default ON if we can't read it ... safe choice.
                    map.insert(id, true);
                }
            }
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
            let ignore_online = folder_ignore_online.get(&folder_id).copied().unwrap_or(true);
            let filter = FileFilter::new().with_ignore_online_files(ignore_online);
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

                    // Recurse into subdirectories, skipping trash.
                    if path.is_dir() {
                        if path.file_name().map_or(false, |n| {
                            n == crate::upload::worker::TRASH_DIR_NAME
                        }) {
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
                    if scanned % 100 == 0 {
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

    std::thread::spawn(move || {
        info!(window_type, "Spawning window subprocess");
        match std::process::Command::new(&exe)
            .args(["--window", window_type])
            .status()
        {
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

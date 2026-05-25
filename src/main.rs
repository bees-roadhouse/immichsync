#![windows_subsystem = "windows"]

mod app;
mod config;
mod db;
mod updater;

mod api;
mod platform;
mod ui;
mod upload;
mod watch;

use std::sync::Arc;

use tracing::info;

/// Direct file-based debug log that bypasses the non_blocking tracing buffer.
/// This ensures messages are written even if the process exits abruptly.
fn debug_log(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("immichsync_debug.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(
            f,
            "[{}] [pid={}] {msg}",
            chrono::Local::now().format("%H:%M:%S%.3f"),
            std::process::id(),
        );
    }
}

fn main() -> anyhow::Result<()> {
    debug_log(&format!(
        "=== ImmichSync starting === version={} args={:?}",
        env!("CARGO_PKG_VERSION"),
        std::env::args().collect::<Vec<_>>()
    ));

    // ── Legacy data migration ────────────────────────────────────────────
    // Two-step migration covers every install we've shipped:
    //  1. Very-old %APPDATA%\ImmichSync\ (pre-namespace) → roaming config +
    //     local state under bees-roadhouse\immichsync\.
    //  2. Mixed %APPDATA%\bees-roadhouse\immichsync\ (binary + DB + logs all
    //     in roaming) → Windows-standard split: binary in
    //     %LOCALAPPDATA%\Programs\immichsync\, config stays in roaming,
    //     DB + logs move to %LOCALAPPDATA%\bees-roadhouse\immichsync\.
    if let Err(e) = platform::migrate_legacy_data() {
        // Non-fatal: log to stderr since tracing isn't up yet.
        eprintln!("Warning: legacy data migration failed: {e}");
    }
    if let Err(e) = platform::migrate_to_split_layout() {
        eprintln!("Warning: split-layout migration failed: {e}");
    }

    // Clean up leftover .exe.old from a previous self-update.
    updater::cleanup_old_exe();

    // ── Logging ──────────────────────────────────────────────────────────
    let log_dir = config::Config::local_data_dir()?.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let file_appender = tracing_appender::rolling::daily(&log_dir, "immichsync.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    // ── Subprocess window mode ──────────────────────────────────────────
    // When launched with `--window <type>`, run only that UI window and
    // exit.  Each subprocess gets its own winit EventLoop, avoiding the
    // "EventLoop can't be recreated" limitation of winit 0.30.
    //
    // IMPORTANT: This must come before the install check, otherwise
    // `--window install` would try to show the install dialog recursively.
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 2 && args[1] == "--window" {
        debug_log(&format!("Subprocess mode: --window {}", &args[2]));
        return run_window_subprocess(&args[2]);
    }

    // ── Uninstall mode ──────────────────────────────────────────────────
    // Launched by Windows Apps & Features (Uninstall click) or by `winget
    // uninstall ImmichSync`. Removes the binary + shortcuts + autostart +
    // Uninstall registry block. Preserves user data (config.toml + DB +
    // logs) so reinstall picks up where the user left off.
    if args.iter().any(|a| a == "--uninstall") {
        let silent = args.iter().any(|a| a == "--silent");
        debug_log(&format!("Uninstall mode: silent={silent}"));
        info!(silent, "Running uninstall");
        if let Err(e) = platform::uninstall::run(silent) {
            tracing::error!(error = %e, "Uninstall failed");
            return Err(e);
        }
        return Ok(());
    }

    // ── Install dialog + relaunch ────────────────────────────────────────
    // If not running from the installed location, show an install/update
    // dialog as a subprocess. The subprocess handles killing any running
    // instances and copying the exe (with rename-dance fallback).
    //
    // We call AllowSetForegroundWindow before spawning so the subprocess
    // can bring its window to the foreground on Windows.
    let current_exe_path = std::env::current_exe().unwrap_or_default();
    debug_log(&format!(
        "Install check: current_exe={}",
        current_exe_path.display()
    ));
    match platform::is_running_installed() {
        Ok(false) => {
            debug_log("is_running_installed=false");
            // Check if user previously chose portable mode.
            let portable = config::Config::load()
                .map(|c| c.ui.portable_mode)
                .unwrap_or(false);

            if portable {
                debug_log("Portable mode, skipping install");
                info!("Portable mode enabled, skipping install");
            } else {
                let installed_exe = platform::installed_exe_path().ok();
                let installed_exists = installed_exe.as_ref().is_some_and(|p| p.exists());
                debug_log(&format!(
                    "installed_exe={:?}, exists={installed_exists}",
                    installed_exe
                ));

                if installed_exists {
                    // Determine if the installed copy needs updating.
                    let installed_ver = platform::install::installed_version();
                    let update_info = match &installed_ver {
                        None => Some((
                            String::from("unknown"),
                            platform::install::running_version().to_string(),
                        )),
                        Some(_) => platform::install::is_update_available(),
                    };
                    debug_log(&format!(
                        "installed_ver={installed_ver:?}, update_info={update_info:?}"
                    ));

                    if let Some((old_ver, new_ver)) = update_info {
                        info!(
                            from = %old_ver, to = %new_ver,
                            "Newer version running, spawning install-update dialog"
                        );
                        debug_log(&format!(
                            "Spawning install-update dialog: {old_ver} -> {new_ver}"
                        ));
                        match spawn_install_dialog(&[
                            "--window",
                            "install-update",
                            "--old-version",
                            &old_ver,
                        ]) {
                            Some(0) => {
                                // The subprocess already relaunched from the installed path
                                // and killed the old instance + parent. Just exit.
                                debug_log("Install dialog returned 0 (installed), exiting");
                                info!("Update complete, subprocess handled relaunch");
                                return Ok(());
                            }
                            other => {
                                debug_log(&format!(
                                    "Install dialog returned {other:?}, continuing"
                                ));
                                info!("User declined update, continuing from current location");
                            }
                        }
                    } else {
                        // Same version already installed — silently relaunch.
                        debug_log("Same version installed, relaunching");
                        info!("Same version already installed, relaunching from installed path");
                        if let Err(e) = platform::relaunch_installed() {
                            debug_log(&format!("Relaunch failed: {e}"));
                            tracing::warn!(error = %e, "Relaunch failed, continuing from current location");
                        }
                    }
                } else {
                    // No installed copy — show fresh install dialog.
                    debug_log("No installed exe, spawning fresh install dialog");
                    info!("Not running from installed path, spawning install dialog");
                    match spawn_install_dialog(&["--window", "install"]) {
                        Some(0) => {
                            // The subprocess already relaunched from the installed path.
                            debug_log("Install dialog returned 0 (installed), exiting");
                            info!("Install complete, subprocess handled relaunch");
                            return Ok(());
                        }
                        other => {
                            debug_log(&format!(
                                "Install dialog returned {other:?}, continuing portable"
                            ));
                            info!("User chose portable mode, continuing from current location");
                        }
                    }
                }
            }
        }
        Err(e) => {
            debug_log(&format!("is_running_installed error: {e}"));
            tracing::warn!(error = %e, "Could not check install status, continuing");
        }
        Ok(true) => {
            debug_log("is_running_installed=true, checking for updates");
            // Running from installed location — check for updates before starting the tray app.
            info!("Running from installed path, checking for updates");
            check_for_update_on_startup();
        }
    }

    info!("ImmichSync starting");

    // ── Single-instance check ────────────────────────────────────────────
    debug_log("Acquiring single-instance mutex");
    let _instance = match platform::SingleInstance::acquire() {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            debug_log("Another instance is running, exiting");
            return Ok(());
        }
        Err(e) => {
            tracing::error!("Single-instance check failed: {}", e);
            return Err(e.into());
        }
    };

    // ── Config ───────────────────────────────────────────────────────────
    let mut config = config::Config::load()?;
    info!("Config loaded");

    // ── First-run wizard ────────────────────────────────────────────────
    // Runs as a child process so it gets its own winit EventLoop, leaving
    // the main process free to spawn further UI windows later.
    if config.server.url.is_empty() {
        info!("Server not configured, launching first-run wizard");
        let exe = platform::installed_exe_path()
            .map(|p| {
                if p.exists() {
                    p
                } else {
                    std::env::current_exe().unwrap_or(p)
                }
            })
            .unwrap_or_else(|_| std::env::current_exe().expect("current_exe"));
        let status = std::process::Command::new(&exe)
            .args(["--window", "wizard"])
            .status();
        match status {
            Ok(s) if s.success() => {
                // Reload config — the wizard saves to disk on completion.
                config = config::Config::load()?;
                if config.server.url.is_empty() {
                    info!("First-run wizard cancelled, continuing with defaults");
                } else {
                    info!("First-run wizard completed");
                }
            }
            Ok(s) => {
                tracing::warn!(code = ?s.code(), "First-run wizard exited abnormally");
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to launch first-run wizard");
            }
        }
    }

    // ── Database ─────────────────────────────────────────────────────────
    let database = db::Database::open()?;
    let db_store = Arc::new(db::DbStore::new(database));

    // ── Immich client ────────────────────────────────────────────────────
    let client = if !config.server.url.is_empty() && !config.server.api_key.is_empty() {
        match api::ImmichClient::with_bandwidth_limit(
            &config.server.url,
            &config.server.api_key,
            config.upload.bandwidth_limit_kbps,
        ) {
            Ok(c) => {
                info!(url = %config.server.url, "Immich client created");
                Some(c)
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to create Immich client: {}; continuing without uploads",
                    e
                );
                None
            }
        }
    } else {
        info!("Server not configured; running without uploads");
        None
    };

    // ── Tokio runtime ────────────────────────────────────────────────────
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()?,
    );

    // ── App lifecycle ────────────────────────────────────────────────────
    let mut app = app::App::new(config, db_store, client, runtime);
    app.init()?;
    app.run(); // blocks until Quit

    Ok(())
}

/// Blocking update check at startup (before the tray app starts).
///
/// Runs a quick GitHub API check. If an update is available, serializes the
/// info to a temp file, spawns the update dialog as a subprocess, and waits.
/// If the update was applied (exit code 0), relaunches self.  Otherwise
/// continues to the normal tray app.
fn check_for_update_on_startup() {
    // Load config to check if updates are enabled.
    let updates_enabled = config::Config::load()
        .map(|c| c.advanced.check_for_updates)
        .unwrap_or(true);

    if !updates_enabled {
        info!("Update checks disabled in config, skipping startup check");
        return;
    }

    // Build a temporary tokio runtime for the async check.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to create runtime for update check");
            return;
        }
    };

    let (repo, channel) = config::Config::load()
        .map(|c| (c.advanced.update_repo, c.advanced.update_channel))
        .unwrap_or_default();
    let result = rt.block_on(updater::check_for_update(&repo, &channel));

    match result {
        updater::UpdateCheckResult::Available(info) => {
            info!(
                current = %info.current_version,
                new = %info.new_version,
                "Update available at startup, showing update dialog"
            );

            // Serialize UpdateInfo to a temp JSON file.
            let temp_dir = std::env::temp_dir();
            let info_path = temp_dir.join("immichsync_update_info.json");
            let info_json = match serde_json::to_string(&info) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to serialize update info");
                    return;
                }
            };
            if let Err(e) = std::fs::write(&info_path, &info_json) {
                tracing::warn!(error = %e, "Failed to write update info file");
                return;
            }

            // Grant the subprocess permission to steal foreground focus.
            #[cfg(target_os = "windows")]
            {
                use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
                const ASFW_ANY: u32 = u32::MAX;
                unsafe {
                    let _ = AllowSetForegroundWindow(ASFW_ANY);
                }
            }

            // Spawn the update dialog subprocess.
            let exe = std::env::current_exe().expect("current_exe");
            let info_path_str = info_path.display().to_string();

            info!("Spawning update dialog subprocess");
            match std::process::Command::new(&exe)
                .args(["--window", "update", "--update-info", &info_path_str])
                .status()
            {
                Ok(status) if status.code() == Some(0) => {
                    // Update was applied — relaunch self.
                    info!("Startup update applied, relaunching");
                    updater::relaunch_self();
                }
                Ok(status) => {
                    info!(code = ?status.code(), "User skipped startup update, continuing to tray app");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to launch update dialog");
                }
            }
        }
        updater::UpdateCheckResult::UpToDate => {
            info!("Already on latest version at startup");
        }
        updater::UpdateCheckResult::Failed(msg) => {
            tracing::warn!(error = %msg, "Startup update check failed, continuing");
        }
    }
}

/// Spawn a subprocess dialog with foreground window rights.
///
/// Calls `AllowSetForegroundWindow(ASFW_ANY)` so the child process can
/// bring its eframe window to the foreground, then spawns `current_exe()`
/// with the given args and waits for it to exit.
///
/// Returns `Some(exit_code)` on success, `None` if the spawn failed.
fn spawn_install_dialog(args: &[&str]) -> Option<i32> {
    // Grant the subprocess permission to steal foreground focus.
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
        // ASFW_ANY = (DWORD)-1 — allows any process to set foreground.
        const ASFW_ANY: u32 = u32::MAX;
        unsafe {
            let _ = AllowSetForegroundWindow(ASFW_ANY);
        }
    }

    let exe = std::env::current_exe().expect("current_exe");
    debug_log(&format!(
        "spawn_install_dialog: exe={}, args={args:?}",
        exe.display()
    ));
    info!(exe = %exe.display(), args = ?args, "Spawning install dialog subprocess");

    match std::process::Command::new(&exe).args(args).status() {
        Ok(status) => {
            let code = status.code();
            debug_log(&format!(
                "spawn_install_dialog: subprocess exited with code={code:?}"
            ));
            info!(code = ?code, "Install dialog subprocess exited");
            code
        }
        Err(e) => {
            debug_log(&format!("spawn_install_dialog: FAILED to spawn: {e}"));
            tracing::error!(error = %e, "Failed to spawn install dialog subprocess");
            None
        }
    }
}

/// Run a single UI window in subprocess mode and exit.
///
/// Called when the binary is launched with `--window <type>`.  Each
/// subprocess gets its own winit EventLoop, sidestepping the winit 0.30
/// limitation that only allows one EventLoop per process lifetime.
fn run_window_subprocess(window_type: &str) -> anyhow::Result<()> {
    match window_type {
        "install" => {
            info!("Subprocess: running install dialog");
            ui::install::run_install_dialog_subprocess(false, None);
        }
        "install-update" => {
            info!("Subprocess: running update dialog");
            // Parse --old-version from remaining args.
            let args: Vec<String> = std::env::args().collect();
            let old_version = args
                .windows(2)
                .find(|w| w[0] == "--old-version")
                .map(|w| w[1].clone());
            ui::install::run_install_dialog_subprocess(true, old_version);
        }
        "wizard" => {
            info!("Subprocess: running first-run wizard");
            ui::first_run::run_first_run_wizard();
        }
        "settings" => {
            info!("Subprocess: running settings window");
            let config = config::Config::load()?;
            ui::settings::show_settings(config, None);
        }
        "about" => {
            info!("Subprocess: running about dialog");
            ui::about::show_about();
        }
        "update" => {
            info!("Subprocess: running update dialog");
            let args: Vec<String> = std::env::args().collect();
            let info_path = args
                .windows(2)
                .find(|w| w[0] == "--update-info")
                .map(|w| w[1].clone())
                .unwrap_or_default();
            if info_path.is_empty() {
                tracing::error!("--update-info path required for update window");
                std::process::exit(1);
            }
            ui::update::run_update_dialog(&info_path);
        }
        "log" => {
            info!("Subprocess: running upload log");
            let database = db::Database::open()?;
            let db_store = Arc::new(db::DbStore::new(database));
            ui::upload_log::show_upload_log(db_store);
        }
        "trash-log" => {
            info!("Subprocess: running trash log");
            let database = db::Database::open()?;
            let db_store = Arc::new(db::DbStore::new(database));
            ui::trash_log::show_trash_log(db_store);
        }
        other => {
            tracing::error!("Unknown window type: {other}");
        }
    }
    Ok(())
}

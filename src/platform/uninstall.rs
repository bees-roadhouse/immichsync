//! Self-uninstall flow invoked by `immichsync.exe --uninstall`.
//!
//! Triggered two ways:
//!
//! - Windows Apps & Features → Uninstall click → runs `UninstallString`.
//! - `winget uninstall ImmichSync` / automation → runs `QuietUninstallString`
//!   (with `--silent`).
//!
//! Steps:
//!
//! 1. Stop any other running ImmichSync instance via `WM_CLOSE` to its
//!    shutdown window (graceful, lets in-flight uploads finish).
//! 2. Remove autostart registry value (`HKCU\...\Run\ImmichSync`).
//! 3. Remove desktop + Start Menu shortcuts.
//! 4. Remove the Apps & Features Uninstall registry block.
//! 5. Schedule the install directory for deletion after this process exits
//!    (detached `cmd.exe` spin-loop), since we can't delete the running
//!    binary from itself on Windows.
//! 6. Show a final "uninstall complete" dialog (unless `--silent`).
//!
//! Deliberately preserves user data:
//! - `%APPDATA%\bees-roadhouse\immichsync\config.toml`
//! - `%LOCALAPPDATA%\bees-roadhouse\immichsync\state.db*`
//! - `%LOCALAPPDATA%\bees-roadhouse\immichsync\logs\*`
//!
//! Matches the convention of every photo app, sync tool, and IDE ... users
//! uninstalling almost always mean "remove the program," not "wipe my
//! settings and history."  A separate `--purge` flag is a follow-up issue.

use std::path::PathBuf;

use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};

use crate::config::Config;

/// Top-level entry point for `immichsync.exe --uninstall [--silent]`.
pub fn run(silent: bool) -> anyhow::Result<()> {
    info!(silent, "Uninstall started");

    // 1. Stop any other running instance.
    if let Err(e) = stop_running_instance() {
        warn!(error = %e, "Could not stop running instance (continuing)");
    }
    // Give the other process a beat to release its file handles / mutex.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // 2. Remove autostart registry value.
    if let Err(e) = crate::platform::set_autostart(false) {
        warn!(error = %e, "Failed to remove autostart entry (continuing)");
    }

    // 3. Remove shortcuts.
    if let Err(e) = crate::platform::shortcuts::remove_desktop_shortcut("ImmichSync") {
        warn!(error = %e, "Failed to remove desktop shortcut (continuing)");
    }
    if let Err(e) = crate::platform::shortcuts::remove_start_menu_shortcut("ImmichSync") {
        warn!(error = %e, "Failed to remove Start Menu shortcut (continuing)");
    }

    // 4. Remove the Apps & Features Uninstall registry block.
    if let Err(e) = crate::platform::install::delete_uninstall_registry() {
        warn!(error = %e, "Failed to remove Uninstall registry block (continuing)");
    }

    // 5. Show the "uninstall complete" dialog BEFORE scheduling self-delete,
    //    so the user sees confirmation before the binary disappears.
    if !silent {
        show_complete_dialog();
    }

    // 6. Schedule self-delete of the install directory. The current process
    //    keeps holding immichsync.exe open until we exit, so we hand the
    //    delete to a detached cmd.exe that waits a few seconds then nukes
    //    the dir.
    let install_dir = Config::install_dir().ok();
    if let Some(dir) = install_dir {
        if let Err(e) = schedule_self_delete(&dir) {
            warn!(path = %dir.display(), error = %e, "Failed to schedule self-delete");
        }
    }

    info!("Uninstall complete");
    Ok(())
}

/// Find the hidden shutdown window registered by `platform::shutdown` and
/// post `WM_CLOSE` to it. The other instance's message pump will translate
/// that into `PostQuitMessage(0)` → graceful shutdown.
fn stop_running_instance() -> anyhow::Result<()> {
    // Window class name must match `platform::shutdown::install`.
    let class_wide: Vec<u16> = "ImmichSyncShutdown\0".encode_utf16().collect();

    let hwnd = unsafe {
        FindWindowW(PCWSTR(class_wide.as_ptr()), PCWSTR::null()).unwrap_or(HWND::default())
    };

    if hwnd.0.is_null() {
        info!("No running ImmichSync instance to stop");
        return Ok(());
    }

    info!(?hwnd, "Posting WM_CLOSE to running instance");
    unsafe {
        PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0))?;
    }
    Ok(())
}

/// Schedule deletion of the install directory via a detached `cmd.exe` that
/// loops trying to delete until our process has exited (and the file handle
/// is released).
///
/// Pattern: `cmd /c "ping 127.0.0.1 -n 3 >nul & rmdir /s /q <dir>"`. The
/// short ping is a portable sleep ... `timeout` is interactive and refuses
/// to run without a console. We launch with `DETACHED_PROCESS` +
/// `CREATE_NO_WINDOW` so it survives our exit and shows no flash.
fn schedule_self_delete(install_dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;

    // DETACHED_PROCESS | CREATE_NO_WINDOW = 0x00000008 | 0x08000000.
    const DETACHED_AND_HIDDEN: u32 = 0x00000008 | 0x08000000;

    let dir_str = install_dir.display().to_string();
    let script = format!(
        // Wait ~4s (5 pings @ ~1s each minus one immediate), then rmdir.
        // The `& exit` keeps the shell tidy even if rmdir fails (e.g. user
        // opened a file from inside the dir). 4s is comfortably more than
        // the 500ms we already gave the other instance to exit + the ~1s
        // this --uninstall process needs to wrap up after spawning cmd.
        "ping 127.0.0.1 -n 5 >nul & rmdir /s /q \"{dir_str}\" & exit"
    );

    info!(path = %install_dir.display(), "Scheduling install dir for delete-after-exit");

    std::process::Command::new("cmd.exe")
        .args(["/c", &script])
        .creation_flags(DETACHED_AND_HIDDEN)
        .spawn()?;
    Ok(())
}

/// Show a minimal "uninstall complete" message box. Uses the Win32
/// `MessageBoxW` so we don't have to drag eframe into the uninstall path
/// (which keeps the uninstall fast and avoids spinning up a full GUI just
/// to confirm).
fn show_complete_dialog() {
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };

    let cfg_path = Config::config_path()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let local_path = Config::local_data_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let body = format!(
        "ImmichSync has been removed.\n\n\
        Your settings and upload history are preserved at:\n\
        {cfg_path}\n\
        {local_path}\n\n\
        Reinstall any time to resume from where you left off."
    );
    let body_wide: Vec<u16> = body.encode_utf16().chain(std::iter::once(0)).collect();
    let title_wide: Vec<u16> = "ImmichSync\0".encode_utf16().collect();

    unsafe {
        MessageBoxW(
            None,
            PCWSTR(body_wide.as_ptr()),
            PCWSTR(title_wide.as_ptr()),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// User-facing data paths that uninstall deliberately preserves.
///
/// Returned for documentation/test purposes. The uninstall flow never
/// touches these.
#[allow(dead_code)]
pub fn preserved_data_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(p) = Config::config_path() {
        paths.push(p);
    }
    if let Ok(p) = Config::local_data_dir() {
        paths.push(p);
    }
    paths
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserved_paths_include_config_and_local_data() {
        let paths = preserved_data_paths();
        let s: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();

        // Config path should be the config.toml in the roaming dir.
        assert!(
            s.iter().any(|p| p.ends_with("config.toml")),
            "config.toml not in preserved paths: {s:?}"
        );
        // Local data dir should be present.
        assert!(
            s.iter().any(|p| p.contains("bees-roadhouse")),
            "bees-roadhouse local dir not in preserved paths: {s:?}"
        );
    }
}

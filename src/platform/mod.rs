// Platform layer ... every OS-specific integration lives here.
//
// Callers reach the same names (`platform::autostart`, `platform::SingleInstance`,
// ...) on every OS; `cfg(target_os)` picks the implementation. Windows keeps
// its original modules in this directory; macOS lives under `macos/`. See
// docs/adr/0001-macos-platform-seam.md for the Windows → macOS map.

pub mod logs;
pub mod volumes;

#[cfg(windows)]
pub mod autostart;
#[cfg(windows)]
mod device;
#[cfg(windows)]
mod drives;
#[cfg(windows)]
pub mod encryption;
#[cfg(windows)]
pub mod event_loop;
#[cfg(windows)]
pub mod install;
#[cfg(windows)]
pub mod known_folders;
#[cfg(windows)]
pub mod shortcuts;
#[cfg(windows)]
pub mod shutdown;
#[cfg(windows)]
pub mod single_instance;
#[cfg(windows)]
pub mod uninstall;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{
    autostart, bundle_update, encryption, event_loop, install, known_folders, notify, packages,
    photos, shutdown, single_instance, trash,
};

/// Application User Model ID for toast notifications and Start Menu shortcuts.
///
/// Must match the AppUserModelID property set on the Start Menu shortcut;
/// otherwise Windows silently drops toast notifications from desktop apps.
#[cfg(windows)]
pub const APP_USER_MODEL_ID: &str = "BeesRoadhouse.ImmichSync";

/// Settings label for the autostart toggle, in the OS's own words.
pub const AUTOSTART_LABEL: &str = if cfg!(windows) {
    "Start with Windows"
} else {
    "Start at login"
};

/// Bring a window that another of our processes owns to the front: the
/// Settings subprocess, when Settings is clicked while it's open (#102).
/// Best effort: the OS may decline (focus-stealing rules), and that's logged.
pub fn focus_window(pid: u32, title: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = title;
        use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
        if pid == 0 {
            return;
        }
        let activated = NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32)
            .map(|app| app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows))
            .unwrap_or(false);
        if !activated {
            tracing::debug!(pid, "Could not activate the window's process");
        }
    }
    #[cfg(windows)]
    {
        let _ = pid;
        use windows::core::HSTRING;
        use windows::Win32::UI::WindowsAndMessaging::{
            FindWindowW, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
        };
        // SAFETY: plain Win32 calls on a window handle we just looked up.
        unsafe {
            match FindWindowW(None, &HSTRING::from(title)) {
                Ok(hwnd) if !hwnd.is_invalid() => {
                    if IsIconic(hwnd).as_bool() {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                    }
                    if !SetForegroundWindow(hwnd).as_bool() {
                        tracing::debug!(title, "SetForegroundWindow declined");
                    }
                }
                _ => tracing::debug!(title, "Window not found to focus"),
            }
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = (pid, title);
}

/// The After Upload "trash" choice, saying where files actually go: the
/// Finder Trash on macOS (#95); elsewhere a `.immichsync-trash` folder in
/// the watched folder, not the Recycle Bin (#102).
pub const TRASH_ACTION_LABEL: &str = if cfg!(target_os = "macos") {
    "Move to Trash"
} else {
    "Move to .immichsync-trash"
};

/// Whether the GitHub-release self-updater can replace this build. Windows
/// swaps the `.exe`; Apple Silicon macOS swaps the `.app` bundle from the
/// `macos-arm64` zip (#75). Intel Macs get no release asset, so no checks.
pub const SELF_UPDATE_SUPPORTED: bool = cfg!(any(
    windows,
    all(target_os = "macos", target_arch = "aarch64")
));

/// Shown in the update dialog. An ad-hoc-signed bundle (until #93) has a
/// new code identity on every release, so macOS re-asks for the Keychain
/// item once. Accepted for now (Nate, 2026-10-19).
pub const UPDATE_NOTE: Option<&str> = if cfg!(target_os = "macos") {
    Some("macOS may ask to allow Keychain access after updating. Choose Always Allow.")
} else {
    None
};

// Re-export only the symbols callers actually reach through the short
// `platform::*` form. Symbols only ever referenced through the long
// `platform::submodule::*` form don't need a re-export and trip the
// `unused_imports` lint when they do.

pub use autostart::set_autostart;
pub use install::installed_exe_path;
#[cfg(windows)]
pub use install::{
    cleanup_legacy_install, install_exe, is_running_installed, migrate_legacy_data,
    migrate_to_split_layout, relaunch_installed,
};
#[cfg(windows)]
pub use shortcuts::{create_desktop_shortcut, create_start_menu_shortcut};
pub use single_instance::SingleInstance;

// Platform layer ... every OS-specific integration lives here.
//
// Callers reach the same names (`platform::autostart`, `platform::SingleInstance`,
// ...) on every OS; `cfg(target_os)` picks the implementation. Windows keeps
// its original modules in this directory; macOS lives under `macos/`. See
// docs/adr/0001-macos-platform-seam.md for the Windows → macOS map.

pub mod logs;

#[cfg(windows)]
pub mod autostart;
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
    autostart, encryption, event_loop, install, known_folders, notify, shutdown, single_instance,
    trash,
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

/// Whether the GitHub-release self-updater can replace this build. Windows
/// swaps the `.exe`; macOS has no bundle to swap until #63, so update checks
/// are skipped there rather than offering an `.exe`.
pub const SELF_UPDATE_SUPPORTED: bool = cfg!(windows);

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

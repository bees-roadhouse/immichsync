// Platform layer ... Windows-specific integrations.
//
// All submodules are Windows-only.  No cross-platform abstractions are
// provided; the entire application targets Windows 10/11.

pub mod autostart;
pub mod encryption;
pub mod install;
pub mod known_folders;
pub mod shortcuts;
pub mod shutdown;
pub mod single_instance;
pub mod uninstall;

/// Application User Model ID for toast notifications and Start Menu shortcuts.
///
/// Must match the AppUserModelID property set on the Start Menu shortcut;
/// otherwise Windows silently drops toast notifications from desktop apps.
pub const APP_USER_MODEL_ID: &str = "BeesRoadhouse.ImmichSync";

// Re-export only the symbols callers actually reach through the short
// `platform::*` form. Symbols only ever referenced through the long
// `platform::submodule::*` form don't need a re-export and trip the
// `unused_imports` lint when they do.

pub use autostart::set_autostart;
pub use install::{
    cleanup_legacy_install, install_exe, installed_exe_path, is_running_installed,
    migrate_legacy_data, migrate_to_split_layout, relaunch_installed,
};
pub use shortcuts::{create_desktop_shortcut, create_start_menu_shortcut};
pub use single_instance::SingleInstance;

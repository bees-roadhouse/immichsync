// Platform layer — Windows-specific integrations.
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

/// Application User Model ID for toast notifications and Start Menu shortcuts.
///
/// Must match the AppUserModelID property set on the Start Menu shortcut;
/// otherwise Windows silently drops toast notifications from desktop apps.
pub const APP_USER_MODEL_ID: &str = "BeesRoadhouse.ImmichSync";

// Re-export the most-used public types at the platform crate boundary so
// callers can write `platform::SingleInstance` instead of
// `platform::single_instance::SingleInstance`.

pub use autostart::set_autostart;
pub use install::{
    install_exe, installed_exe_path, is_running_installed, migrate_legacy_data, relaunch_installed,
};
pub use shortcuts::{create_desktop_shortcut, create_start_menu_shortcut};
pub use single_instance::SingleInstance;

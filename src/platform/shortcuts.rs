// Windows shortcut (.lnk) creation via COM.
//
// Uses IShellLinkW + IPersistFile to create standard Windows shortcuts
// on the desktop and in the Start Menu.  Sets AppUserModelID on each
// shortcut so Windows can route toast notifications to our app.
//
// Required Cargo.toml features:
//   "Win32_UI_Shell"                — IShellLinkW, SHGetKnownFolderPath, FOLDERID_*
//   "Win32_System_Com"              — CoInitializeEx, CoCreateInstance, IPersistFile, CoTaskMemFree
//   "Win32_UI_Shell_PropertiesSystem" — IPropertyStore, PROPERTYKEY

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::info;
use windows::core::{Interface, GUID, HSTRING, PCWSTR, PROPVARIANT, PWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, PROPERTYKEY};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG,
};

// CLSID_ShellLink: {00021401-0000-0000-C000-000000000046}
const CLSID_SHELLLINK: GUID = GUID::from_values(
    0x00021401,
    0x0000,
    0x0000,
    [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);

// PKEY_AppUserModel_ID: {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, pid 5
const PKEY_APPUSERMODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_values(
        0x9F4C2855,
        0x9F79,
        0x4B39,
        [0xA8, 0xD0, 0xE1, 0xD4, 0x2D, 0xE1, 0xD5, 0xF3],
    ),
    pid: 5,
};

#[derive(Debug, Error)]
pub enum ShortcutError {
    #[error("failed to create ShellLink instance: {0}")]
    CreateInstance(#[source] windows::core::Error),

    #[error("failed to set shortcut path: {0}")]
    SetPath(#[source] windows::core::Error),

    #[error("failed to set shortcut description: {0}")]
    SetDescription(#[source] windows::core::Error),

    #[error("failed to set shortcut icon: {0}")]
    SetIcon(#[source] windows::core::Error),

    #[error("failed to set working directory: {0}")]
    SetWorkingDir(#[source] windows::core::Error),

    #[error("failed to query IPersistFile: {0}")]
    QueryPersistFile(#[source] windows::core::Error),

    #[error("failed to save .lnk file: {0}")]
    Save(#[source] windows::core::Error),

    #[error("failed to set AppUserModelID: {0}")]
    SetAppId(#[source] windows::core::Error),

    #[error("failed to resolve known folder: {0}")]
    KnownFolder(#[source] windows::core::Error),

    #[error("known folder path is not valid UTF-16")]
    InvalidFolderPath,
}

/// Create a desktop shortcut pointing to the given executable.
pub fn create_desktop_shortcut(
    exe_path: &Path,
    name: &str,
    description: &str,
) -> Result<PathBuf, ShortcutError> {
    let desktop = get_known_folder(&FOLDERID_Desktop)?;
    let lnk_path = desktop.join(format!("{name}.lnk"));

    info!(lnk = %lnk_path.display(), "Creating desktop shortcut");
    create_shortcut(exe_path, &lnk_path, description)?;

    Ok(lnk_path)
}

/// Create a Start Menu shortcut pointing to the given executable.
pub fn create_start_menu_shortcut(
    exe_path: &Path,
    name: &str,
    description: &str,
) -> Result<PathBuf, ShortcutError> {
    let programs = get_known_folder(&FOLDERID_Programs)?;
    let lnk_path = programs.join(format!("{name}.lnk"));

    info!(lnk = %lnk_path.display(), "Creating Start Menu shortcut");
    create_shortcut(exe_path, &lnk_path, description)?;

    Ok(lnk_path)
}

/// Create a .lnk shortcut file via COM with AppUserModelID set.
fn create_shortcut(
    exe_path: &Path,
    lnk_path: &Path,
    description: &str,
) -> Result<(), ShortcutError> {
    unsafe {
        // Initialize COM (apartment-threaded). If already initialized, the
        // call returns S_FALSE which is still Ok.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // Create the ShellLink COM object.
        let shell_link: IShellLinkW =
            CoCreateInstance(&CLSID_SHELLLINK, None, CLSCTX_INPROC_SERVER)
                .map_err(ShortcutError::CreateInstance)?;

        // Set the target executable path.
        let exe_hstr = HSTRING::from(exe_path.as_os_str());
        shell_link
            .SetPath(&exe_hstr)
            .map_err(ShortcutError::SetPath)?;

        // Set the description (tooltip).
        let desc_hstr = HSTRING::from(description);
        shell_link
            .SetDescription(&desc_hstr)
            .map_err(ShortcutError::SetDescription)?;

        // Set the icon to the exe itself (icon index 0).
        shell_link
            .SetIconLocation(&exe_hstr, 0)
            .map_err(ShortcutError::SetIcon)?;

        // Set working directory to the exe's parent folder.
        if let Some(parent) = exe_path.parent() {
            let parent_hstr = HSTRING::from(parent.as_os_str());
            shell_link
                .SetWorkingDirectory(&parent_hstr)
                .map_err(ShortcutError::SetWorkingDir)?;
        }

        // Set AppUserModelID so toast notifications can find us.
        let prop_store: IPropertyStore = shell_link.cast().map_err(ShortcutError::SetAppId)?;

        let aumid = PROPVARIANT::from(super::APP_USER_MODEL_ID);
        prop_store
            .SetValue(&PKEY_APPUSERMODEL_ID, &aumid)
            .map_err(ShortcutError::SetAppId)?;
        prop_store.Commit().map_err(ShortcutError::SetAppId)?;

        // Save the .lnk file via IPersistFile.
        let persist_file: IPersistFile =
            shell_link.cast().map_err(ShortcutError::QueryPersistFile)?;

        let lnk_wide: Vec<u16> = lnk_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        persist_file
            .Save(PCWSTR(lnk_wide.as_ptr()), true)
            .map_err(ShortcutError::Save)?;
    }

    Ok(())
}

/// Resolve a known folder path by GUID (reuses SHGetKnownFolderPath pattern).
fn get_known_folder(folder_id: &GUID) -> Result<PathBuf, ShortcutError> {
    let path_ptr: PWSTR = unsafe {
        SHGetKnownFolderPath(folder_id, KNOWN_FOLDER_FLAG(0), None)
            .map_err(ShortcutError::KnownFolder)?
    };

    let path = unsafe {
        let s = path_ptr
            .to_string()
            .map_err(|_| ShortcutError::InvalidFolderPath)?;
        PathBuf::from(s)
    };

    unsafe {
        CoTaskMemFree(Some(path_ptr.0 as *const core::ffi::c_void));
    }

    Ok(path)
}

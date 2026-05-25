// Known-folder resolution via SHGetKnownFolderPath.
//
// Additional windows features required (add to Cargo.toml):
//   "Win32_System_Com"   — for CoTaskMemFree (frees the PWSTR returned by
//                          SHGetKnownFolderPath)
//
// Note: FOLDERID_Pictures and SHGetKnownFolderPath are under Win32_UI_Shell,
// which is already listed in Cargo.toml.

use std::path::PathBuf;
use thiserror::Error;
use windows::core::PWSTR;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};

// FOLDERID_Pictures: {33E28130-4E1E-4676-835A-98395C3BC3BB}
// Defined as a const here to avoid depending on KnownFolders.h directly.
// The windows crate exposes it under Win32::UI::Shell::FOLDERID_Pictures.
use windows::Win32::UI::Shell::FOLDERID_Pictures;

#[derive(Debug, Error)]
pub enum KnownFolderError {
    #[error("SHGetKnownFolderPath failed: {0}")]
    ShellApiError(#[from] windows::core::Error),

    #[error("returned path is not valid UTF-16")]
    InvalidPath,
}

/// Resolve the user's Pictures folder, respecting OneDrive redirects and
/// custom shell folder locations set by the user.
pub fn get_pictures_folder() -> Result<PathBuf, KnownFolderError> {
    // SAFETY: SHGetKnownFolderPath allocates a wide string via CoTaskMem;
    // we must free it with CoTaskMemFree after use.
    let path_ptr: PWSTR = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_Pictures,
            KNOWN_FOLDER_FLAG(0), // KF_FLAG_DEFAULT
            None,                 // current user token
        )?
    };

    // Convert the PWSTR to a Rust PathBuf before freeing.
    let path = unsafe {
        // PWSTR::to_string() walks the null-terminated wide string.
        let s = path_ptr
            .to_string()
            .map_err(|_| KnownFolderError::InvalidPath)?;
        PathBuf::from(s)
    };

    // Free the CoTaskMem allocation.
    // SAFETY: path_ptr was returned by SHGetKnownFolderPath and has not been
    // freed yet.
    unsafe {
        CoTaskMemFree(Some(path_ptr.0 as *const core::ffi::c_void));
    }

    tracing::debug!("pictures folder: {}", path.display());
    Ok(path)
}

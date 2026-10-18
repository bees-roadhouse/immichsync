// Known-folder resolution (macOS): `~/Pictures`.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum KnownFolderError {
    #[error("could not resolve the Pictures folder")]
    NotFound,
}

/// Resolve the user's Pictures folder (`~/Pictures`).
pub fn get_pictures_folder() -> Result<PathBuf, KnownFolderError> {
    dirs::picture_dir().ok_or(KnownFolderError::NotFound)
}

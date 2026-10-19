// The Finder Trash (macOS).
//
// The post-upload "Move to Trash" action uses the real Trash here, via
// `-[NSFileManager trashItemAtURL:resultingItemURL:error:]`, so files can be
// put back from Finder. The Windows build keeps its `.immichsync-trash`
// folder inside each watched folder.

use std::path::{Path, PathBuf};

use objc2_foundation::{NSFileManager, NSString, NSURL};

/// Move `path` to the user's Trash; returns where it landed. The Trash
/// renames on collision ("photo 2.jpg"), so an earlier trashed file is never
/// overwritten.
pub fn move_to_trash(path: &Path) -> std::io::Result<PathBuf> {
    let path_str = path
        .to_str()
        .ok_or_else(|| std::io::Error::other("path is not valid UTF-8"))?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
    let mut landed = None;
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, Some(&mut landed))
        .map_err(|e| std::io::Error::other(e.localizedDescription().to_string()))?;
    Ok(landed
        .and_then(|u| u.path())
        .map(|p| PathBuf::from(p.to_string()))
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    /// Puts a temp file in the real Trash, then removes that one item again
    /// (it is this test's own file). Opt-in:
    /// `cargo test -- --ignored finder_trash_round_trip`.
    #[test]
    #[ignore]
    fn finder_trash_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir
            .path()
            .join(format!("immichsync-trash-test-{}.jpg", std::process::id()));
        std::fs::write(&f, b"test").unwrap();
        let landed = super::move_to_trash(&f).unwrap();
        assert!(!f.exists(), "source still present");
        assert!(landed.exists(), "not in the Trash: {}", landed.display());
        assert!(
            landed.to_string_lossy().contains("/.Trash/"),
            "{}",
            landed.display()
        );
        std::fs::remove_file(&landed).unwrap();
    }
}

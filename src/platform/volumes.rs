// Volume mount/unmount events ... the removable-media seam (#65).
//
// One shape on every OS: a `VolumeMonitor` that, once started, sends a
// `VolumeEvent` for every volume already mounted and then for each mount and
// unmount after that. It knows nothing about DCIM folders or config; the
// decision lives in `watch::device`.
//
// - Windows: a message-only window receiving `WM_DEVICECHANGE`
//   (`platform/device.rs`, revived from before #42), plus a drive-letter
//   sweep at start (`platform/drives.rs`).
// - macOS: DiskArbitration appeared / description-changed / disappeared
//   callbacks on a CFRunLoop thread (`platform/macos/volumes.rs`).

use std::path::{Path, PathBuf};

/// A volume came or went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeEvent {
    Mounted {
        /// Where the volume is mounted: `E:\` or `/Volumes/CARD`.
        root: PathBuf,
        /// Volume label, when it has one.
        label: Option<String>,
        /// A stable-ish identifier for logs: volume serial on Windows,
        /// volume UUID (or BSD name) on macOS.
        id: String,
        /// Whether the OS calls the media removable or ejectable.
        removable: bool,
    },
    Unmounted {
        root: PathBuf,
    },
}

/// `true` if `<root>/DCIM` is a directory (the camera-card convention).
pub fn has_dcim(root: &Path) -> bool {
    root.join("DCIM").is_dir()
}

#[cfg(windows)]
pub use super::device::VolumeMonitor;
#[cfg(target_os = "macos")]
pub use super::macos::volumes::VolumeMonitor;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_dcim_sees_only_a_dcim_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_dcim(dir.path()));
        std::fs::write(dir.path().join("DCIM"), b"not a dir").unwrap();
        assert!(!has_dcim(dir.path()));
        std::fs::remove_file(dir.path().join("DCIM")).unwrap();
        std::fs::create_dir(dir.path().join("DCIM")).unwrap();
        assert!(has_dcim(dir.path()));
    }
}

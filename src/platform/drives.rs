// Drive enumeration and type detection via Win32 storage APIs.
//
// Windows features used (all under Win32_Storage_FileSystem, which is already
// in Cargo.toml):
//   GetLogicalDrives       — returns a bitmask of available drive letters
//   GetDriveTypeW          — returns the type (fixed, removable, network, etc.)
//   GetVolumeInformationW  — returns the volume label

use std::path::PathBuf;
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};

/// High-level drive type, mapped from Win32 DRIVE_* constants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveType {
    Fixed,
    Removable,
    Network,
    CdRom,
    Unknown,
}

/// Information about one logical drive on the system.
#[derive(Debug, Clone)]
pub struct DriveInfo {
    /// Drive letter, e.g. `'C'`.
    pub letter: char,
    /// Categorised drive type.
    pub drive_type: DriveType,
    /// Volume label, if one is set and can be read.
    pub label: Option<String>,
}

/// Encode a null-terminated ASCII/UTF-16 drive-root string, e.g. `"C:\\\0"`.
fn drive_root_wide(letter: char) -> Vec<u16> {
    let s = format!("{}:\\", letter);
    s.encode_utf16().chain(std::iter::once(0u16)).collect()
}

/// Enumerate all logical drives currently visible to the OS.
///
/// Drives that cannot be queried (e.g. unmounted optical drives) have
/// `label = None`.  Their type is still classified.
pub fn list_drives() -> Vec<DriveInfo> {
    // GetLogicalDrives returns a bitmask: bit 0 = A, bit 1 = B, … bit 25 = Z.
    let bitmask = unsafe { GetLogicalDrives() };
    if bitmask == 0 {
        tracing::warn!("GetLogicalDrives returned 0");
        return Vec::new();
    }

    let mut drives = Vec::new();

    for bit in 0u32..26 {
        if bitmask & (1 << bit) == 0 {
            continue;
        }

        let letter = char::from(b'A' + bit as u8);
        let root_wide = drive_root_wide(letter);
        let root_pcwstr = PCWSTR(root_wide.as_ptr());

        // Determine drive type.
        let raw_type = unsafe { GetDriveTypeW(root_pcwstr) };
        let drive_type = match raw_type {
            3 => DriveType::Fixed,     // DRIVE_FIXED
            2 => DriveType::Removable, // DRIVE_REMOVABLE
            4 => DriveType::Network,   // DRIVE_REMOTE
            5 => DriveType::CdRom,     // DRIVE_CDROM
            _ => DriveType::Unknown,
        };

        // Attempt to read the volume label.
        // Buffer of 260 UTF-16 code units is sufficient per MAX_PATH.
        let mut label_buf = vec![0u16; 260];
        let label_ok = unsafe {
            GetVolumeInformationW(
                root_pcwstr,
                Some(label_buf.as_mut_slice()),
                None, // volume serial number
                None, // max component length
                None, // filesystem flags
                None, // filesystem name
            )
            .is_ok()
        };

        let label = if label_ok {
            // Find the null terminator.
            let len = label_buf.iter().position(|&c| c == 0).unwrap_or(0);
            if len > 0 {
                String::from_utf16_lossy(&label_buf[..len])
                    .trim()
                    .to_owned()
                    .into()
            } else {
                None
            }
        } else {
            None
        };

        tracing::debug!("drive {}: type={:?} label={:?}", letter, drive_type, label);

        drives.push(DriveInfo {
            letter,
            drive_type,
            label,
        });
    }

    drives
}

/// Return `true` if `{drive}:\DCIM` exists (camera DCIM folder convention).
pub fn has_dcim_folder(drive_letter: char) -> bool {
    let path = PathBuf::from(format!("{}:\\DCIM", drive_letter));
    let exists = path.is_dir();
    tracing::debug!("DCIM check {}: {}", path.display(), exists);
    exists
}

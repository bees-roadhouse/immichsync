// Drive enumeration and type detection via Win32 storage APIs.
//
// Revived from before #42 for the removable-media seam
// (`platform::volumes`, #89): the device monitor uses it to sweep drives
// already present at start and to describe a drive when it arrives.
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
    /// Volume serial number, if the volume could be queried.
    pub serial: Option<u32>,
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

    drive_letters_from_mask(bitmask)
        .into_iter()
        .map(drive_info)
        .collect()
}

/// Root path of a drive letter: `E:\`.
pub fn drive_root(letter: char) -> PathBuf {
    PathBuf::from(format!("{}:\\", letter))
}

/// Drive letter(s) in a unit mask. Bit 0 = A, bit 1 = B, ... bit 25 = Z.
pub fn drive_letters_from_mask(mask: u32) -> Vec<char> {
    (0u32..26)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| char::from(b'A' + bit as u8))
        .collect()
}

/// Type, label and serial of one drive letter.
pub fn drive_info(letter: char) -> DriveInfo {
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
    let mut serial = 0u32;
    let label_ok = unsafe {
        GetVolumeInformationW(
            root_pcwstr,
            Some(label_buf.as_mut_slice()),
            Some(&mut serial),
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

    DriveInfo {
        letter,
        drive_type,
        label,
        serial: label_ok.then_some(serial),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_letters_from_mask_reads_each_bit() {
        // Bit 2 = C, bit 4 = E
        assert_eq!(drive_letters_from_mask(0b10100), vec!['C', 'E']);
        assert!(drive_letters_from_mask(0).is_empty());
    }

    #[test]
    fn drive_root_is_the_letter_root() {
        assert_eq!(drive_root('E'), PathBuf::from("E:\\"));
    }
}

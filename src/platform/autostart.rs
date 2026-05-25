// Windows autostart via the per-user Run registry key.
//
// Additional windows features required (add to Cargo.toml):
//   "Win32_System_Registry"  — for RegOpenKeyExW, RegSetValueExW,
//                              RegDeleteValueW, RegQueryValueExW,
//                              RegCloseKey, and the associated constants.
//
// The key written is:
//   HKCU\Software\Microsoft\Windows\CurrentVersion\Run
//   Value name : "ImmichSync"
//   Value data : full path to the running executable

use thiserror::Error;
use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_WRITE, REG_SZ,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0";
const VALUE_NAME: &str = "ImmichSync\0";

#[derive(Debug, Error)]
pub enum AutostartError {
    #[error("registry open failed: Win32 error {0:#010x}")]
    RegistryOpen(u32),

    #[error("registry write failed: Win32 error {0:#010x}")]
    RegistryWrite(u32),

    #[error("registry delete failed: Win32 error {0:#010x}")]
    RegistryDelete(u32),

    #[error("could not determine current executable path: {0}")]
    ExePath(#[from] std::io::Error),

    #[error("executable path is not valid UTF-8")]
    ExePathEncoding,
}

// Encode a Rust &str (expected to be null-terminated, e.g. "foo\0") as
// a Vec<u16> suitable for use as a PCWSTR.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Enable or disable autostart for the current user.
///
/// When `enabled` is `true`, writes the current executable path as the
/// registry value.  When `false`, deletes the value if it exists.
pub fn set_autostart(enabled: bool) -> Result<(), AutostartError> {
    let key_wide = wide(RUN_KEY);
    let value_wide = wide(VALUE_NAME);

    let mut hkey = HKEY::default();
    let open_result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_wide.as_ptr()),
            0,
            KEY_WRITE,
            &mut hkey,
        )
    };

    if open_result.is_err() {
        return Err(AutostartError::RegistryOpen(open_result.0 as u32));
    }

    let result = if enabled {
        // Always point autostart at the installed exe path so the registry
        // value is stable regardless of where the app was originally launched.
        let exe = crate::platform::install::installed_exe_path()
            .map_err(|e| AutostartError::ExePath(std::io::Error::other(e)))?;
        let exe_str = exe.to_str().ok_or(AutostartError::ExePathEncoding)?;

        // Encode as a null-terminated wide string for REG_SZ.
        // REG_SZ data must include the null terminator in the byte count.
        let exe_wide: Vec<u16> = exe_str.encode_utf16().chain(std::iter::once(0)).collect();
        let byte_len = (exe_wide.len() * 2) as u32;

        tracing::info!("enabling autostart: {}", exe_str);

        let r = unsafe {
            RegSetValueExW(
                hkey,
                PCWSTR(value_wide.as_ptr()),
                0,
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    exe_wide.as_ptr() as *const u8,
                    byte_len as usize,
                )),
            )
        };
        if r.is_err() {
            Err(AutostartError::RegistryWrite(r.0 as u32))
        } else {
            Ok(())
        }
    } else {
        tracing::info!("disabling autostart");

        let r = unsafe { RegDeleteValueW(hkey, PCWSTR(value_wide.as_ptr())) };

        match r.0 {
            0 => Ok(()), // ERROR_SUCCESS
            2 => Ok(()), // ERROR_FILE_NOT_FOUND — already absent, that's fine
            e => Err(AutostartError::RegistryDelete(e)),
        }
    };

    let _ = unsafe { RegCloseKey(hkey) };
    result
}

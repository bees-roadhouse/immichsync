// Single-instance enforcement via a Windows named mutex.
//
// Additional windows features required (add to Cargo.toml):
//   "Win32_System_Threading"  — for CreateMutexW
//
// The mutex is named "Global\ImmichSync" so it is visible across all
// terminal-server sessions on the machine (Global\ namespace).  Acquiring
// it means we are the first instance; ERROR_ALREADY_EXISTS means another
// copy is already running.

use thiserror::Error;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WIN32_ERROR};
use windows::Win32::System::Threading::CreateMutexW;

const ERROR_ALREADY_EXISTS: WIN32_ERROR = WIN32_ERROR(183);
const MUTEX_NAME: &str = "Global\\ImmichSync\0"; // null-terminated for PCWSTR

#[derive(Debug, Error)]
pub enum SingleInstanceError {
    #[error("failed to create named mutex: Win32 error {0}")]
    CreateFailed(u32),
}

/// Guard that holds the named mutex for the lifetime of the process.
/// Drop releases the mutex handle.
pub struct SingleInstance {
    handle: HANDLE,
}

// HANDLE is a raw pointer type — we know we are the sole owner.
unsafe impl Send for SingleInstance {}
unsafe impl Sync for SingleInstance {}

impl SingleInstance {
    /// Attempt to acquire the single-instance mutex.
    ///
    /// Returns:
    /// - `Ok(Some(guard))` — mutex acquired; this is the first instance.
    /// - `Ok(None)`        — another instance is already running.
    /// - `Err(_)`          — unexpected Win32 error.
    pub fn acquire() -> Result<Option<Self>, SingleInstanceError> {
        // Encode the mutex name as a null-terminated wide string.
        let name_wide: Vec<u16> = MUTEX_NAME.encode_utf16().collect();

        // SAFETY: name_wide is a valid null-terminated wide string kept alive
        // for the duration of the call.
        let handle = unsafe {
            CreateMutexW(
                None, // default security attributes
                true, // we request initial ownership
                PCWSTR(name_wide.as_ptr()),
            )
        };

        match handle {
            Ok(h) => {
                // CreateMutexW may succeed but set ERROR_ALREADY_EXISTS when
                // the mutex already existed (another instance owns it).
                let last_error = unsafe { GetLastError() };
                if last_error == ERROR_ALREADY_EXISTS {
                    // Release our reference — we don't need it.
                    let _ = unsafe { CloseHandle(h) };
                    tracing::info!("another ImmichSync instance is already running");
                    Ok(None)
                } else {
                    tracing::debug!("single-instance mutex acquired");
                    Ok(Some(Self { handle: h }))
                }
            }
            Err(e) => {
                tracing::error!("CreateMutexW failed: {}", e);
                Err(SingleInstanceError::CreateFailed(e.code().0 as u32))
            }
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        tracing::debug!("releasing single-instance mutex");
        // SAFETY: handle was obtained from CreateMutexW and we are the owner.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

// Single-instance enforcement via an exclusive `flock` (macOS).
//
// The Windows build uses the named mutex `Global\ImmichSync`. Here the lock
// is `immichsync.lock` in the local data dir; the kernel drops it when the
// process exits, crashed or not, so a stale file never blocks a launch.

use std::fs::File;
use std::os::fd::AsRawFd;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SingleInstanceError {
    #[error("could not resolve data dir: {0}")]
    DataDir(#[from] crate::config::ConfigError),

    #[error("could not open lock file: {0}")]
    Open(#[from] std::io::Error),
}

/// Holds the lock for the lifetime of the process. Drop closes the file,
/// which releases the lock.
pub struct SingleInstance {
    _file: File,
}

impl SingleInstance {
    /// Attempt to take the lock.
    ///
    /// - `Ok(Some(guard))` — lock taken; this is the first instance.
    /// - `Ok(None)`        — another instance holds it.
    pub fn acquire() -> Result<Option<Self>, SingleInstanceError> {
        let path = crate::config::Config::local_data_dir()?.join("immichsync.lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        // SAFETY: plain syscall on a descriptor we own.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(Self { _file: file }));
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(err.into())
        }
    }
}

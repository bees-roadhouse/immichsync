//! Graceful shutdown on SIGTERM / SIGINT (macOS).
//!
//! The Windows build catches `WM_CLOSE`/`WM_ENDSESSION` on a hidden window.
//! On macOS, logout, `kill`, and `launchctl stop` deliver SIGTERM, and a
//! terminal Ctrl-C delivers SIGINT. The handler only sets a flag, which is
//! all that is async-signal-safe, and `event_loop::pump` reports it to
//! `App::run`, which runs the normal shutdown path.
//!
//! SIGKILL cannot be caught; crash recovery (`reset_stale_uploading`) covers it.

use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;

static QUIT: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Error)]
pub enum ShutdownWindowError {
    #[error("failed to install signal handler: {0}")]
    Signal(std::io::Error),
}

extern "C" fn on_signal(_sig: libc::c_int) {
    QUIT.store(true, Ordering::SeqCst);
}

/// Install the SIGTERM/SIGINT handlers. Same name as the Windows hook so
/// `App::init` stays OS-agnostic.
pub fn install() -> Result<(), ShutdownWindowError> {
    for sig in [libc::SIGTERM, libc::SIGINT] {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SAFETY: the handler only touches an atomic.
        if unsafe { libc::signal(sig, handler) } == libc::SIG_ERR {
            return Err(ShutdownWindowError::Signal(std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// True once a termination signal has arrived.
pub fn quit_requested() -> bool {
    QUIT.load(Ordering::SeqCst)
}

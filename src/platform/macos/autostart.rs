// Start-at-login (macOS) ... not yet implemented.
//
// The real version registers `SMAppService.mainApp`, which needs a signed
// `.app` bundle, with a `~/Library/LaunchAgents` plist fallback (#60). Until
// then enabling it returns `Unsupported`, which callers log as a warning.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AutostartError {
    #[error("start-at-login is not implemented on macOS yet (#60)")]
    Unsupported,
}

/// Disabling is a no-op (nothing is ever registered); enabling is unsupported.
pub fn set_autostart(enabled: bool) -> Result<(), AutostartError> {
    if enabled {
        return Err(AutostartError::Unsupported);
    }
    Ok(())
}

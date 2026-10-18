// Start-at-login (macOS) ... not yet implemented.
//
// The real version registers `SMAppService.mainApp`, which needs a signed
// `.app` bundle, with a `~/Library/LaunchAgents` plist fallback (#60). Until
// then the setting is accepted and logged so the shared UI keeps working.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AutostartError {
    #[error("start-at-login is not implemented on macOS yet (#60)")]
    Unsupported,
}

/// Accept the setting without acting on it.
pub fn set_autostart(enabled: bool) -> Result<(), AutostartError> {
    if enabled {
        tracing::info!("Start at login requested; not implemented on macOS yet (#60)");
    }
    Ok(())
}

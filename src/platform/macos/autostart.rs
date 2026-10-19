// Start-at-login (macOS).
//
// A bundled build (`ImmichSync.app`) registers itself with
// `SMAppService.mainApp` (macOS 13+): it shows in System Settings → General →
// Login Items under its own name and icon, and the user can switch it off
// there. A bare binary (a dev build, or a bundle where registration fails)
// falls back to a per-user LaunchAgent plist in `~/Library/LaunchAgents`,
// which launchd reads at the next login. Neither path needs admin rights.
//
// Only one mechanism is ever left in place: registering one removes the
// other, so a dev build that later became a bundle doesn't start twice.

use std::path::{Path, PathBuf};

use objc2_service_management::{SMAppService, SMAppServiceStatus};
use thiserror::Error;
use tracing::{info, warn};

/// launchd label and plist name of the fallback agent.
pub const LAUNCH_AGENT_LABEL: &str = "com.beesroadhouse.immichsync";

#[derive(Debug, Error)]
pub enum AutostartError {
    #[error("could not resolve the home directory")]
    NoHome,

    #[error("could not resolve the running executable: {0}")]
    ExePath(std::io::Error),

    #[error("could not write or remove the LaunchAgent: {0}")]
    LaunchAgent(std::io::Error),

    #[error("could not remove the login item: {0}")]
    Unregister(String),
}

/// Enable or disable starting ImmichSync at login.
pub fn set_autostart(enabled: bool) -> Result<(), AutostartError> {
    let exe = std::env::current_exe().map_err(AutostartError::ExePath)?;
    let agents = launch_agents_dir()?;
    apply(enabled, &exe, &agents, set_main_app_service)
}

/// The decision logic, with SMAppService passed in so tests can fail it.
fn apply(
    enabled: bool,
    exe: &Path,
    agents: &Path,
    main_app_service: impl Fn(bool) -> Result<(), String>,
) -> Result<(), AutostartError> {
    if is_bundled(exe) {
        match main_app_service(enabled) {
            Ok(()) => {
                // SMAppService owns it now; drop any dev-build agent.
                remove_launch_agent(agents)?;
                return Ok(());
            }
            // Disabling must not report success while the login item is
            // still registered: the user would think it's off. Clear any
            // agent anyway, then surface the failure.
            Err(e) if !enabled => {
                remove_launch_agent(agents)?;
                return Err(AutostartError::Unregister(e));
            }
            Err(e) => warn!(
                error = %e,
                "SMAppService registration failed; falling back to a LaunchAgent"
            ),
        }
    } else if enabled {
        // A bare binary can't use SMAppService. Make sure a bundle's earlier
        // registration doesn't linger alongside the agent.
        let _ = main_app_service(false);
    }

    if enabled {
        write_launch_agent(agents, exe)
    } else {
        remove_launch_agent(agents)
    }
}

/// True when the executable sits at `<X>.app/Contents/MacOS/<exe>`.
fn is_bundled(exe: &Path) -> bool {
    let mut parents = exe.ancestors().skip(1);
    matches!(
        (parents.next(), parents.next(), parents.next()),
        (Some(macos), Some(contents), Some(app))
            if macos.file_name().is_some_and(|n| n == "MacOS")
                && contents.file_name().is_some_and(|n| n == "Contents")
                && app.extension().is_some_and(|e| e == "app")
    )
}

fn set_main_app_service(enabled: bool) -> Result<(), String> {
    // SAFETY: plain ServiceManagement calls on the shared main-app service.
    unsafe {
        let service = SMAppService::mainAppService();
        let status = service.status();
        let result = if enabled {
            if status == SMAppServiceStatus::Enabled {
                return Ok(());
            }
            service.registerAndReturnError()
        } else {
            if status == SMAppServiceStatus::NotRegistered || status == SMAppServiceStatus::NotFound
            {
                return Ok(());
            }
            service.unregisterAndReturnError()
        };
        result.map_err(|e| e.localizedDescription().to_string())?;
        if enabled && service.status() == SMAppServiceStatus::RequiresApproval {
            info!(
                "Login item registered; the user must approve it in System Settings → Login Items"
            );
        } else {
            info!(enabled, "Login item updated via SMAppService");
        }
    }
    Ok(())
}

fn launch_agents_dir() -> Result<PathBuf, AutostartError> {
    dirs::home_dir()
        .map(|h| h.join("Library").join("LaunchAgents"))
        .ok_or(AutostartError::NoHome)
}

fn launch_agent_path(agents_dir: &Path) -> PathBuf {
    agents_dir.join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

fn write_launch_agent(agents_dir: &Path, exe: &Path) -> Result<(), AutostartError> {
    std::fs::create_dir_all(agents_dir).map_err(AutostartError::LaunchAgent)?;
    let path = launch_agent_path(agents_dir);
    std::fs::write(&path, launch_agent_plist(exe)).map_err(AutostartError::LaunchAgent)?;
    info!(path = %path.display(), "LaunchAgent written; ImmichSync starts at next login");
    Ok(())
}

fn remove_launch_agent(agents_dir: &Path) -> Result<(), AutostartError> {
    let path = launch_agent_path(agents_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            info!(path = %path.display(), "LaunchAgent removed");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(AutostartError::LaunchAgent(e)),
    }
}

/// The agent runs the binary once per GUI login session.
fn launch_agent_plist(exe: &Path) -> String {
    let exe = xml_escape(&exe.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LAUNCH_AGENT_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>ProcessType</key>
	<string>Interactive</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_detection() {
        assert!(is_bundled(Path::new(
            "/Applications/ImmichSync.app/Contents/MacOS/immichsync"
        )));
        assert!(!is_bundled(Path::new("/Users/x/target/debug/immichsync")));
        assert!(!is_bundled(Path::new("/Contents/MacOS/immichsync")));
        assert!(!is_bundled(Path::new(
            "/Applications/ImmichSync/Contents/MacOS/immichsync"
        )));
    }

    #[test]
    fn plist_escapes_the_path_and_runs_at_load() {
        let plist = launch_agent_plist(Path::new("/tmp/A&B <x>/immichsync"));
        assert!(plist.contains("<string>/tmp/A&amp;B &lt;x&gt;/immichsync</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(plist.contains(&format!("<string>{LAUNCH_AGENT_LABEL}</string>")));
    }

    #[test]
    fn launch_agent_write_then_remove() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("Library/LaunchAgents");
        let exe = Path::new("/Applications/ImmichSync.app/Contents/MacOS/immichsync");

        write_launch_agent(&agents, exe).unwrap();
        let path = launch_agent_path(&agents);
        assert!(path.exists());
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains(exe.to_str().unwrap()));

        // launchd silently ignores a malformed plist; make sure it parses.
        let lint = std::process::Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&path)
            .status()
            .unwrap();
        assert!(lint.success(), "plutil -lint rejected the LaunchAgent");

        remove_launch_agent(&agents).unwrap();
        assert!(!path.exists());
        // Removing again is not an error.
        remove_launch_agent(&agents).unwrap();
    }

    const BUNDLED: &str = "/Applications/ImmichSync.app/Contents/MacOS/immichsync";

    #[test]
    fn bundled_disable_failure_is_an_error_and_clears_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("LaunchAgents");
        write_launch_agent(&agents, Path::new(BUNDLED)).unwrap();

        let r = apply(false, Path::new(BUNDLED), &agents, |_| Err("denied".into()));
        assert!(matches!(r, Err(AutostartError::Unregister(ref m)) if m == "denied"));
        assert!(!launch_agent_path(&agents).exists());
    }

    #[test]
    fn bundled_enable_failure_falls_back_to_an_agent() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("LaunchAgents");
        apply(true, Path::new(BUNDLED), &agents, |_| Err("nope".into())).unwrap();
        assert!(launch_agent_path(&agents).exists());
    }

    #[test]
    fn bundled_success_removes_a_stale_agent() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("LaunchAgents");
        write_launch_agent(&agents, Path::new(BUNDLED)).unwrap();
        apply(true, Path::new(BUNDLED), &agents, |_| Ok(())).unwrap();
        assert!(!launch_agent_path(&agents).exists());
    }
}

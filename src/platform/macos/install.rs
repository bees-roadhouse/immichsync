// Install location (macOS).
//
// A Mac app is a bundle the user drags into Applications; there is no
// self-install, Apps & Features entry, or shortcut. The running binary is
// the "installed" one, and window subprocesses re-exec it.

use std::path::PathBuf;

/// Path used to spawn `--window <x>` subprocesses: the running binary.
pub fn installed_exe_path() -> std::io::Result<PathBuf> {
    std::env::current_exe()
}

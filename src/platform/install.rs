// Self-install and legacy data migration.
//
// On first launch the running executable copies itself into the data directory
// (`%APPDATA%\bees-roadhouse\immichsync\immichsync.exe`) so that autostart and
// subprocess spawning always resolve to a stable, known path regardless of
// where the user originally downloaded the binary.

use std::path::PathBuf;

use thiserror::Error;
use tracing::{debug, info, warn};

use crate::config::Config;

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("could not determine data directory: {0}")]
    DataDir(#[from] crate::config::ConfigError),

    #[error("could not determine current executable: {0}")]
    CurrentExe(std::io::Error),

    #[error("failed to copy exe to `{dest}`: {source}")]
    Copy {
        dest: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to create directory `{path}`: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// The install directory — same as the data directory.
pub fn install_dir() -> Result<PathBuf, InstallError> {
    Ok(Config::data_dir()?)
}

/// Full path to the installed copy of the executable.
pub fn installed_exe_path() -> Result<PathBuf, InstallError> {
    Ok(install_dir()?.join("immichsync.exe"))
}

/// Returns `true` if the currently running executable is the installed copy.
pub fn is_running_installed() -> Result<bool, InstallError> {
    let current = std::env::current_exe().map_err(InstallError::CurrentExe)?;
    let installed = installed_exe_path()?;

    // Canonicalize both paths for a reliable comparison (resolves symlinks,
    // normalises case on Windows, etc.).
    let current_canon = std::fs::canonicalize(&current).unwrap_or(current);
    let installed_canon = std::fs::canonicalize(&installed).unwrap_or(installed);

    Ok(current_canon == installed_canon)
}

/// Copy the running executable into the install directory and write a
/// `version.txt` file alongside it containing the current version.
///
/// This is a no-op if the binary is already running from the installed path.
/// On Windows the copy will fail if the destination file is locked by another
/// running instance — callers should handle that gracefully.
pub fn install_exe() -> Result<(), InstallError> {
    if is_running_installed()? {
        debug!("Already running from installed path");
        return Ok(());
    }

    let current = std::env::current_exe().map_err(InstallError::CurrentExe)?;
    let dest = installed_exe_path()?;

    // Ensure the parent directory exists (data_dir creates it, but belt-and-suspenders).
    if let Some(parent) = dest.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|source| InstallError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }

    info!(
        src = %current.display(),
        dest = %dest.display(),
        "Installing exe to data directory"
    );

    std::fs::copy(&current, &dest).map_err(|source| InstallError::Copy {
        dest: dest.clone(),
        source,
    })?;

    // Write version.txt alongside the installed exe.
    write_version_file()?;

    Ok(())
}

/// Path to the `version.txt` file in the install directory.
fn version_file_path() -> Result<PathBuf, InstallError> {
    Ok(install_dir()?.join("version.txt"))
}

/// Write the current binary's version to `version.txt` in the install dir.
fn write_version_file() -> Result<(), InstallError> {
    let path = version_file_path()?;
    std::fs::write(&path, running_version())
        .map_err(|source| InstallError::Copy { dest: path, source })?;
    Ok(())
}

/// The version of the currently running binary (from Cargo.toml at build time).
pub fn running_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Read the version of the installed copy from `version.txt`.
///
/// Returns `None` if the file doesn't exist or can't be read.
pub fn installed_version() -> Option<String> {
    version_file_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Check if the running version is newer than the installed version.
///
/// Returns `Some((installed_ver, running_ver))` if an update is available.
/// Returns `None` if versions match, installed copy doesn't exist, or
/// the running copy is older/same.
pub fn is_update_available() -> Option<(String, String)> {
    let installed_str = installed_version()?;
    let running_str = running_version().to_string();

    let installed_ver = semver::Version::parse(&installed_str).ok()?;
    let running_ver = semver::Version::parse(&running_str).ok()?;

    if running_ver > installed_ver {
        Some((installed_str, running_str))
    } else {
        None
    }
}

/// Migrate data files from the legacy `%APPDATA%\ImmichSync\` directory to the
/// new `%APPDATA%\bees-roadhouse\immichsync\` path.
///
/// Only copies files that exist at the source and do NOT already exist at the
/// destination (preserves any data already created at the new location).
pub fn migrate_legacy_data() -> Result<(), InstallError> {
    let base = match dirs::config_dir() {
        Some(d) => d,
        None => return Ok(()), // can't determine APPDATA; nothing to migrate
    };

    let legacy_dir = base.join("ImmichSync");
    if !legacy_dir.exists() {
        debug!("No legacy data directory found, nothing to migrate");
        return Ok(());
    }

    let new_dir = Config::data_dir()?;

    // Individual files to migrate.
    let files = ["config.toml", "state.db", "state.db-wal", "state.db-shm"];

    for file in &files {
        let src = legacy_dir.join(file);
        let dest = new_dir.join(file);

        if src.exists() && !dest.exists() {
            info!(file, "Migrating legacy data file");
            if let Err(e) = std::fs::copy(&src, &dest) {
                warn!(file, error = %e, "Failed to migrate legacy file (continuing)");
            }
        }
    }

    // Migrate the logs directory.
    let legacy_logs = legacy_dir.join("logs");
    let new_logs = new_dir.join("logs");

    if legacy_logs.exists() && legacy_logs.is_dir() {
        if !new_logs.exists() {
            std::fs::create_dir_all(&new_logs).map_err(|source| InstallError::CreateDir {
                path: new_logs.clone(),
                source,
            })?;
        }

        if let Ok(entries) = std::fs::read_dir(&legacy_logs) {
            for entry in entries.flatten() {
                let src = entry.path();
                if src.is_file() {
                    if let Some(name) = src.file_name() {
                        let dest = new_logs.join(name);
                        if !dest.exists() {
                            if let Err(e) = std::fs::copy(&src, &dest) {
                                warn!(
                                    file = %name.to_string_lossy(),
                                    error = %e,
                                    "Failed to migrate legacy log file (continuing)"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    info!("Legacy data migration complete");
    Ok(())
}

/// Relaunch from the installed exe path, forwarding all command-line arguments.
///
/// On success this function does **not** return — the current process exits.
/// Returns an error only if the relaunch fails.
pub fn relaunch_installed() -> Result<(), InstallError> {
    let installed = installed_exe_path()?;

    // Forward all args except argv[0].
    let args: Vec<String> = std::env::args().skip(1).collect();

    info!(
        exe = %installed.display(),
        args = ?args,
        "Relaunching from installed path"
    );

    let status = std::process::Command::new(&installed)
        .args(&args)
        .status()?;

    std::process::exit(status.code().unwrap_or(0));
}

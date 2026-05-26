// Self-install and legacy data migration.
//
// On first launch the running executable copies itself into the install
// directory (`%LOCALAPPDATA%\Programs\immichsync\immichsync.exe`) so that
// autostart and subprocess spawning always resolve to a stable, known path
// regardless of where the user originally downloaded the binary.
//
// The install directory holds the binary + `version.txt` only. User data
// (config.toml, state.db, logs) lives elsewhere ... see `config.rs`.

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

/// The binary install directory: `%LOCALAPPDATA%\Programs\immichsync\`.
pub fn install_dir() -> Result<PathBuf, InstallError> {
    Ok(Config::install_dir()?)
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

/// Step 1 migration: pre-namespace `%APPDATA%\ImmichSync\` → split layout.
///
/// The very first releases stored everything under `%APPDATA%\ImmichSync\`
/// (no `bees-roadhouse` namespace). This routes:
///
/// - `config.toml` → `%APPDATA%\bees-roadhouse\immichsync\config.toml`
/// - `state.db*`   → `%LOCALAPPDATA%\bees-roadhouse\immichsync\state.db*`
/// - `logs\*`      → `%LOCALAPPDATA%\bees-roadhouse\immichsync\logs\*`
///
/// Only copies files that exist at the source and do NOT already exist at
/// the destination (preserves any data already created at the new location).
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

    let new_config_dir = Config::config_dir()?;
    let new_local_dir = Config::local_data_dir()?;

    migrate_pre_namespace_layout(&legacy_dir, &new_config_dir, &new_local_dir);
    info!("Legacy data migration (step 1) complete");
    Ok(())
}

/// Core of step 1 migration with explicit paths ... testable in isolation.
///
/// Copies (not moves) the legacy files because the pre-namespace dir might
/// still be in use by a downgraded build; copy is the safe primitive.
fn migrate_pre_namespace_layout(
    legacy_dir: &std::path::Path,
    new_config_dir: &std::path::Path,
    new_local_dir: &std::path::Path,
) {
    // config.toml → roaming.
    let cfg_src = legacy_dir.join("config.toml");
    let cfg_dest = new_config_dir.join("config.toml");
    if cfg_src.exists() && !cfg_dest.exists() {
        info!("Migrating legacy config.toml");
        if let Err(e) = std::fs::copy(&cfg_src, &cfg_dest) {
            warn!(error = %e, "Failed to migrate legacy config.toml (continuing)");
        }
    }

    // state.db (+ WAL/SHM sidecars) → local.
    for file in &["state.db", "state.db-wal", "state.db-shm"] {
        let src = legacy_dir.join(file);
        let dest = new_local_dir.join(file);
        if src.exists() && !dest.exists() {
            info!(file, "Migrating legacy state file");
            if let Err(e) = std::fs::copy(&src, &dest) {
                warn!(file, error = %e, "Failed to migrate legacy state file (continuing)");
            }
        }
    }

    // logs\* → local.
    let legacy_logs = legacy_dir.join("logs");
    let new_logs = new_local_dir.join("logs");
    if legacy_logs.exists() && legacy_logs.is_dir() {
        if !new_logs.exists() {
            if let Err(e) = std::fs::create_dir_all(&new_logs) {
                warn!(path = %new_logs.display(), error = %e, "Failed to create new logs dir");
                return;
            }
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
}

/// Step 2 migration: roaming-everything → Windows-standard split.
///
/// Versions 0.1.x stored binary + DB + logs all under
/// `%APPDATA%\bees-roadhouse\immichsync\`. Issue #20 splits that:
///
/// - `config.toml` stays put (already in roaming, correct location).
/// - `state.db*` → `%LOCALAPPDATA%\bees-roadhouse\immichsync\state.db*`.
/// - `logs\*`    → `%LOCALAPPDATA%\bees-roadhouse\immichsync\logs\*`.
/// - `immichsync.exe` + `version.txt` are left in the old roaming dir for
///   now ... they'll get cleaned up the next time the user accepts an
///   install/update prompt (which copies the new binary into the new
///   `%LOCALAPPDATA%\Programs\immichsync\` location). We don't delete from
///   under a running exe.
///
/// Idempotent: only moves files that exist at the source and don't already
/// exist at the destination.
pub fn migrate_to_split_layout() -> Result<(), InstallError> {
    let base = match dirs::config_dir() {
        Some(d) => d,
        None => return Ok(()),
    };

    let old_dir = base.join("bees-roadhouse").join("immichsync");
    if !old_dir.exists() {
        debug!("No legacy split-source directory found, nothing to migrate");
        return Ok(());
    }

    let new_local_dir = Config::local_data_dir()?;

    // Safety check: if the old dir IS the new local dir (e.g. someone has
    // %APPDATA% and %LOCALAPPDATA% pointed at the same place, or a future
    // tester monkeys with env vars), bail out cleanly to avoid moving a file
    // onto itself.
    if old_dir == new_local_dir {
        debug!("Old and new dirs are identical, nothing to migrate");
        return Ok(());
    }

    migrate_mixed_roaming_layout(&old_dir, &new_local_dir);
    info!("Split-layout migration (step 2) complete");
    Ok(())
}

/// Core of step 2 migration with explicit paths ... testable in isolation.
///
/// Uses move-with-copy-fallback so the user's machine-local state ends up
/// in the right volume even if `%APPDATA%` and `%LOCALAPPDATA%` live on
/// different drives (rare but possible on redirected profiles).
fn migrate_mixed_roaming_layout(old_dir: &std::path::Path, new_local_dir: &std::path::Path) {
    // state.db (+ sidecars) → local.
    for file in &["state.db", "state.db-wal", "state.db-shm"] {
        let src = old_dir.join(file);
        let dest = new_local_dir.join(file);
        if src.exists() && !dest.exists() {
            info!(file, "Migrating state file to local-data dir");
            match std::fs::rename(&src, &dest) {
                Ok(()) => {}
                Err(e) => {
                    warn!(file, error = %e, "rename failed, trying copy");
                    if let Err(e2) = std::fs::copy(&src, &dest) {
                        warn!(file, error = %e2, "Failed to migrate state file (continuing)");
                    } else {
                        let _ = std::fs::remove_file(&src);
                    }
                }
            }
        }
    }

    // logs\* → local.
    let old_logs = old_dir.join("logs");
    let new_logs = new_local_dir.join("logs");
    if old_logs.exists() && old_logs.is_dir() {
        if !new_logs.exists() {
            if let Err(e) = std::fs::create_dir_all(&new_logs) {
                warn!(path = %new_logs.display(), error = %e, "Failed to create new logs dir");
                return;
            }
        }
        if let Ok(entries) = std::fs::read_dir(&old_logs) {
            for entry in entries.flatten() {
                let src = entry.path();
                if src.is_file() {
                    if let Some(name) = src.file_name() {
                        let dest = new_logs.join(name);
                        if !dest.exists() {
                            if let Err(e) = std::fs::rename(&src, &dest) {
                                if let Err(e2) = std::fs::copy(&src, &dest) {
                                    warn!(
                                        file = %name.to_string_lossy(),
                                        rename_err = %e,
                                        copy_err = %e2,
                                        "Failed to migrate legacy log file (continuing)"
                                    );
                                } else {
                                    let _ = std::fs::remove_file(&src);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Try to remove the now-empty logs dir; ignore failures.
        let _ = std::fs::remove_dir(&old_logs);
    }
}

// ── Apps & Features registry (Uninstall key) ────────────────────────────

/// The subkey under `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall`
/// where ImmichSync registers itself.
pub const UNINSTALL_SUBKEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\ImmichSync";

/// The publisher string shown in Apps & Features (until we're code-signed,
/// at which point Windows uses the cert's CN instead).
pub const PUBLISHER: &str = "Bee's Roadhouse";

/// All registry values written to the Uninstall key, computed from the given
/// install location.
///
/// Factored out so unit tests can verify the construction without touching
/// the registry. Values follow the layout documented in MS-Help for
/// "Uninstall Registry Key Values" (the same set Windows scans to build the
/// Apps & Features list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallRegistryValues {
    /// REG_SZ. Shown as the row label.
    pub display_name: String,
    /// REG_SZ. Run when the user clicks Uninstall.
    pub uninstall_string: String,
    /// REG_SZ. Run by automation (winget, intune, etc.).
    pub quiet_uninstall_string: String,
    /// REG_SZ. From `env!("CARGO_PKG_VERSION")`.
    pub display_version: String,
    /// REG_SZ. Shown in the row metadata.
    pub publisher: String,
    /// REG_SZ. `"<exe>",0` so Windows extracts icon 0 from the exe.
    pub display_icon: String,
    /// REG_SZ. The install directory.
    pub install_location: String,
    /// REG_SZ. `YYYYMMDD`.
    pub install_date: String,
    /// REG_DWORD. Approximate footprint in KB.
    pub estimated_size_kb: u32,
    /// REG_SZ. "Visit website" link target.
    pub url_info_about: String,
    /// REG_SZ. "Support" link target.
    pub help_link: String,
    /// REG_DWORD. `1` to hide Modify button.
    pub no_modify: u32,
    /// REG_DWORD. `1` to hide Repair button.
    pub no_repair: u32,
}

/// Compute the registry values to write at install time.
///
/// `install_dir` is the directory holding `immichsync.exe`. `install_date`
/// is supplied as a `chrono::DateTime` so tests can pin it. `exe_size_bytes`
/// is the on-disk size of the installed binary (converted to KB).
pub fn build_uninstall_registry_values(
    install_dir: &std::path::Path,
    install_date: chrono::DateTime<chrono::Local>,
    exe_size_bytes: u64,
) -> UninstallRegistryValues {
    let exe = install_dir.join("immichsync.exe");
    let exe_str = exe.display().to_string();
    let install_str = install_dir.display().to_string();

    UninstallRegistryValues {
        display_name: "ImmichSync".to_string(),
        uninstall_string: format!("\"{exe_str}\" --uninstall"),
        quiet_uninstall_string: format!("\"{exe_str}\" --uninstall --silent"),
        display_version: running_version().to_string(),
        publisher: PUBLISHER.to_string(),
        display_icon: format!("\"{exe_str}\",0"),
        install_location: install_str,
        install_date: install_date.format("%Y%m%d").to_string(),
        estimated_size_kb: exe_size_bytes.div_ceil(1024) as u32,
        url_info_about: "https://github.com/bees-roadhouse/immichsync".to_string(),
        help_link: "https://github.com/bees-roadhouse/immichsync/issues".to_string(),
        no_modify: 1,
        no_repair: 1,
    }
}

/// Write the Apps & Features Uninstall registry block under
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\ImmichSync`.
///
/// Idempotent ... existing values are overwritten, missing values are
/// created. Returns the values that were written (useful for logging/tests).
pub fn write_uninstall_registry() -> Result<UninstallRegistryValues, InstallError> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
        REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let dir = install_dir()?;
    // Best-effort size: 0 is fine for the registry if the file isn't there yet.
    let size = std::fs::metadata(dir.join("immichsync.exe"))
        .map(|m| m.len())
        .unwrap_or(0);
    let values = build_uninstall_registry_values(&dir, chrono::Local::now(), size);

    // Open or create the subkey.
    let subkey_wide: Vec<u16> = UNINSTALL_SUBKEY
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut hkey = HKEY::default();
    let result = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_wide.as_ptr()),
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
    };
    if result.is_err() {
        return Err(InstallError::Io(std::io::Error::other(format!(
            "RegCreateKeyExW failed: 0x{:08x}",
            result.0
        ))));
    }

    // Closure to write a single REG_SZ value (UTF-16, null-terminated).
    let write_sz = |name: &str, value: &str| -> Result<(), InstallError> {
        let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let value_wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        let byte_len = value_wide.len() * 2;
        let r = unsafe {
            RegSetValueExW(
                hkey,
                PCWSTR(name_wide.as_ptr()),
                0,
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    value_wide.as_ptr() as *const u8,
                    byte_len,
                )),
            )
        };
        if r.is_err() {
            return Err(InstallError::Io(std::io::Error::other(format!(
                "RegSetValueExW({name}) failed: 0x{:08x}",
                r.0
            ))));
        }
        Ok(())
    };

    let write_dword = |name: &str, value: u32| -> Result<(), InstallError> {
        let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = value.to_le_bytes();
        let r =
            unsafe { RegSetValueExW(hkey, PCWSTR(name_wide.as_ptr()), 0, REG_DWORD, Some(&bytes)) };
        if r.is_err() {
            return Err(InstallError::Io(std::io::Error::other(format!(
                "RegSetValueExW({name}) failed: 0x{:08x}",
                r.0
            ))));
        }
        Ok(())
    };

    let result = (|| -> Result<(), InstallError> {
        write_sz("DisplayName", &values.display_name)?;
        write_sz("UninstallString", &values.uninstall_string)?;
        write_sz("QuietUninstallString", &values.quiet_uninstall_string)?;
        write_sz("DisplayVersion", &values.display_version)?;
        write_sz("Publisher", &values.publisher)?;
        write_sz("DisplayIcon", &values.display_icon)?;
        write_sz("InstallLocation", &values.install_location)?;
        write_sz("InstallDate", &values.install_date)?;
        write_sz("URLInfoAbout", &values.url_info_about)?;
        write_sz("HelpLink", &values.help_link)?;
        write_dword("EstimatedSize", values.estimated_size_kb)?;
        write_dword("NoModify", values.no_modify)?;
        write_dword("NoRepair", values.no_repair)?;
        Ok(())
    })();

    let _ = unsafe { RegCloseKey(hkey) };
    result?;
    info!("Wrote Apps & Features Uninstall registry block");
    Ok(values)
}

/// Delete the Apps & Features Uninstall registry block.
///
/// Called from the uninstall flow. Silently succeeds if the key doesn't
/// exist (treated the same as a successful delete).
pub fn delete_uninstall_registry() -> Result<(), InstallError> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER};

    let subkey_wide: Vec<u16> = UNINSTALL_SUBKEY
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let r = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(subkey_wide.as_ptr())) };
    match r.0 {
        0 => {
            info!("Removed Apps & Features Uninstall registry block");
            Ok(())
        }
        2 => {
            // ERROR_FILE_NOT_FOUND ... already gone.
            debug!("Uninstall registry block was already absent");
            Ok(())
        }
        e => Err(InstallError::Io(std::io::Error::other(format!(
            "RegDeleteTreeW failed: 0x{e:08x}"
        )))),
    }
}

// ── Legacy install cleanup (0.1.x → 0.2.0+ upgrade) ─────────────────────────

/// Directories where 0.1.x binaries lived. Used to detect leftover installs
/// when 0.2.x starts up for the first time after a manual download upgrade.
///
/// Returned in descending order of "newer" so callers that want to act on the
/// most recent leftover first can do so without re-sorting.
pub fn legacy_install_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(base) = dirs::config_dir() {
        // 0.1.x with bees-roadhouse namespace (later builds).
        dirs.push(base.join("bees-roadhouse").join("immichsync"));
        // Pre-namespace original layout (earliest builds).
        dirs.push(base.join("ImmichSync"));
    }
    dirs
}

/// Returns the subset of legacy install dirs that currently contain a binary.
pub fn detect_legacy_installs() -> Vec<PathBuf> {
    legacy_install_dirs()
        .into_iter()
        .filter(|d| d.join("immichsync.exe").exists())
        .collect()
}

/// Returns true if `candidate` lives under (or equals) any of `legacy_dirs`.
///
/// Pure function ... no filesystem access, exact-string normalization only.
/// Both inputs are compared in lowercase to handle Windows' case-insensitive
/// paths. Symlinks are not resolved (best effort).
pub fn path_is_under_legacy(candidate: &std::path::Path, legacy_dirs: &[PathBuf]) -> bool {
    let candidate_norm = candidate.to_string_lossy().to_lowercase();
    legacy_dirs.iter().any(|d| {
        let prefix = d.to_string_lossy().to_lowercase();
        candidate_norm == prefix || candidate_norm.starts_with(&format!("{prefix}\\"))
    })
}

/// Detect and clean up leftovers from any pre-0.2.0 install.
///
/// 0.1.x binaries live at one of two paths:
/// - `%APPDATA%\ImmichSync\immichsync.exe` (pre-namespace)
/// - `%APPDATA%\bees-roadhouse\immichsync\immichsync.exe` (later 0.1.x)
///
/// 0.2.0 moved the binary to `%LOCALAPPDATA%\Programs\immichsync\` but the
/// in-app update path only triggers cleanup when a binary already exists at
/// the new path. Manual upgrades via Releases download don't go through that
/// path, leaving:
/// - The old binary on disk.
/// - A stale HKCU\...\Run\ImmichSync autostart entry pointing at it.
/// - A stale Apps & Features Uninstall key with InstallLocation at the old dir.
/// - Worst case: the old binary still running, holding `state.db` open while
///   `migrate_to_split_layout` tries to move it.
///
/// This function is called from `main()` before the migrations run so file
/// locks are released first.
pub fn cleanup_legacy_install() {
    let legacy_dirs = legacy_install_dirs();
    let active = detect_legacy_installs();
    if active.is_empty() {
        debug!("No legacy install detected");
        return;
    }

    info!(
        count = active.len(),
        "Legacy install detected, cleaning up before migration"
    );

    // Kill any other immichsync.exe processes so file locks release. The PID
    // filter excludes ourselves; we don't care which path the other instances
    // are running from ... by definition they're either v0.1.x at a legacy
    // path (the case we're handling) or a stale v0.2.x that should also exit
    // before we install/migrate.
    kill_other_immichsync_processes();

    // Brief wait for kernel to flush handles. taskkill returns when the
    // process exit signal has been delivered, but locked files can linger
    // a few hundred ms after exit on Windows.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Remove the binaries (and version.txt) so they can't auto-launch later.
    for dir in &active {
        let binary = dir.join("immichsync.exe");
        if let Err(e) = std::fs::remove_file(&binary) {
            warn!(path = %binary.display(), error = %e, "Failed to remove legacy binary");
        } else {
            info!(path = %binary.display(), "Removed legacy binary");
        }
        let version_file = dir.join("version.txt");
        if version_file.exists() {
            let _ = std::fs::remove_file(&version_file);
        }
    }

    // Stale autostart / uninstall registry entries.
    cleanup_stale_autostart(&legacy_dirs);
    cleanup_stale_uninstall_key(&legacy_dirs);
}

/// Kill all immichsync.exe processes except ourselves.
///
/// Uses the same taskkill /FI "PID ne …" pattern as the install-update flow.
/// Best-effort: failures (including "no tasks running" when there are none)
/// are logged and ignored.
fn kill_other_immichsync_processes() {
    let our_pid = std::process::id();
    let pid_filter = format!("PID ne {our_pid}");
    match std::process::Command::new("taskkill")
        .args(["/F", "/FI", &pid_filter, "/IM", "immichsync.exe"])
        .output()
    {
        Ok(out) => {
            debug!(
                our_pid,
                stdout = %String::from_utf8_lossy(&out.stdout).trim(),
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "Killed other immichsync.exe processes"
            );
        }
        Err(e) => warn!(error = %e, "taskkill spawn failed"),
    }
}

/// If the HKCU Run\ImmichSync value points at a legacy path, remove it.
///
/// We don't touch entries that point at the new install dir or anywhere else
/// the user may have set up. Only legacy paths get cleared, so users who had
/// autostart disabled stay disabled.
fn cleanup_stale_autostart(legacy_dirs: &[PathBuf]) {
    let Some(current) = read_autostart_value() else {
        debug!("No autostart Run entry present");
        return;
    };
    let path = std::path::Path::new(&current);
    if !path_is_under_legacy(path, legacy_dirs) {
        debug!(path = %current, "Autostart entry not in legacy path, leaving in place");
        return;
    }
    info!(path = %current, "Removing stale autostart entry pointing at legacy path");
    if let Err(e) = crate::platform::autostart::set_autostart(false) {
        warn!(error = %e, "Failed to clear stale autostart entry");
    }
}

/// If the Apps & Features Uninstall key's InstallLocation is a legacy dir,
/// remove the whole subtree.
fn cleanup_stale_uninstall_key(legacy_dirs: &[PathBuf]) {
    let Some(install_location) = read_uninstall_install_location() else {
        debug!("No Apps & Features Uninstall key present");
        return;
    };
    let path = std::path::Path::new(&install_location);
    if !path_is_under_legacy(path, legacy_dirs) {
        debug!(
            path = %install_location,
            "Uninstall key InstallLocation not in legacy path, leaving in place"
        );
        return;
    }
    info!(
        path = %install_location,
        "Removing stale Apps & Features Uninstall key pointing at legacy path"
    );
    if let Err(e) = delete_uninstall_registry() {
        warn!(error = %e, "Failed to clear stale Uninstall registry key");
    }
}

/// Read the current `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\ImmichSync`
/// value as a UTF-8 string. Returns `None` if the key/value doesn't exist or
/// the read fails for any other reason.
fn read_autostart_value() -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    };

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0";
    const VALUE_NAME: &str = "ImmichSync\0";

    let key_wide: Vec<u16> = RUN_KEY.encode_utf16().collect();
    let value_wide: Vec<u16> = VALUE_NAME.encode_utf16().collect();

    let mut hkey = HKEY::default();
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_wide.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        )
    }
    .is_err()
    {
        return None;
    }

    let mut byte_len: u32 = 0;
    let probe = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(value_wide.as_ptr()),
            None,
            None,
            None,
            Some(&mut byte_len),
        )
    };
    if probe.is_err() || byte_len == 0 {
        let _ = unsafe { RegCloseKey(hkey) };
        return None;
    }

    let mut buf = vec![0u8; byte_len as usize];
    let r = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(value_wide.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut byte_len),
        )
    };
    let _ = unsafe { RegCloseKey(hkey) };
    if r.is_err() {
        return None;
    }

    decode_utf16_reg_sz(&buf)
}

/// Read `HKCU\...\Uninstall\ImmichSync\InstallLocation`. Returns `None` on
/// missing key, missing value, or any decode failure.
fn read_uninstall_install_location() -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    };

    let subkey_wide: Vec<u16> = UNINSTALL_SUBKEY
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value_wide: Vec<u16> = "InstallLocation\0".encode_utf16().collect();

    let mut hkey = HKEY::default();
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_wide.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        )
    }
    .is_err()
    {
        return None;
    }

    let mut byte_len: u32 = 0;
    let probe = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(value_wide.as_ptr()),
            None,
            None,
            None,
            Some(&mut byte_len),
        )
    };
    if probe.is_err() || byte_len == 0 {
        let _ = unsafe { RegCloseKey(hkey) };
        return None;
    }

    let mut buf = vec![0u8; byte_len as usize];
    let r = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(value_wide.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut byte_len),
        )
    };
    let _ = unsafe { RegCloseKey(hkey) };
    if r.is_err() {
        return None;
    }

    decode_utf16_reg_sz(&buf)
}

/// Decode a REG_SZ payload (UTF-16 LE, null-terminated, byte buffer) into
/// a Rust String. Strips the trailing null. Returns None on length mismatch
/// or invalid UTF-16.
fn decode_utf16_reg_sz(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let words: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    // Strip trailing nulls.
    let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
    String::from_utf16(&words[..end]).ok()
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

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn write_file(path: &std::path::Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create_dir_all");
        }
        std::fs::write(path, contents).expect("write");
    }

    #[test]
    fn migrate_pre_namespace_routes_files_to_correct_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let legacy = tmp.path().join("legacy");
        let new_cfg = tmp.path().join("new_cfg");
        let new_local = tmp.path().join("new_local");

        write_file(&legacy.join("config.toml"), b"[server]\nurl='x'");
        write_file(&legacy.join("state.db"), b"sqlite-bytes");
        write_file(&legacy.join("state.db-wal"), b"wal-bytes");
        write_file(&legacy.join("logs").join("a.log"), b"log-a");
        write_file(&legacy.join("logs").join("b.log"), b"log-b");

        std::fs::create_dir_all(&new_cfg).expect("create new_cfg");
        std::fs::create_dir_all(&new_local).expect("create new_local");

        migrate_pre_namespace_layout(&legacy, &new_cfg, &new_local);

        // config → roaming.
        assert_eq!(
            std::fs::read(new_cfg.join("config.toml")).expect("read config"),
            b"[server]\nurl='x'"
        );
        // state.db + sidecar → local.
        assert_eq!(
            std::fs::read(new_local.join("state.db")).expect("read state.db"),
            b"sqlite-bytes"
        );
        assert_eq!(
            std::fs::read(new_local.join("state.db-wal")).expect("read wal"),
            b"wal-bytes"
        );
        // logs → local.
        assert_eq!(
            std::fs::read(new_local.join("logs").join("a.log")).expect("read a.log"),
            b"log-a"
        );
        assert_eq!(
            std::fs::read(new_local.join("logs").join("b.log")).expect("read b.log"),
            b"log-b"
        );
    }

    #[test]
    fn migrate_pre_namespace_does_not_overwrite_existing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let legacy = tmp.path().join("legacy");
        let new_cfg = tmp.path().join("new_cfg");
        let new_local = tmp.path().join("new_local");

        write_file(&legacy.join("config.toml"), b"old-config");
        write_file(&new_cfg.join("config.toml"), b"new-config");
        write_file(&legacy.join("state.db"), b"old-db");
        write_file(&new_local.join("state.db"), b"new-db");

        migrate_pre_namespace_layout(&legacy, &new_cfg, &new_local);

        // Existing files at destination are preserved untouched.
        assert_eq!(
            std::fs::read(new_cfg.join("config.toml")).expect("read"),
            b"new-config"
        );
        assert_eq!(
            std::fs::read(new_local.join("state.db")).expect("read"),
            b"new-db"
        );
    }

    #[test]
    fn migrate_pre_namespace_is_noop_when_legacy_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let legacy = tmp.path().join("nonexistent");
        let new_cfg = tmp.path().join("new_cfg");
        let new_local = tmp.path().join("new_local");

        // Does not panic, does not error, simply does nothing.
        migrate_pre_namespace_layout(&legacy, &new_cfg, &new_local);
        assert!(!new_cfg.join("config.toml").exists());
        assert!(!new_local.join("state.db").exists());
    }

    #[test]
    fn migrate_mixed_roaming_moves_state_and_logs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old_dir = tmp.path().join("old_roaming");
        let new_local = tmp.path().join("new_local");

        write_file(&old_dir.join("state.db"), b"state-bytes");
        write_file(&old_dir.join("state.db-shm"), b"shm-bytes");
        write_file(&old_dir.join("logs").join("today.log"), b"today");
        write_file(&old_dir.join("config.toml"), b"cfg-should-stay");

        std::fs::create_dir_all(&new_local).expect("create new_local");

        migrate_mixed_roaming_layout(&old_dir, &new_local);

        // state files moved.
        assert!(!old_dir.join("state.db").exists());
        assert_eq!(
            std::fs::read(new_local.join("state.db")).expect("read"),
            b"state-bytes"
        );
        assert!(!old_dir.join("state.db-shm").exists());
        // logs moved.
        assert_eq!(
            std::fs::read(new_local.join("logs").join("today.log")).expect("read"),
            b"today"
        );
        // config.toml stays put (it belongs in roaming).
        assert_eq!(
            std::fs::read(old_dir.join("config.toml")).expect("read"),
            b"cfg-should-stay"
        );
    }

    #[test]
    fn migrate_mixed_roaming_preserves_existing_new_dest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old_dir = tmp.path().join("old_roaming");
        let new_local = tmp.path().join("new_local");

        write_file(&old_dir.join("state.db"), b"old");
        write_file(&new_local.join("state.db"), b"new");

        migrate_mixed_roaming_layout(&old_dir, &new_local);

        // Destination already had a state.db ... it wins, old left in place.
        assert_eq!(
            std::fs::read(new_local.join("state.db")).expect("read new"),
            b"new"
        );
        assert_eq!(
            std::fs::read(old_dir.join("state.db")).expect("read old"),
            b"old"
        );
    }

    #[test]
    fn migrate_mixed_roaming_is_noop_when_source_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old_dir = tmp.path().join("nonexistent");
        let new_local = tmp.path().join("new_local");
        std::fs::create_dir_all(&new_local).expect("create");

        migrate_mixed_roaming_layout(&old_dir, &new_local);
        assert!(!new_local.join("state.db").exists());
    }

    #[test]
    fn uninstall_registry_values_have_expected_shape() {
        // Pin the date so the test is deterministic.
        let date = chrono::Local
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .unwrap();
        let install_dir = PathBuf::from("C:\\Users\\test\\AppData\\Local\\Programs\\immichsync");
        // 1 MB exactly = 1024 KB.
        let values = build_uninstall_registry_values(&install_dir, date, 1024 * 1024);

        assert_eq!(values.display_name, "ImmichSync");
        assert_eq!(values.publisher, "Bee's Roadhouse");
        assert_eq!(
            values.uninstall_string,
            "\"C:\\Users\\test\\AppData\\Local\\Programs\\immichsync\\immichsync.exe\" --uninstall"
        );
        assert_eq!(
            values.quiet_uninstall_string,
            "\"C:\\Users\\test\\AppData\\Local\\Programs\\immichsync\\immichsync.exe\" --uninstall --silent"
        );
        assert_eq!(
            values.display_icon,
            "\"C:\\Users\\test\\AppData\\Local\\Programs\\immichsync\\immichsync.exe\",0"
        );
        assert_eq!(
            values.install_location,
            "C:\\Users\\test\\AppData\\Local\\Programs\\immichsync"
        );
        assert_eq!(values.install_date, "20260525");
        assert_eq!(values.estimated_size_kb, 1024);
        assert_eq!(values.display_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            values.url_info_about,
            "https://github.com/bees-roadhouse/immichsync"
        );
        assert_eq!(
            values.help_link,
            "https://github.com/bees-roadhouse/immichsync/issues"
        );
        assert_eq!(values.no_modify, 1);
        assert_eq!(values.no_repair, 1);
    }

    #[test]
    fn uninstall_registry_size_rounds_up_to_nearest_kb() {
        let date = chrono::Local
            .with_ymd_and_hms(2026, 5, 25, 0, 0, 0)
            .unwrap();
        let install_dir = PathBuf::from("C:\\test\\immichsync");

        // 1025 bytes → 2 KB (rounded up).
        let v1 = build_uninstall_registry_values(&install_dir, date, 1025);
        assert_eq!(v1.estimated_size_kb, 2);

        // 0 bytes → 0 KB.
        let v0 = build_uninstall_registry_values(&install_dir, date, 0);
        assert_eq!(v0.estimated_size_kb, 0);

        // Exactly 1024 bytes → 1 KB.
        let v_exact = build_uninstall_registry_values(&install_dir, date, 1024);
        assert_eq!(v_exact.estimated_size_kb, 1);
    }

    #[test]
    fn uninstall_subkey_matches_documented_path() {
        // Sanity: if anyone moves this path the Apps & Features integration
        // breaks silently. The path is load-bearing.
        assert_eq!(
            UNINSTALL_SUBKEY,
            "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\ImmichSync"
        );
    }

    #[test]
    fn path_is_under_legacy_matches_exact_and_descendants() {
        let legacy = vec![
            PathBuf::from("C:\\Users\\u\\AppData\\Roaming\\ImmichSync"),
            PathBuf::from("C:\\Users\\u\\AppData\\Roaming\\bees-roadhouse\\immichsync"),
        ];

        // Exact match.
        assert!(path_is_under_legacy(
            std::path::Path::new("C:\\Users\\u\\AppData\\Roaming\\ImmichSync"),
            &legacy
        ));

        // Descendant.
        assert!(path_is_under_legacy(
            std::path::Path::new("C:\\Users\\u\\AppData\\Roaming\\ImmichSync\\immichsync.exe"),
            &legacy
        ));
        assert!(path_is_under_legacy(
            std::path::Path::new(
                "C:\\Users\\u\\AppData\\Roaming\\bees-roadhouse\\immichsync\\state.db"
            ),
            &legacy
        ));

        // Case-insensitive (Windows).
        assert!(path_is_under_legacy(
            std::path::Path::new("c:\\users\\U\\appdata\\roaming\\immichsync\\immichsync.exe"),
            &legacy
        ));

        // New install location ... must NOT match.
        assert!(!path_is_under_legacy(
            std::path::Path::new(
                "C:\\Users\\u\\AppData\\Local\\Programs\\immichsync\\immichsync.exe"
            ),
            &legacy
        ));

        // Sibling dir that shares a prefix string ... must NOT match (the
        // backslash separator on the prefix guards against this).
        assert!(!path_is_under_legacy(
            std::path::Path::new("C:\\Users\\u\\AppData\\Roaming\\ImmichSyncBackup\\file.dat"),
            &legacy
        ));
    }

    #[test]
    fn decode_utf16_reg_sz_handles_typical_values() {
        // Encode "C:\\app" as UTF-16 LE + null terminator.
        let s = "C:\\app";
        let mut bytes: Vec<u8> = s.encode_utf16().flat_map(|w| w.to_le_bytes()).collect();
        bytes.extend_from_slice(&[0, 0]); // null terminator
        assert_eq!(decode_utf16_reg_sz(&bytes).as_deref(), Some(s));
    }

    #[test]
    fn decode_utf16_reg_sz_strips_trailing_nulls() {
        // "x" + two null words.
        let bytes: Vec<u8> = "x"
            .encode_utf16()
            .chain(std::iter::once(0))
            .chain(std::iter::once(0))
            .flat_map(|w| w.to_le_bytes())
            .collect();
        assert_eq!(decode_utf16_reg_sz(&bytes).as_deref(), Some("x"));
    }

    #[test]
    fn decode_utf16_reg_sz_rejects_odd_length() {
        assert_eq!(decode_utf16_reg_sz(&[1, 2, 3]), None);
        assert_eq!(decode_utf16_reg_sz(&[1]), None);
        assert_eq!(decode_utf16_reg_sz(&[]), None);
    }

    #[test]
    fn legacy_install_dirs_contains_both_known_layouts() {
        let dirs = legacy_install_dirs();
        // We can't assert the absolute path (depends on the runtime user),
        // but we can assert each leaf matches the expected layout.
        let names: Vec<String> = dirs
            .iter()
            .map(|d| d.display().to_string().to_lowercase())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.ends_with("\\immichsync") && !n.contains("bees-roadhouse")),
            "Expected pre-namespace dir ending in \\ImmichSync, got: {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| n.ends_with("\\bees-roadhouse\\immichsync")),
            "Expected mixed-roaming dir ending in \\bees-roadhouse\\immichsync, got: {names:?}"
        );
    }
}

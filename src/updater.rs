// Remote update checker and applier.
//
// Checks GitHub Releases for new versions, downloads the update, and applies
// it using a rename dance (can't overwrite/delete a running exe on Windows,
// but CAN rename it).

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

// ─── Constants ───────────────────────────────────────────────────────────────

const DEFAULT_REPO: &str = "gumbees/immichsync";
const ASSET_NAME: &str = "immichsync.exe";

/// Delay before the first automatic update check after startup.
pub const STARTUP_DELAY: Duration = Duration::from_secs(30);

/// Build the GitHub API URL for the latest release of the given repo.
fn github_api_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases/latest")
}

/// Build the check interval from config hours (0 = disabled).
pub fn check_interval_from_hours(hours: u32) -> Duration {
    if hours == 0 {
        Duration::from_secs(u64::MAX) // effectively disabled
    } else {
        Duration::from_secs(hours as u64 * 3600)
    }
}

// ─── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    body: Option<String>,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// Information about an available update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub new_version: String,
    pub download_url: String,
    pub size: u64,
    pub changelog: String,
}

/// Result of checking for an update.
#[derive(Debug)]
pub enum UpdateCheckResult {
    /// A newer version is available.
    Available(UpdateInfo),
    /// Already on the latest version.
    UpToDate,
    /// Check failed.
    Failed(String),
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("no matching asset '{ASSET_NAME}' in release")]
    NoAsset,

    #[error("invalid version in release tag: {0}")]
    InvalidVersion(String),

    #[error("file error: {0}")]
    File(#[from] std::io::Error),
}

// ─── Check ───────────────────────────────────────────────────────────────────

/// Check GitHub for a newer release.
///
/// `repo` is the GitHub `owner/repo` string (e.g. `"gumbees/immichsync"`).
/// Pass an empty string to use the default.
pub async fn check_for_update(repo: &str) -> UpdateCheckResult {
    let repo = if repo.is_empty() { DEFAULT_REPO } else { repo };
    match check_inner(repo).await {
        Ok(result) => result,
        Err(e) => UpdateCheckResult::Failed(e.to_string()),
    }
}

async fn check_inner(repo: &str) -> Result<UpdateCheckResult, UpdateError> {
    let url = github_api_url(repo);
    let client = reqwest::Client::builder()
        .user_agent(format!("ImmichSync/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()?;

    let release: GitHubRelease = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // Parse version from tag (strip leading 'v' if present).
    let tag = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let remote_ver = semver::Version::parse(tag)
        .map_err(|_| UpdateError::InvalidVersion(release.tag_name.clone()))?;

    let current_str = env!("CARGO_PKG_VERSION");
    let current_ver = semver::Version::parse(current_str)
        .map_err(|_| UpdateError::InvalidVersion(current_str.to_string()))?;

    if remote_ver <= current_ver {
        debug!(current = %current_ver, remote = %remote_ver, "Already up to date");
        return Ok(UpdateCheckResult::UpToDate);
    }

    // Find the exe asset.
    let asset = release
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(ASSET_NAME))
        .ok_or(UpdateError::NoAsset)?;

    let info = UpdateInfo {
        current_version: current_str.to_string(),
        new_version: remote_ver.to_string(),
        download_url: asset.browser_download_url.clone(),
        size: asset.size,
        changelog: release.body.unwrap_or_default(),
    };

    info!(
        current = %current_ver,
        new = %remote_ver,
        size = asset.size,
        "Update available"
    );

    Ok(UpdateCheckResult::Available(info))
}

// ─── Download ────────────────────────────────────────────────────────────────

/// Download the update binary to a temp file next to the current exe.
///
/// `progress_fn` is called with `(bytes_downloaded, total_bytes)`.
/// Returns the path to the downloaded file.
pub fn download_update_blocking<F>(url: &str, progress_fn: F) -> Result<PathBuf, UpdateError>
where
    F: Fn(u64, u64),
{
    let current_exe = std::env::current_exe().map_err(UpdateError::File)?;
    let dest = current_exe.with_extension("exe.new");

    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("ImmichSync/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(UpdateError::Network)?;

    let mut response = client
        .get(url)
        .send()
        .map_err(UpdateError::Network)?
        .error_for_status()
        .map_err(UpdateError::Network)?;

    let total = response.content_length().unwrap_or(0);

    let mut file = std::fs::File::create(&dest).map_err(UpdateError::File)?;
    let mut downloaded: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];

    use std::io::{Read, Write};
    loop {
        let n = response.read(&mut buf).map_err(UpdateError::File)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(UpdateError::File)?;
        downloaded += n as u64;
        progress_fn(downloaded, total);
    }

    file.flush().map_err(UpdateError::File)?;
    drop(file);

    info!(path = %dest.display(), bytes = downloaded, "Update downloaded");
    Ok(dest)
}

// ─── Apply ───────────────────────────────────────────────────────────────────

/// Apply the update using a rename dance:
/// 1. Rename current exe → .exe.old (works while running)
/// 2. Rename downloaded exe → current exe name
/// 3. Write version.txt with new version
pub fn apply_update(new_exe_path: &std::path::Path, new_version: &str) -> Result<(), UpdateError> {
    let current_exe = std::env::current_exe().map_err(UpdateError::File)?;
    let old_exe = current_exe.with_extension("exe.old");

    // Remove a leftover .old file if it exists.
    if old_exe.exists() {
        let _ = std::fs::remove_file(&old_exe);
    }

    // Step 1: Rename running exe → .old
    info!(
        from = %current_exe.display(),
        to = %old_exe.display(),
        "Renaming current exe to .old"
    );
    std::fs::rename(&current_exe, &old_exe).map_err(UpdateError::File)?;

    // Step 2: Move downloaded exe → current exe name
    info!(
        from = %new_exe_path.display(),
        to = %current_exe.display(),
        "Moving new exe into place"
    );
    std::fs::rename(new_exe_path, &current_exe).map_err(UpdateError::File)?;

    // Step 3: Write version.txt next to the exe.
    let version_file = current_exe
        .parent()
        .map(|p| p.join("version.txt"))
        .unwrap_or_else(|| PathBuf::from("version.txt"));
    let _ = std::fs::write(&version_file, new_version);

    info!(version = new_version, "Update applied");
    Ok(())
}

// ─── Cleanup ─────────────────────────────────────────────────────────────────

/// Delete leftover `.exe.old` from a previous update.
///
/// Call early in startup, before anything else needs the exe.
pub fn cleanup_old_exe() {
    let Ok(current_exe) = std::env::current_exe() else {
        return;
    };

    let old_exe = current_exe.with_extension("exe.old");
    if old_exe.exists() {
        match std::fs::remove_file(&old_exe) {
            Ok(()) => info!(path = %old_exe.display(), "Cleaned up old exe"),
            Err(e) => warn!(path = %old_exe.display(), error = %e, "Failed to clean up old exe"),
        }
    }
}

// ─── Background download + apply ─────────────────────────────────────────────

/// Download and apply an update without any UI.
///
/// Used by the periodic background update check in `app.rs`. Returns `Ok(())`
/// if the update was downloaded and applied (caller should prompt for restart),
/// or an error if anything failed.
pub fn download_and_apply(info: &UpdateInfo) -> Result<(), UpdateError> {
    info!(version = %info.new_version, "Background: downloading update");
    let path = download_update_blocking(&info.download_url, |_, _| {})?;
    apply_update(&path, &info.new_version)?;
    info!(version = %info.new_version, "Background: update applied");
    Ok(())
}

// ─── Relaunch ────────────────────────────────────────────────────────────────

/// Relaunch the current exe (after update) and exit this process.
pub fn relaunch_self() -> ! {
    let exe = std::env::current_exe().expect("current_exe");
    let args: Vec<String> = std::env::args().skip(1).collect();

    info!(exe = %exe.display(), "Relaunching after update");

    let _ = std::process::Command::new(&exe).args(&args).spawn();
    std::process::exit(0);
}

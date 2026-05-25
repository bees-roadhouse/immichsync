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

const DEFAULT_REPO: &str = "bees-roadhouse/immichsync";
const ASSET_NAME: &str = "immichsync.exe";

/// Delay before the first automatic update check after startup.
pub const STARTUP_DELAY: Duration = Duration::from_secs(30);

/// Build the check interval from config hours (0 = disabled).
pub fn check_interval_from_hours(hours: u32) -> Duration {
    if hours == 0 {
        Duration::from_secs(u64::MAX) // effectively disabled
    } else {
        Duration::from_secs(hours as u64 * 3600)
    }
}

// ─── Repo + channel parsing ─────────────────────────────────────────────────

/// Validate a GitHub `owner/repo` string.
///
/// Accepts ASCII alphanumerics plus `.`, `_`, `-` in each segment. Rejects
/// URLs, SSH refs, enterprise hostnames, and anything else that's not a bare
/// `owner/repo`. v1 is public github.com only.
pub fn is_valid_repo(s: &str) -> bool {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 2 {
        return false;
    }
    parts.iter().all(|seg| {
        !seg.is_empty()
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    })
}

/// Validate a pinned tag of the form `vX.Y.Z` or `vX.Y.Z-prerelease`.
fn is_valid_tag_pin(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('v') else {
        return false;
    };
    // Split off optional `-prerelease` suffix first.
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    if !parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    if let Some(p) = pre {
        if p.is_empty()
            || !p
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return false;
        }
    }
    true
}

/// Update channel resolved from config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Channel {
    /// Track the latest published release.
    Latest,
    /// Track latest including prereleases.
    Prerelease,
    /// Pin to a specific tag.
    Tag(String),
    /// Update checks disabled.
    None,
}

impl Channel {
    /// Parse a channel string from config.
    ///
    /// Returns `None` if the string is not a recognised channel or valid tag
    /// pin. Callers should treat unparseable channels as "disable for the
    /// session" and log a warning.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        match s {
            "latest" | "" => Some(Channel::Latest),
            "prerelease" => Some(Channel::Prerelease),
            "none" => Some(Channel::None),
            other if is_valid_tag_pin(other) => Some(Channel::Tag(other.to_string())),
            _ => None,
        }
    }
}

/// Build the GitHub API URL for the resolved channel.
fn channel_api_url(repo: &str, channel: &Channel) -> Option<String> {
    match channel {
        Channel::Latest => Some(format!(
            "https://api.github.com/repos/{repo}/releases/latest"
        )),
        Channel::Prerelease => Some(format!(
            "https://api.github.com/repos/{repo}/releases?per_page=10"
        )),
        Channel::Tag(tag) => Some(format!(
            "https://api.github.com/repos/{repo}/releases/tags/{tag}"
        )),
        Channel::None => None,
    }
}

// ─── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    body: Option<String>,
    assets: Vec<GitHubAsset>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

#[derive(Debug, Deserialize)]
struct GitHubRepoInfo {
    #[serde(default)]
    private: bool,
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

    #[error("no matching release for channel")]
    NoRelease,

    #[error("repo is private or not found: {0}")]
    PrivateOrMissing(String),

    #[error("file error: {0}")]
    File(#[from] std::io::Error),
}

// ─── Check ───────────────────────────────────────────────────────────────────

/// Check GitHub for a newer release on the given channel.
///
/// `repo` is the GitHub `owner/repo` string. Empty falls back to the
/// compiled-in default. `channel_str` is the raw config value
/// (`"latest"`, `"prerelease"`, `"none"`, or `"vX.Y.Z"`). Unparseable
/// channels and invalid repos resolve to `Failed(...)`; the caller
/// should log and disable updates for the session.
pub async fn check_for_update(repo: &str, channel_str: &str) -> UpdateCheckResult {
    let repo = if repo.is_empty() { DEFAULT_REPO } else { repo };

    if !is_valid_repo(repo) {
        return UpdateCheckResult::Failed(format!("invalid repo format: {repo}"));
    }

    let channel = match Channel::parse(channel_str) {
        Some(c) => c,
        None => {
            return UpdateCheckResult::Failed(format!(
                "invalid update channel: '{channel_str}' (expected 'latest', 'prerelease', 'none', or 'vX.Y.Z')"
            ));
        }
    };

    if channel == Channel::None {
        debug!("Update channel is 'none', skipping check");
        return UpdateCheckResult::UpToDate;
    }

    match check_inner(repo, &channel).await {
        Ok(result) => result,
        Err(e) => UpdateCheckResult::Failed(e.to_string()),
    }
}

async fn check_inner(repo: &str, channel: &Channel) -> Result<UpdateCheckResult, UpdateError> {
    let client = reqwest::Client::builder()
        .user_agent(format!("ImmichSync/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()?;

    // Pre-flight: confirm the repo is public and reachable. If it 404s or
    // comes back private, disable updates for the session.
    let repo_url = format!("https://api.github.com/repos/{repo}");
    let repo_resp = client.get(&repo_url).send().await?;
    if repo_resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(UpdateError::PrivateOrMissing(repo.to_string()));
    }
    let repo_info: GitHubRepoInfo = repo_resp.error_for_status()?.json().await?;
    if repo_info.private {
        return Err(UpdateError::PrivateOrMissing(repo.to_string()));
    }

    // Resolve the release endpoint for this channel.
    let url = channel_api_url(repo, channel).expect("None channel handled above");

    let release = match channel {
        Channel::Prerelease => {
            // List endpoint returns an array; pick the first non-draft entry.
            let releases: Vec<GitHubRelease> = client
                .get(&url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            releases
                .into_iter()
                .find(|r| !r.draft)
                .ok_or(UpdateError::NoRelease)?
        }
        Channel::Latest | Channel::Tag(_) => {
            let resp = client.get(&url).send().await?;
            if matches!(channel, Channel::Tag(_)) && resp.status() == reqwest::StatusCode::NOT_FOUND
            {
                return Err(UpdateError::NoRelease);
            }
            resp.error_for_status()?.json::<GitHubRelease>().await?
        }
        Channel::None => unreachable!(),
    };

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

    // No-downgrade policy: if the resolved release is at or below the running
    // binary, we just stay where we are (regardless of channel).
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
        prerelease = release.prerelease,
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

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_repo_accepts_owner_slash_repo() {
        assert!(is_valid_repo("bees-roadhouse/immichsync"));
        assert!(is_valid_repo("gumbees/immichsync"));
        assert!(is_valid_repo("user.name/repo.name"));
        assert!(is_valid_repo("u_1/r_2"));
        assert!(is_valid_repo("a/b"));
    }

    #[test]
    fn valid_repo_rejects_garbage() {
        assert!(!is_valid_repo(""));
        assert!(!is_valid_repo("no-slash"));
        assert!(!is_valid_repo("/leading-slash"));
        assert!(!is_valid_repo("trailing-slash/"));
        assert!(!is_valid_repo("too/many/slashes"));
        assert!(!is_valid_repo("https://github.com/owner/repo"));
        assert!(!is_valid_repo("git@github.com:owner/repo"));
        assert!(!is_valid_repo("owner repo/has spaces"));
        assert!(!is_valid_repo("owner/repo$"));
    }

    #[test]
    fn channel_parse_handles_known_literals() {
        assert_eq!(Channel::parse("latest"), Some(Channel::Latest));
        assert_eq!(Channel::parse(""), Some(Channel::Latest));
        assert_eq!(Channel::parse("prerelease"), Some(Channel::Prerelease));
        assert_eq!(Channel::parse("none"), Some(Channel::None));
        assert_eq!(Channel::parse("  latest  "), Some(Channel::Latest));
    }

    #[test]
    fn channel_parse_accepts_tag_pins() {
        assert_eq!(
            Channel::parse("v0.1.8"),
            Some(Channel::Tag("v0.1.8".to_string()))
        );
        assert_eq!(
            Channel::parse("v1.0.0-rc.1"),
            Some(Channel::Tag("v1.0.0-rc.1".to_string()))
        );
        assert_eq!(
            Channel::parse("v10.20.30"),
            Some(Channel::Tag("v10.20.30".to_string()))
        );
    }

    #[test]
    fn channel_parse_rejects_bogus() {
        assert_eq!(Channel::parse("stable"), None);
        assert_eq!(Channel::parse("1.0.0"), None); // missing 'v'
        assert_eq!(Channel::parse("v1.0"), None); // not three segments
        assert_eq!(Channel::parse("vX.Y.Z"), None); // non-numeric
        assert_eq!(Channel::parse("v1.0.0-"), None); // empty prerelease
    }

    #[test]
    fn channel_api_url_picks_right_endpoint() {
        let repo = "bees-roadhouse/immichsync";
        assert_eq!(
            channel_api_url(repo, &Channel::Latest),
            Some("https://api.github.com/repos/bees-roadhouse/immichsync/releases/latest".into())
        );
        assert!(channel_api_url(repo, &Channel::Prerelease)
            .unwrap()
            .contains("/releases?"));
        assert_eq!(
            channel_api_url(repo, &Channel::Tag("v0.1.8".into())),
            Some(
                "https://api.github.com/repos/bees-roadhouse/immichsync/releases/tags/v0.1.8"
                    .into()
            )
        );
        assert_eq!(channel_api_url(repo, &Channel::None), None);
    }

    #[test]
    fn check_interval_zero_is_disabled() {
        assert_eq!(check_interval_from_hours(0), Duration::from_secs(u64::MAX));
        assert_eq!(check_interval_from_hours(1), Duration::from_secs(3600));
        assert_eq!(check_interval_from_hours(24), Duration::from_secs(86400));
    }
}

// Remote update checker and applier.
//
// Checks GitHub Releases for new versions, downloads the update, and applies
// it. Windows uses a rename dance (can't overwrite/delete a running exe, but
// CAN rename it); macOS swaps the whole `.app` bundle from the release zip,
// see platform::bundle_update (#75).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info};

// ─── Constants ───────────────────────────────────────────────────────────────

const DEFAULT_REPO: &str = "bees-roadhouse/immichsync";

/// Which release asset this build updates from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// `immichsync.exe`
    Windows,
    /// `ImmichSync-<version>-macos-arm64.zip` from packaging/macos/make-app.sh
    MacArm64,
}

impl Target {
    /// The target of the running build, if releases carry an asset for it.
    pub fn current() -> Option<Self> {
        if cfg!(windows) {
            Some(Target::Windows)
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some(Target::MacArm64)
        } else {
            None
        }
    }

    /// The asset name this target expects in release `version`.
    pub fn asset_name(self, version: &semver::Version) -> String {
        match self {
            Target::Windows => "immichsync.exe".to_string(),
            Target::MacArm64 => format!("ImmichSync-{version}-macos-arm64.zip"),
        }
    }
}

/// Pick this target's asset out of a release, by exact name.
fn select_asset<'a>(
    assets: &'a [GitHubAsset],
    target: Target,
    version: &semver::Version,
) -> Option<&'a GitHubAsset> {
    let want = target.asset_name(version);
    assets.iter().find(|a| a.name.eq_ignore_ascii_case(&want))
}

/// Set once this process has a newer version on disk that it isn't running
/// yet (applied here, or reported applied by the update dialog).
static UPDATE_APPLIED: AtomicBool = AtomicBool::new(false);

/// Record that an update is on disk and waits for a restart (#128).
pub fn note_update_applied() {
    UPDATE_APPLIED.store(true, Ordering::SeqCst);
}

/// The executable to spawn a window subprocess from. Normally `default`;
/// after an update is applied but before restart, the previous image, so
/// Settings and the logs stay the same version as the running tray (and,
/// on ad-hoc macOS builds, the same code identity, so the Keychain doesn't
/// prompt early). Falls back to `default` if the old image is gone.
pub fn window_exe(default: PathBuf) -> PathBuf {
    if !UPDATE_APPLIED.load(Ordering::SeqCst) {
        return default;
    }
    match previous_image(&default) {
        Some(old) if old.exists() => {
            debug!(exe = %old.display(), "Update pending restart: windows run the previous image");
            old
        }
        _ => {
            info!("Update pending restart but the previous image is gone; windows run the new one");
            default
        }
    }
}

/// Where the image `exe` was parked by an update: `immichsync.exe.old` on
/// Windows; inside `.ImmichSync.app.old` on macOS.
fn previous_image(exe: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let bundle = crate::platform::bundle_update::bundle_of(exe)?;
        let relative = exe.strip_prefix(&bundle).ok()?;
        Some(crate::platform::bundle_update::old_bundle_path(&bundle).join(relative))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(exe.with_extension("exe.old"))
    }
}

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
    /// `sha256:<hex>`, computed by GitHub at upload. Missing on assets
    /// uploaded before GitHub added it (mid-2025).
    #[serde(default)]
    digest: Option<String>,
}

/// Information about an available update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub new_version: String,
    pub download_url: String,
    pub size: u64,
    pub changelog: String,
    /// The asset's `sha256:<hex>` from the Releases API, checked after the
    /// download (#129). `default` so an older info file still parses.
    #[serde(default)]
    pub digest: Option<String>,
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

    #[error("no matching asset '{0}' in release")]
    NoAsset(String),

    #[error("no release asset is published for this platform")]
    UnsupportedPlatform,

    #[error("the download doesn't match the release's checksum (expected {expected}, got {actual}); nothing was installed")]
    DigestMismatch { expected: String, actual: String },

    #[cfg(target_os = "macos")]
    #[error(transparent)]
    Bundle(#[from] crate::platform::bundle_update::BundleError),

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

    // Find this platform's asset.
    let target = Target::current().ok_or(UpdateError::UnsupportedPlatform)?;
    let asset = select_asset(&release.assets, target, &remote_ver)
        .ok_or_else(|| UpdateError::NoAsset(target.asset_name(&remote_ver)))?;

    let info = UpdateInfo {
        current_version: current_str.to_string(),
        new_version: remote_ver.to_string(),
        download_url: asset.browser_download_url.clone(),
        size: asset.size,
        changelog: release.body.unwrap_or_default(),
        digest: asset.digest.clone(),
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

/// Where the download goes. Windows: `immichsync.exe.new` beside the exe,
/// so the rename stays on one volume. macOS: the zip in $TMPDIR, after
/// checking the bundle can be replaced at all, so a read-only or
/// translocated copy says so before downloading anything.
fn download_dest() -> Result<PathBuf, UpdateError> {
    #[cfg(target_os = "macos")]
    {
        let bundle = crate::platform::bundle_update::running_bundle()?;
        crate::platform::bundle_update::preflight(&bundle)?;
        Ok(std::env::temp_dir().join("ImmichSync-update.zip"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let current_exe = std::env::current_exe().map_err(UpdateError::File)?;
        Ok(current_exe.with_extension("exe.new"))
    }
}

/// Compare the download's SHA-256 with the release asset's digest.
///
/// A missing digest (an asset uploaded before GitHub computed them) or one
/// in another algorithm is logged and allowed: the download still came over
/// HTTPS from GitHub, which is all an update relied on before #129. Our own
/// releases from v0.3.0 on all carry `sha256:`.
fn check_digest(expected: Option<&str>, actual_sha256_hex: &str) -> Result<(), UpdateError> {
    let Some(expected) = expected else {
        tracing::warn!("Release asset has no digest; skipping the checksum check");
        return Ok(());
    };
    let Some(hex) = expected.strip_prefix("sha256:") else {
        tracing::warn!(
            digest = expected,
            "Unrecognised digest algorithm; skipping the checksum check"
        );
        return Ok(());
    };
    if hex.eq_ignore_ascii_case(actual_sha256_hex) {
        debug!("Update checksum matches the release");
        Ok(())
    } else {
        Err(UpdateError::DigestMismatch {
            expected: expected.to_string(),
            actual: format!("sha256:{actual_sha256_hex}"),
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Download the update to a temp file (see `download_dest`), hashing it on
/// the way and refusing it (file deleted) if it doesn't match `digest`.
///
/// `progress_fn` is called with `(bytes_downloaded, total_bytes)`.
/// Returns the path to the downloaded file.
pub fn download_update_blocking<F>(
    url: &str,
    digest: Option<&str>,
    progress_fn: F,
) -> Result<PathBuf, UpdateError>
where
    F: Fn(u64, u64),
{
    use sha2::Digest;
    let dest = download_dest()?;

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
    let mut sha = sha2::Sha256::new();

    use std::io::{Read, Write};
    loop {
        let n = response.read(&mut buf).map_err(UpdateError::File)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(UpdateError::File)?;
        sha.update(&buf[..n]);
        downloaded += n as u64;
        progress_fn(downloaded, total);
    }

    file.flush().map_err(UpdateError::File)?;
    drop(file);

    if let Err(e) = check_digest(digest, &hex(&sha.finalize())) {
        let _ = std::fs::remove_file(&dest);
        return Err(e);
    }

    info!(path = %dest.display(), bytes = downloaded, "Update downloaded");
    Ok(dest)
}

// ─── Apply ───────────────────────────────────────────────────────────────────

/// Apply the update on macOS: unpack the zip beside the running bundle,
/// verify it, and swap it in (platform::bundle_update::apply_zip).
#[cfg(target_os = "macos")]
pub fn apply_update(zip_path: &std::path::Path, new_version: &str) -> Result<(), UpdateError> {
    let bundle = crate::platform::bundle_update::running_bundle()?;
    let result = crate::platform::bundle_update::apply_zip(zip_path, &bundle);
    let _ = std::fs::remove_file(zip_path);
    result?;
    note_update_applied();
    info!(version = new_version, "Update applied");
    Ok(())
}

/// Apply the update using a rename dance:
/// 1. Rename current exe → .exe.old (works while running)
/// 2. Rename downloaded exe → current exe name
/// 3. Write version.txt with new version
#[cfg(not(target_os = "macos"))]
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

    note_update_applied();
    info!(version = new_version, "Update applied");
    Ok(())
}

// ─── Cleanup ─────────────────────────────────────────────────────────────────

/// Delete leftover `.exe.old` from a previous update.
///
/// Call early in startup, before anything else needs the exe.
#[cfg(windows)]
pub fn cleanup_old_exe() {
    let Ok(current_exe) = std::env::current_exe() else {
        return;
    };

    let old_exe = current_exe.with_extension("exe.old");
    if old_exe.exists() {
        match std::fs::remove_file(&old_exe) {
            Ok(()) => info!(path = %old_exe.display(), "Cleaned up old exe"),
            Err(e) => {
                tracing::warn!(path = %old_exe.display(), error = %e, "Failed to clean up old exe")
            }
        }
    }
}

/// Delete the parked `.ImmichSync.app.old` (and any half-unpacked staging
/// dir) from a previous update. The new instance calls this at startup.
#[cfg(target_os = "macos")]
pub fn cleanup_old_bundle() {
    if let Ok(bundle) = crate::platform::bundle_update::running_bundle() {
        crate::platform::bundle_update::cleanup(&bundle);
    }
}

/// The version now on disk if it differs from the running one: an update
/// applied with its restart deferred (#127). Windows reads `version.txt`
/// beside the exe (`apply_update` writes it); macOS reads the running
/// bundle's CFBundleShortVersionString (the swap replaced it).
pub fn pending_restart_version() -> Option<String> {
    newer_on_disk(version_on_disk().as_deref(), env!("CARGO_PKG_VERSION"))
}

fn newer_on_disk(on_disk: Option<&str>, running: &str) -> Option<String> {
    let on_disk = on_disk?.trim();
    let disk = semver::Version::parse(on_disk).ok()?;
    let running = semver::Version::parse(running).ok()?;
    (disk > running).then(|| disk.to_string())
}

#[cfg(target_os = "macos")]
fn version_on_disk() -> Option<String> {
    let bundle = crate::platform::bundle_update::running_bundle().ok()?;
    let out = std::process::Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(bundle.join("Contents/Info.plist"))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(not(target_os = "macos"))]
fn version_on_disk() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    std::fs::read_to_string(exe.parent()?.join("version.txt")).ok()
}

// ─── Background download + apply ─────────────────────────────────────────────

/// Download and apply an update without any UI.
///
/// Used by the periodic background update check in `app.rs`. Returns `Ok(())`
/// if the update was downloaded and applied (caller should prompt for restart),
/// or an error if anything failed.
pub fn download_and_apply(info: &UpdateInfo) -> Result<(), UpdateError> {
    info!(version = %info.new_version, "Background: downloading update");
    let path = download_update_blocking(&info.download_url, info.digest.as_deref(), |_, _| {})?;
    apply_update(&path, &info.new_version)?;
    info!(version = %info.new_version, "Background: update applied");
    Ok(())
}

// ─── Relaunch ────────────────────────────────────────────────────────────────

/// Relaunch the current exe (after update) and exit this process. On macOS
/// the bundle relaunches through `open` once this process is gone.
pub fn relaunch_self() -> ! {
    #[cfg(target_os = "macos")]
    if let Ok(bundle) = crate::platform::bundle_update::running_bundle() {
        crate::platform::bundle_update::relaunch(&bundle);
    }

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

    fn asset(name: &str) -> GitHubAsset {
        GitHubAsset {
            name: name.to_string(),
            browser_download_url: format!("https://example.invalid/{name}"),
            size: 1,
            digest: None,
        }
    }

    #[test]
    fn select_asset_picks_each_platforms_own_file() {
        let v = semver::Version::parse("0.3.0").unwrap();
        let assets = [
            asset("immichsync.exe"),
            asset("ImmichSync-0.3.0-macos-arm64.zip"),
        ];
        assert_eq!(
            select_asset(&assets, Target::Windows, &v).unwrap().name,
            "immichsync.exe"
        );
        assert_eq!(
            select_asset(&assets, Target::MacArm64, &v).unwrap().name,
            "ImmichSync-0.3.0-macos-arm64.zip"
        );
    }

    #[test]
    fn macos_never_takes_the_exe_or_another_versions_zip() {
        let v = semver::Version::parse("0.3.0").unwrap();
        let exe_only = [asset("immichsync.exe")];
        assert!(select_asset(&exe_only, Target::MacArm64, &v).is_none());
        let stale = [
            asset("immichsync.exe"),
            asset("ImmichSync-0.2.9-macos-arm64.zip"),
        ];
        assert!(select_asset(&stale, Target::MacArm64, &v).is_none());
        let mac_only = [asset("ImmichSync-0.3.0-macos-arm64.zip")];
        assert!(select_asset(&mac_only, Target::Windows, &v).is_none());
    }

    #[test]
    fn current_target_matches_the_build() {
        #[cfg(windows)]
        assert_eq!(Target::current(), Some(Target::Windows));
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(Target::current(), Some(Target::MacArm64));
        assert_eq!(
            Target::current().is_some(),
            crate::platform::SELF_UPDATE_SUPPORTED
        );
    }

    #[test]
    fn digest_check_accepts_a_match_and_refuses_a_mismatch() {
        // sha256("abc")
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(check_digest(Some(&format!("sha256:{abc}")), abc).is_ok());
        assert!(check_digest(Some(&format!("sha256:{}", abc.to_uppercase())), abc).is_ok());
        let err = check_digest(Some(&format!("sha256:{abc}")), &"0".repeat(64)).unwrap_err();
        assert!(matches!(err, UpdateError::DigestMismatch { .. }), "{err}");
    }

    #[test]
    fn digest_check_allows_a_missing_or_unknown_digest() {
        assert!(check_digest(None, "00").is_ok());
        assert!(check_digest(Some("sha512:ffff"), "00").is_ok());
    }

    #[test]
    fn hex_and_sha256_agree_with_a_known_vector() {
        use sha2::Digest;
        assert_eq!(
            hex(&sha2::Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn release_assets_carry_their_digest_through() {
        let json = r#"{"name":"immichsync.exe","browser_download_url":"u","size":1,
            "digest":"sha256:89204113393b596ec7d3dbc6c0b72d176b792f05ecca5cffc03a2025dbcc0ee2"}"#;
        let a: GitHubAsset = serde_json::from_str(json).unwrap();
        assert!(a.digest.unwrap().starts_with("sha256:8920"));
        let old: GitHubAsset =
            serde_json::from_str(r#"{"name":"x","browser_download_url":"u","size":1}"#).unwrap();
        assert_eq!(old.digest, None);
    }

    #[test]
    fn a_newer_version_on_disk_means_a_restart_is_pending() {
        assert_eq!(
            newer_on_disk(Some("0.3.1\n"), "0.3.0"),
            Some("0.3.1".into())
        );
        assert_eq!(newer_on_disk(Some("0.3.0"), "0.3.0"), None);
        assert_eq!(newer_on_disk(Some("0.2.9"), "0.3.0"), None);
        assert_eq!(newer_on_disk(Some("garbage"), "0.3.0"), None);
        assert_eq!(newer_on_disk(None, "0.3.0"), None);
    }

    #[test]
    fn previous_image_is_the_parked_copy() {
        #[cfg(target_os = "macos")]
        assert_eq!(
            previous_image(Path::new(
                "/Applications/ImmichSync.app/Contents/MacOS/immichsync"
            )),
            Some(PathBuf::from(
                "/Applications/.ImmichSync.app.old/Contents/MacOS/immichsync"
            ))
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            previous_image(Path::new("/tmp/target/debug/immichsync")),
            None
        );
        #[cfg(windows)]
        assert_eq!(
            previous_image(Path::new(r"C:\Apps\immichsync.exe")),
            Some(PathBuf::from(r"C:\Apps\immichsync.exe.old"))
        );
    }

    /// One test owns the process-wide flag, so the sequence is deterministic.
    #[test]
    fn window_exe_switches_to_the_old_image_only_after_an_update() {
        let tmp = tempfile::tempdir().unwrap();
        #[cfg(target_os = "macos")]
        let (exe, old) = {
            let app = tmp.path().join("ImmichSync.app/Contents/MacOS");
            let old = tmp.path().join(".ImmichSync.app.old/Contents/MacOS");
            std::fs::create_dir_all(&app).unwrap();
            std::fs::create_dir_all(&old).unwrap();
            (app.join("immichsync"), old.join("immichsync"))
        };
        #[cfg(not(target_os = "macos"))]
        let (exe, old) = (
            tmp.path().join("immichsync.exe"),
            tmp.path().join("immichsync.exe.old"),
        );
        std::fs::write(&exe, b"new").unwrap();

        assert_eq!(window_exe(exe.clone()), exe, "no update: the running exe");
        note_update_applied();
        assert_eq!(window_exe(exe.clone()), exe, "old image missing: fall back");
        std::fs::write(&old, b"old").unwrap();
        assert_eq!(
            window_exe(exe.clone()),
            old,
            "update pending: the old image"
        );
        UPDATE_APPLIED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn check_interval_zero_is_disabled() {
        assert_eq!(check_interval_from_hours(0), Duration::from_secs(u64::MAX));
        assert_eq!(check_interval_from_hours(1), Duration::from_secs(3600));
        assert_eq!(check_interval_from_hours(24), Duration::from_secs(86400));
    }
}

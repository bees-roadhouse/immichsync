//! Configuration management for ImmichSync.
//!
//! Path layout follows the Windows-standard per-user split (matches VSCode,
//! Slack, Discord, GitHub Desktop):
//!
//! - **Binary install**: `%LOCALAPPDATA%\Programs\immichsync\immichsync.exe`
//!   (resolved via [`Config::install_dir`])
//! - **Roaming config**: `%APPDATA%\bees-roadhouse\immichsync\config.toml`
//!   (resolved via [`Config::config_dir`]). Small, portable across machines.
//! - **Local data**: `%LOCALAPPDATA%\bees-roadhouse\immichsync\` containing
//!   `state.db` + `logs\` (resolved via [`Config::local_data_dir`]). Machine-
//!   local state, never roams.
//!
//! `config.toml` is loaded from the roaming dir; writes are atomic
//! (write-temp-then-rename).

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info};

// ─── Errors ──────────────────────────────────────────────────────────────────

// `toml::de::Error` and `toml::ser::Error` are large structs (>100 bytes
// each) which would push every `Result<_, ConfigError>` over clippy's
// `result_large_err` threshold and force every transitive caller to also
// suppress or box. Boxing the toml errors here keeps the enum compact and
// the lint happy for the whole call chain.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Could not determine application data directory")]
    NoDataDir,

    #[error("Failed to create data directory `{path}`: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Failed to read config file `{path}`: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Failed to parse config file `{path}`: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },

    #[error("Failed to serialize config: {0}")]
    Serialize(Box<toml::ser::Error>),

    /// macOS: the key could not be put in the Keychain (e.g. the user clicked
    /// Deny). The save is refused rather than writing the key in plaintext.
    #[cfg(target_os = "macos")]
    #[error("Could not store the API key in the Keychain: {0}")]
    SecretStore(String),

    #[error("Failed to write config file `{path}`: {source}")]
    WriteFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Failed to rename temp config to final path `{path}`: {source}")]
    Rename {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl From<toml::ser::Error> for ConfigError {
    fn from(e: toml::ser::Error) -> Self {
        ConfigError::Serialize(Box::new(e))
    }
}

// ─── Sub-structs ─────────────────────────────────────────────────────────────

/// Immich server connection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Base URL of the Immich server, e.g. `https://photos.example.com`.
    /// Empty until the user configures it.
    pub url: String,

    /// API key for authentication.
    /// Stored as a plain string here; the encryption layer handles
    /// wrapping/unwrapping before this value is populated.
    pub api_key: String,

    /// Device identifier sent with every upload request.
    /// Defaults to `immichsync-{hostname}`.
    pub device_id: String,
}

/// Environment variable that supplies the API key for this process only.
///
/// For dev and test runs (`IMMICHSYNC_API_KEY="$(op read …)" immichsync`):
/// the key is used in memory and never written to `config.toml`, so a secret
/// read at run time stays off disk. Window subprocesses inherit it.
pub const API_KEY_ENV: &str = "IMMICHSYNC_API_KEY";

fn env_api_key() -> Option<String> {
    std::env::var(API_KEY_ENV).ok().filter(|k| !k.is_empty())
}

/// Turn a plaintext API key into its stored form.
///
/// If sealing fails, Windows keeps its long-standing fallback (plaintext,
/// with a warning). macOS refuses the save instead: there the failure is
/// almost always the user denying Keychain access, and writing the key into
/// `config.toml` behind their back is the opposite of what they chose.
fn seal_api_key<E: std::fmt::Display>(
    plaintext: &str,
    seal: impl Fn(&str) -> Result<String, E>,
) -> Result<String, ConfigError> {
    match seal(plaintext) {
        Ok(sealed) => {
            debug!("API key encrypted for storage");
            Ok(sealed)
        }
        #[cfg(target_os = "macos")]
        Err(e) => Err(ConfigError::SecretStore(e.to_string())),
        #[cfg(not(target_os = "macos"))]
        Err(e) => {
            tracing::warn!(error = %e, "Failed to encrypt API key; saving in plain text");
            Ok(plaintext.to_string())
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        let hostname = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".to_string());

        Self {
            url: String::new(),
            api_key: String::new(),
            device_id: format!("immichsync-{hostname}"),
        }
    }
}

/// Upload pipeline settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UploadConfig {
    /// Number of simultaneous upload workers.
    pub concurrency: u32,

    /// Maximum upload bandwidth in KB/s. `0` means unlimited.
    pub bandwidth_limit_kbps: u64,

    /// Upload ordering strategy: `"newest_first"` or `"oldest_first"`.
    pub order: String,

    /// Maximum number of retry attempts before a queue entry is marked failed.
    pub max_retries: u32,

    /// Per-request inactivity (HTTP read) timeout in seconds.
    ///
    /// This is the maximum time to wait with no response bytes before aborting
    /// a request. It is NOT an overall transfer-duration limit — a 50GB upload
    /// at 10MB/s legitimately streams for ~83 minutes without triggering it.
    /// The timeout fires only when the server stops sending bytes entirely,
    /// e.g. during post-upload checksumming on a slow backend (Orange Pi /
    /// Rockchip SBC). Default 3600s (1 hour) accommodates those cases.
    pub timeout_secs: u64,

    /// Number of days to retain files in `.immichsync-trash/` before
    /// permanent deletion.  Only applies when a folder's post-upload
    /// action is set to `Trash`.
    pub trash_retention_days: u32,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            concurrency: 2,
            bandwidth_limit_kbps: 0,
            order: "newest_first".to_string(),
            max_retries: 5,
            timeout_secs: 3600,
            trash_retention_days: 14,
        }
    }
}

/// Removable-device auto-watch settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DevicesConfig {
    /// Automatically start watching newly inserted removable devices.
    pub auto_watch: bool,

    /// When scanning a removable device, only watch if a DCIM folder is found.
    pub look_for_dcim: bool,

    /// Action taken when a device is inserted: `"auto"`, `"ask"`, or `"ignore"`.
    pub on_insert: String,
}

impl Default for DevicesConfig {
    fn default() -> Self {
        Self {
            auto_watch: true,
            look_for_dcim: true,
            on_insert: "auto".to_string(),
        }
    }
}

/// Apple Photos library source (macOS; docs/adr/0002-photokit-library-source.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, from = "PhotosConfigFile")]
pub struct PhotosConfig {
    /// Sync the Photos library. Off by default; turning it on in Settings is
    /// what raises the system permission prompt.
    pub enabled: bool,

    /// Mirror each Photos user album as an Immich album (many-to-many,
    /// #115). On by default. Smart and shared albums are never mirrored.
    pub sync_albums: bool,

    /// Also put every library upload into this one Immich album, an existing
    /// one or a new name. Empty = off.
    pub all_in_album: String,
}

impl Default for PhotosConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sync_albums: true,
            all_in_album: String::new(),
        }
    }
}

/// `[photos]` as it may appear on disk, including the pre-#115 key.
#[derive(Deserialize)]
#[serde(default)]
struct PhotosConfigFile {
    enabled: bool,
    sync_albums: bool,
    all_in_album: String,
    /// The tracer's single fixed album (#111). Read to migrate, never written.
    album_name: Option<String>,
}

impl Default for PhotosConfigFile {
    fn default() -> Self {
        let d = PhotosConfig::default();
        Self {
            enabled: d.enabled,
            sync_albums: d.sync_albums,
            all_in_album: d.all_in_album,
            album_name: None,
        }
    }
}

impl From<PhotosConfigFile> for PhotosConfig {
    /// Migration (#115): a non-empty tracer `album_name` becomes the
    /// catch-all album, unless `all_in_album` is already set; album sync
    /// stays at its default (on) unless the file says otherwise.
    fn from(f: PhotosConfigFile) -> Self {
        let mut all_in_album = f.all_in_album;
        if all_in_album.trim().is_empty() {
            if let Some(old) = f.album_name.filter(|n| !n.trim().is_empty()) {
                all_in_album = old.trim().to_string();
            }
        }
        Self {
            enabled: f.enabled,
            sync_albums: f.sync_albums,
            all_in_album,
        }
    }
}

/// System tray and notification UI settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Add ImmichSync to the Windows startup registry key.
    pub start_with_windows: bool,

    /// Minimize to the system tray when the settings window is closed.
    pub minimize_to_tray: bool,

    /// Show Windows toast notifications.
    pub show_notifications: bool,

    /// Show a toast notification when a batch upload completes.
    pub notification_on_complete: bool,

    /// Run without installing to AppData. When true, the install dialog
    /// is suppressed and the app runs from its current location.
    pub portable_mode: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            start_with_windows: true,
            minimize_to_tray: true,
            show_notifications: true,
            notification_on_complete: true,
            portable_mode: false,
        }
    }
}

/// Advanced / expert settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdvancedConfig {
    /// Minimum `tracing` log level: `"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"`.
    pub log_level: String,

    /// Polling interval in seconds used for network shares that do not
    /// support `ReadDirectoryChangesW`.
    pub poll_interval_secs: u64,

    /// Debounce window in milliseconds before a file is considered stable
    /// enough to queue for upload.
    pub write_settle_ms: u64,

    /// Automatically check GitHub for new releases.
    pub check_for_updates: bool,

    /// Hours between automatic update checks. `0` disables periodic checks.
    pub update_check_interval_hours: u32,

    /// GitHub repository to check for updates (format: `owner/repo`).
    /// Public repos only. Anything else (URL, SSH ref, hostname) is rejected
    /// at load time and the update check is disabled for the session.
    pub update_repo: String,

    /// Update channel. One of:
    ///   - `"latest"`     ... track the latest published release (default)
    ///   - `"prerelease"` ... track latest including prereleases
    ///   - `"none"`       ... disable update checks entirely
    ///   - `"vX.Y.Z"`     ... pin to a specific tag, never offer newer
    pub update_channel: String,
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            poll_interval_secs: 30,
            write_settle_ms: 2000,
            check_for_updates: true,
            update_check_interval_hours: 24,
            update_repo: "bees-roadhouse/immichsync".to_string(),
            update_channel: "latest".to_string(),
        }
    }
}

// ─── Root config ─────────────────────────────────────────────────────────────

/// Top-level application configuration.
///
/// Stored at `%APPDATA%\bees-roadhouse\immichsync\config.toml` (roaming).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub upload: UploadConfig,
    pub devices: DevicesConfig,
    pub photos: PhotosConfig,
    pub ui: UiConfig,
    pub advanced: AdvancedConfig,
}

impl Config {
    /// Returns the roaming config directory:
    /// `%APPDATA%\bees-roadhouse\immichsync\`. Created on first use.
    ///
    /// Contains `config.toml` only. Small, portable across machines, fine to
    /// participate in roaming profile sync.
    pub fn config_dir() -> Result<PathBuf, ConfigError> {
        let base = dirs::config_dir().ok_or(ConfigError::NoDataDir)?;
        let dir = base.join("bees-roadhouse").join("immichsync");

        if !dir.exists() {
            fs::create_dir_all(&dir).map_err(|source| ConfigError::CreateDir {
                path: dir.clone(),
                source,
            })?;
            debug!(path = %dir.display(), "Created ImmichSync config directory");
        }

        Ok(dir)
    }

    /// Returns the local (non-roaming) data directory:
    /// `%LOCALAPPDATA%\bees-roadhouse\immichsync\`. Created on first use.
    ///
    /// Contains the SQLite state DB and the `logs\` subdirectory. Machine-
    /// local state ... watched folder set, upload progress, log files all
    /// belong here, not in the roaming profile.
    pub fn local_data_dir() -> Result<PathBuf, ConfigError> {
        let base = dirs::data_local_dir().ok_or(ConfigError::NoDataDir)?;
        let dir = base.join("bees-roadhouse").join("immichsync");

        if !dir.exists() {
            fs::create_dir_all(&dir).map_err(|source| ConfigError::CreateDir {
                path: dir.clone(),
                source,
            })?;
            debug!(path = %dir.display(), "Created ImmichSync local data directory");
        }

        Ok(dir)
    }

    /// Returns the binary install directory:
    /// `%LOCALAPPDATA%\Programs\immichsync\`. Created on first use.
    ///
    /// This is where the installed `immichsync.exe` (and `version.txt`)
    /// lives. Matches the per-user pattern used by VSCode, Slack, Discord,
    /// and other major Windows apps that ship without admin elevation.
    #[cfg(windows)]
    pub fn install_dir() -> Result<PathBuf, ConfigError> {
        let base = dirs::data_local_dir().ok_or(ConfigError::NoDataDir)?;
        let dir = base.join("Programs").join("immichsync");

        if !dir.exists() {
            fs::create_dir_all(&dir).map_err(|source| ConfigError::CreateDir {
                path: dir.clone(),
                source,
            })?;
            debug!(path = %dir.display(), "Created ImmichSync install directory");
        }

        Ok(dir)
    }

    /// Returns the path to `config.toml` in the roaming config dir.
    pub fn config_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    /// Advanced → Log Level straight from `config.toml`, without the rest of
    /// [`Config::load`] (no key decryption, so no Keychain access before
    /// logging exists). `None` if there is no file or no setting.
    pub fn configured_log_level() -> Option<String> {
        let text = fs::read_to_string(Self::config_path().ok()?).ok()?;
        let value: toml::Value = toml::from_str(&text).ok()?;
        value
            .get("advanced")?
            .get("log_level")?
            .as_str()
            .map(str::to_string)
    }

    /// Load configuration from disk.
    ///
    /// Returns `Config::default()` if the file does not yet exist (first run).
    /// Returns an error if the file exists but cannot be read or parsed.
    /// Encrypted API keys (`enc:...`) are automatically decrypted.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path()?;

        let mut config = Self::load_from_disk(&path)?;
        if let Some(key) = env_api_key() {
            debug!("Using API key from {API_KEY_ENV} (not persisted)");
            config.server.api_key = key;
        }
        Ok(config)
    }

    fn load_from_disk(path: &std::path::Path) -> Result<Self, ConfigError> {
        let path = path.to_path_buf();
        if !path.exists() {
            info!("Config file not found, using defaults");
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(&path).map_err(|source| ConfigError::ReadFile {
            path: path.clone(),
            source,
        })?;

        let mut config: Self = toml::from_str(&contents).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source: Box::new(source),
        })?;

        // Decrypt API key if it was stored encrypted.
        if crate::platform::encryption::is_encrypted(&config.server.api_key) {
            match crate::platform::encryption::decrypt_api_key(&config.server.api_key) {
                Ok(plaintext) => {
                    config.server.api_key = plaintext;
                    debug!("API key decrypted");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to decrypt API key; clearing it");
                    config.server.api_key.clear();
                }
            }
        }

        debug!(path = %path.display(), "Loaded config from disk");
        Ok(config)
    }

    /// Save the configuration to disk using an atomic rename.
    ///
    /// Writes to `config.toml.tmp` first, then renames to `config.toml`.
    /// This ensures a partial write never corrupts the live file.
    /// The API key is encrypted before writing if it's not empty.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::config_path()?;
        let tmp_path = path.with_extension("toml.tmp");

        // Clone and encrypt the API key before serializing.
        let mut config_to_save = self.clone();

        // A key supplied through API_KEY_ENV is never persisted: keep
        // whatever the file already held instead.
        if env_api_key().is_some_and(|k| k == config_to_save.server.api_key) {
            config_to_save.server.api_key = Self::stored_api_key(&path);
        }
        if !config_to_save.server.api_key.is_empty()
            && !crate::platform::encryption::is_encrypted(&config_to_save.server.api_key)
        {
            config_to_save.server.api_key = seal_api_key(
                &config_to_save.server.api_key,
                crate::platform::encryption::encrypt_api_key,
            )?;
        }

        let contents = toml::to_string_pretty(&config_to_save)?;

        fs::write(&tmp_path, &contents).map_err(|source| ConfigError::WriteFile {
            path: tmp_path.clone(),
            source,
        })?;

        // Owner-only on Unix. The macOS key lives in the Keychain (save() is
        // refused if it can't be stored there), so this is hygiene only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600));
        }

        fs::rename(&tmp_path, &path).map_err(|source| ConfigError::Rename {
            path: path.clone(),
            source,
        })?;

        debug!(path = %path.display(), "Saved config to disk");
        Ok(())
    }

    /// The `api_key` value exactly as stored in the file (possibly `enc:`),
    /// or empty when there is no file or no key.
    fn stored_api_key(path: &std::path::Path) -> String {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str::<Self>(&s).ok())
            .map(|c| c.server.api_key)
            .unwrap_or_default()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_values() {
        let cfg = Config::default();
        assert_eq!(cfg.server.url, "");
        assert_eq!(cfg.server.api_key, "");
        assert!(cfg.server.device_id.starts_with("immichsync-"));
        assert_eq!(cfg.upload.concurrency, 2);
        assert_eq!(cfg.upload.bandwidth_limit_kbps, 0);
        assert_eq!(cfg.upload.order, "newest_first");
        assert_eq!(cfg.upload.max_retries, 5);
        assert_eq!(cfg.upload.timeout_secs, 3600);
        assert!(!cfg.photos.enabled, "the Photos library is opt-in");
        assert!(cfg.devices.auto_watch);
        assert!(cfg.devices.look_for_dcim);
        assert_eq!(cfg.devices.on_insert, "auto");
        assert!(cfg.ui.start_with_windows);
        assert!(cfg.ui.minimize_to_tray);
        assert!(cfg.ui.show_notifications);
        assert!(cfg.ui.notification_on_complete);
        assert_eq!(cfg.advanced.log_level, "info");
        assert_eq!(cfg.advanced.poll_interval_secs, 30);
        assert_eq!(cfg.advanced.write_settle_ms, 2000);
        assert_eq!(cfg.advanced.update_repo, "bees-roadhouse/immichsync");
        assert_eq!(cfg.advanced.update_channel, "latest");
    }

    #[test]
    fn a_tracer_album_name_becomes_the_catch_all_with_album_sync_on() {
        let cfg: Config =
            toml::from_str("[photos]\nenabled = true\nalbum_name = \"Photos library\"\n").unwrap();
        assert!(cfg.photos.enabled);
        assert!(cfg.photos.sync_albums, "album sync is on by default");
        assert_eq!(cfg.photos.all_in_album, "Photos library");

        let written = toml::to_string_pretty(&cfg).unwrap();
        assert!(
            !written.contains("album_name"),
            "the old key is never written back"
        );
        let again: Config = toml::from_str(&written).unwrap();
        assert_eq!(again.photos, cfg.photos);
    }

    #[test]
    fn photos_album_settings_defaults_and_precedence() {
        let fresh = Config::default().photos;
        assert!(fresh.sync_albums);
        assert!(fresh.all_in_album.is_empty());

        // An empty legacy name means no catch-all.
        let cfg: Config = toml::from_str("[photos]\nalbum_name = \"  \"\n").unwrap();
        assert!(cfg.photos.all_in_album.is_empty());

        // The new key wins over a leftover old one.
        let cfg: Config = toml::from_str(
            "[photos]\nall_in_album = \"Mine\"\nalbum_name = \"Old\"\nsync_albums = false\n",
        )
        .unwrap();
        assert_eq!(cfg.photos.all_in_album, "Mine");
        assert!(!cfg.photos.sync_albums);
    }

    #[test]
    fn round_trip_serialization() {
        let original = Config::default();
        let serialized = toml::to_string_pretty(&original).expect("serialize");
        let deserialized: Config = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(original.upload.concurrency, deserialized.upload.concurrency);
        assert_eq!(original.server.device_id, deserialized.server.device_id);
        assert_eq!(
            original.advanced.poll_interval_secs,
            deserialized.advanced.poll_interval_secs
        );
    }

    #[test]
    fn partial_toml_uses_defaults_for_missing_fields() {
        let partial = r#"
[server]
url = "https://photos.example.com"
"#;
        let cfg: Config = toml::from_str(partial).expect("deserialize partial");
        assert_eq!(cfg.server.url, "https://photos.example.com");
        // Fields not in the TOML should fall back to their Default impl.
        assert_eq!(cfg.upload.concurrency, 2);
        assert_eq!(cfg.advanced.log_level, "info");
    }

    #[test]
    fn config_dir_under_roaming_appdata() {
        // dirs::config_dir() on Windows resolves to %APPDATA% (roaming).
        let dir = Config::config_dir().expect("config_dir");
        let roaming = dirs::config_dir().expect("dirs config_dir");
        assert!(
            dir.starts_with(&roaming),
            "config_dir ({}) should be under roaming ({})",
            dir.display(),
            roaming.display()
        );
        assert!(
            dir.ends_with("bees-roadhouse\\immichsync")
                || dir.ends_with("bees-roadhouse/immichsync")
        );
    }

    #[test]
    fn local_data_dir_under_local_appdata() {
        // dirs::data_local_dir() on Windows resolves to %LOCALAPPDATA%.
        let dir = Config::local_data_dir().expect("local_data_dir");
        let local = dirs::data_local_dir().expect("dirs data_local_dir");
        assert!(
            dir.starts_with(&local),
            "local_data_dir ({}) should be under local ({})",
            dir.display(),
            local.display()
        );
        assert!(
            dir.ends_with("bees-roadhouse\\immichsync")
                || dir.ends_with("bees-roadhouse/immichsync")
        );
    }

    #[cfg(windows)]
    #[test]
    fn install_dir_under_local_appdata_programs() {
        let dir = Config::install_dir().expect("install_dir");
        let local = dirs::data_local_dir().expect("dirs data_local_dir");
        assert!(
            dir.starts_with(&local),
            "install_dir ({}) should be under local ({})",
            dir.display(),
            local.display()
        );
        assert!(dir.ends_with("Programs\\immichsync") || dir.ends_with("Programs/immichsync"));
    }

    #[cfg(windows)]
    #[test]
    fn paths_are_distinct() {
        // The whole point of the split: these three dirs are not the same.
        // (macOS has one Application Support dir for both; see below.)
        let cfg = Config::config_dir().expect("config_dir");
        let local = Config::local_data_dir().expect("local_data_dir");
        let install = Config::install_dir().expect("install_dir");
        assert_ne!(cfg, local, "config_dir must differ from local_data_dir");
        assert_ne!(cfg, install, "config_dir must differ from install_dir");
        assert_ne!(
            local, install,
            "local_data_dir must differ from install_dir"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_config_and_state_share_application_support() {
        let cfg = Config::config_dir().expect("config_dir");
        let local = Config::local_data_dir().expect("local_data_dir");
        assert_eq!(cfg, local);
        assert!(
            cfg.ends_with("Library/Application Support/bees-roadhouse/immichsync"),
            "unexpected config_dir {}",
            cfg.display()
        );
    }

    #[test]
    fn config_path_lives_in_config_dir() {
        let path = Config::config_path().expect("config_path");
        let dir = Config::config_dir().expect("config_dir");
        assert_eq!(path.parent(), Some(dir.as_path()));
        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some("config.toml")
        );
    }

    #[test]
    fn sealed_key_is_stored() {
        let r = seal_api_key("k", |_| Ok::<_, String>("keychain:api-key".into()));
        assert_eq!(r.unwrap(), "keychain:api-key");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_refuses_to_save_a_key_it_could_not_seal() {
        let r = seal_api_key("secret", |_| Err::<String, _>("user denied"));
        assert!(matches!(r, Err(ConfigError::SecretStore(ref m)) if m == "user denied"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_keeps_its_plaintext_fallback() {
        let r = seal_api_key("secret", |_| Err::<String, _>("dpapi failed"));
        assert_eq!(r.unwrap(), "secret");
    }
}

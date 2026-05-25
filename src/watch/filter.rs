// File filtering (extensions, globs, size, cloud-placeholder attributes)

use glob::Pattern;
use std::collections::HashSet;
use std::path::Path;
use tracing::debug;

/// Default minimum file size in bytes (1 KB)
const DEFAULT_MIN_SIZE: u64 = 1024;

/// Windows file attribute bits that mark a file as a cloud-storage placeholder.
///
/// Any file with one or more of these bits set is "online" — its content lives
/// in the cloud and any read triggers a download. OneDrive Files On-Demand,
/// iCloud Drive for Windows, Google Drive for Desktop, Dropbox Smart Sync,
/// Box Drive, SeaDrive, etc. all set one or more of these.
///
/// See: <https://learn.microsoft.com/en-us/windows/win32/fileio/file-attribute-constants>
#[cfg(windows)]
pub(crate) const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
#[cfg(windows)]
pub(crate) const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
#[cfg(windows)]
pub(crate) const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// Bitmask of all "cloud-placeholder" attributes. A non-zero AND with a file's
/// raw attribute DWORD means the file content is not materialized locally.
#[cfg(windows)]
pub(crate) const CLOUD_PLACEHOLDER_MASK: u32 =
    FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;

/// File filter that determines whether a given path should be watched and queued.
///
/// Filtering is applied in this order:
/// 1. Exclusion name list (exact filename matches like Thumbs.db)
/// 2. Extension allowlist (image + video extensions by default)
/// 3. Minimum file size
/// 4. Cloud-placeholder check (Windows recall attributes) when enabled
/// 5. Custom exclude glob patterns (per-folder overrides)
/// 6. Custom include glob patterns (if any are set, path must match at least one)
#[derive(Debug, Clone)]
pub struct FileFilter {
    /// Allowed file extensions (lowercase, without leading dot)
    allowed_extensions: HashSet<String>,
    /// Exact filenames to always exclude
    excluded_names: HashSet<String>,
    /// Minimum file size in bytes — files smaller than this are ignored
    min_size: u64,
    /// Custom glob patterns for inclusion (if non-empty, file must match at least one)
    include_patterns: Vec<Pattern>,
    /// Custom glob patterns for exclusion (file is dropped if it matches any)
    exclude_patterns: Vec<Pattern>,
    /// When `true`, files marked with Windows cloud-placeholder attributes
    /// (OneDrive Files On-Demand, iCloud, Google Drive, SeaDrive, etc.) are
    /// dropped before any read. Default `true` — opening a placeholder
    /// triggers a cloud download as a side effect.
    ignore_online_files: bool,
}

impl FileFilter {
    /// Create a new `FileFilter` with default settings.
    ///
    /// Allows all standard image and video extensions, excludes known
    /// Windows/macOS metadata files, and requires files to be at least 1 KB.
    pub fn new() -> Self {
        let allowed_extensions: HashSet<String> = [
            // Images
            "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "tiff", "bmp", "raw",
            "cr2", "cr3", "nef", "arw", "dng", "orf", "rw2", "pef", "srw", "raf",
            // Videos
            "mp4", "mov", "avi", "mkv", "webm", "m4v", "3gp", "mts", "m2ts",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        // Exact filenames that should always be excluded.
        // "Zone.Identifier" typically appears as an ADS (e.g. "photo.jpg:Zone.Identifier"),
        // but the component after the colon will show up as a separate path on some systems,
        // so we also filter the bare name.
        let excluded_names: HashSet<String> = [
            "Thumbs.db",
            "thumbs.db",
            "desktop.ini",
            ".DS_Store",
            "Zone.Identifier",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        Self {
            allowed_extensions,
            excluded_names,
            min_size: DEFAULT_MIN_SIZE,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            ignore_online_files: true,
        }
    }

    /// Override whether files with Windows cloud-placeholder attributes are
    /// skipped. Default is `true`. Set to `false` to upload placeholders too
    /// (the file read will trigger a cloud download for each file).
    pub fn with_ignore_online_files(mut self, ignore: bool) -> Self {
        self.ignore_online_files = ignore;
        self
    }

    /// Add custom glob include patterns.
    ///
    /// When any include patterns are set, a file must match **at least one**
    /// of them to be included (in addition to passing the extension/exclusion
    /// checks). Patterns that fail to compile are logged and skipped.
    pub fn with_include_patterns(mut self, patterns: Vec<String>) -> Self {
        self.include_patterns = patterns
            .into_iter()
            .filter_map(|p| match Pattern::new(&p) {
                Ok(pat) => Some(pat),
                Err(e) => {
                    tracing::warn!("Invalid include pattern '{}': {}", p, e);
                    None
                }
            })
            .collect();
        self
    }

    /// Add custom glob exclude patterns.
    ///
    /// If a file matches **any** of these patterns it is excluded, even if it
    /// would otherwise pass all other checks.
    pub fn with_exclude_patterns(mut self, patterns: Vec<String>) -> Self {
        self.exclude_patterns = patterns
            .into_iter()
            .filter_map(|p| match Pattern::new(&p) {
                Ok(pat) => Some(pat),
                Err(e) => {
                    tracing::warn!("Invalid exclude pattern '{}': {}", p, e);
                    None
                }
            })
            .collect();
        self
    }

    /// Override the minimum file size threshold (bytes).
    ///
    /// Used by the filter test suite to exercise the size-threshold branch
    /// without rewriting the whole filter; production builds construct the
    /// filter with the config-derived defaults and never tune it after.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_min_size(mut self, bytes: u64) -> Self {
        self.min_size = bytes;
        self
    }

    /// Decide whether `path` should be included for upload consideration.
    ///
    /// Returns `false` for any of the following reasons:
    /// - Path is inside a `.immichsync-trash` directory
    /// - Path has no file name component
    /// - File name is in the exclusion list
    /// - Extension is not in the allowed set
    /// - File is smaller than the minimum size (or size cannot be determined)
    /// - A custom exclude glob matches the path
    /// - Custom include globs are configured and none of them match the path
    pub fn should_include(&self, path: &Path) -> bool {
        // Skip anything inside the trash directory.
        if path
            .components()
            .any(|c| c.as_os_str() == crate::upload::worker::TRASH_DIR_NAME)
        {
            debug!("Excluding (inside trash directory): {:?}", path);
            return false;
        }

        // Must have a file name
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => {
                debug!("Skipping path with no file name: {:?}", path);
                return false;
            }
        };

        // Exact-name exclusion list
        if self.excluded_names.contains(file_name) {
            debug!("Excluding by name: {:?}", path);
            return false;
        }

        // Extension check
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_lowercase(),
            None => {
                debug!("Excluding (no extension): {:?}", path);
                return false;
            }
        };
        if !self.allowed_extensions.contains(&ext) {
            debug!("Excluding by extension '{}': {:?}", ext, path);
            return false;
        }

        // Metadata: enforces the minimum-size threshold and (on Windows) the
        // cloud-placeholder check. We do both off the same stat call so we
        // pay one syscall, not two.
        match std::fs::metadata(path) {
            Ok(meta) => {
                if meta.len() < self.min_size {
                    debug!(
                        "Excluding (too small: {} < {} bytes): {:?}",
                        meta.len(),
                        self.min_size,
                        path
                    );
                    return false;
                }

                #[cfg(windows)]
                if self.ignore_online_files && is_online_file(&meta) {
                    debug!(
                        "Excluding (cloud-placeholder / online file, attrs=0x{:x}): {:?}",
                        std::os::windows::fs::MetadataExt::file_attributes(&meta),
                        path
                    );
                    return false;
                }
            }
            Err(e) => {
                debug!("Excluding (cannot stat: {}): {:?}", e, path);
                return false;
            }
        }

        // The glob patterns are matched against the full path string.
        let path_str = path.to_string_lossy();

        // Custom exclude patterns
        for pat in &self.exclude_patterns {
            if pat.matches(&path_str) {
                debug!("Excluding by pattern '{}': {:?}", pat.as_str(), path);
                return false;
            }
        }

        // Custom include patterns (must match at least one if any are configured)
        if !self.include_patterns.is_empty() {
            let matched = self.include_patterns.iter().any(|p| p.matches(&path_str));
            if !matched {
                debug!("Excluding (no include pattern matched): {:?}", path);
                return false;
            }
        }

        true
    }
}

impl Default for FileFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a JSON-encoded array of glob patterns into a `Vec<String>`.
///
/// Accepts `None`, empty strings, malformed JSON, and non-array JSON
/// gracefully by returning an empty `Vec` and logging a warning. Each entry
/// in the array is trimmed; empty entries are dropped.
pub fn parse_patterns_json(raw: Option<&str>) -> Vec<String> {
    let Some(s) = raw else {
        return Vec::new();
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Vec<String>>(trimmed) {
        Ok(v) => v
            .into_iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        Err(e) => {
            tracing::warn!("Malformed patterns JSON '{}': {}", trimmed, e);
            Vec::new()
        }
    }
}

/// Serialize a slice of glob patterns to a JSON array string suitable for
/// storage in the `include_patterns` / `exclude_patterns` column.
///
/// Returns `None` if the input is empty (so the DB column stores NULL).
/// Entries are trimmed and empty entries are dropped before serialization.
pub fn patterns_to_json(patterns: &[String]) -> Option<String> {
    let cleaned: Vec<&str> = patterns
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    serde_json::to_string(&cleaned).ok()
}

/// Return `true` when `meta` indicates a Windows cloud-storage placeholder
/// (OneDrive Files On-Demand, iCloud Drive, Google Drive, Dropbox, SeaDrive,
/// Box Drive, etc.). Reads only the cached attribute DWORD — no extra
/// syscalls and no contents read.
///
/// This is the worker's last-line check before SHA-1 hashing: even with the
/// filter layer in place, a file's attributes can change between filter time
/// and hash time (offload happens in the background).
#[cfg(windows)]
pub fn is_online_file(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    (meta.file_attributes() & CLOUD_PLACEHOLDER_MASK) != 0
}

/// Stub for non-Windows targets: nothing is ever an online file.
#[cfg(not(windows))]
pub fn is_online_file(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_temp_file_with_size(ext: &str, size: usize) -> NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(&format!(".{}", ext))
            .tempfile()
            .unwrap();
        f.write_all(&vec![0u8; size]).unwrap();
        f
    }

    #[test]
    fn test_image_extensions_included() {
        let filter = FileFilter::new();
        for ext in &["jpg", "jpeg", "png", "heic", "cr3", "arw", "dng"] {
            let f = make_temp_file_with_size(ext, 2048);
            assert!(
                filter.should_include(f.path()),
                "Expected '{}' to be included",
                ext
            );
        }
    }

    #[test]
    fn test_video_extensions_included() {
        let filter = FileFilter::new();
        for ext in &["mp4", "mov", "mkv", "m4v", "mts"] {
            let f = make_temp_file_with_size(ext, 2048);
            assert!(
                filter.should_include(f.path()),
                "Expected '{}' to be included",
                ext
            );
        }
    }

    #[test]
    fn test_excluded_names() {
        let filter = FileFilter::new();
        // We can't easily create a path named "Thumbs.db" that passes stat,
        // but we can verify the extension check blocks most of them first.
        // For the name check we test the logic directly via a real temp file
        // with a dummy extension, which won't reach the name check because
        // the extension ".db" is not allowed. That's correct behaviour — the
        // name check fires before the extension check, but .db would be caught
        // by extension anyway. The excluded_names set is tested via should_include
        // returning false for any path where file_name is in the set.
        let dir = tempfile::tempdir().unwrap();
        // Write a real file that happens to be named Thumbs.db
        let p = dir.path().join("Thumbs.db");
        std::fs::write(&p, vec![0u8; 2048]).unwrap();
        assert!(!filter.should_include(&p), "Thumbs.db should be excluded");
    }

    #[test]
    fn test_min_size_exclusion() {
        let filter = FileFilter::new();
        // 512 bytes — below the 1 KB default
        let f = make_temp_file_with_size("jpg", 512);
        assert!(!filter.should_include(f.path()));
    }

    #[test]
    fn test_unknown_extension_excluded() {
        let filter = FileFilter::new();
        let f = make_temp_file_with_size("txt", 2048);
        assert!(!filter.should_include(f.path()));
    }

    #[test]
    fn test_custom_include_pattern() {
        let filter = FileFilter::new().with_include_patterns(vec!["**/vacation*".to_string()]);
        let dir = tempfile::tempdir().unwrap();
        let match_path = dir.path().join("vacation_photo.jpg");
        let no_match_path = dir.path().join("birthday_photo.jpg");
        std::fs::write(&match_path, vec![0u8; 2048]).unwrap();
        std::fs::write(&no_match_path, vec![0u8; 2048]).unwrap();
        assert!(filter.should_include(&match_path));
        assert!(!filter.should_include(&no_match_path));
    }

    #[test]
    fn test_custom_exclude_pattern() {
        let filter = FileFilter::new().with_exclude_patterns(vec!["**/thumbnails/**".to_string()]);
        let dir = tempfile::tempdir().unwrap();
        let thumb_dir = dir.path().join("thumbnails");
        std::fs::create_dir(&thumb_dir).unwrap();
        let p = thumb_dir.join("img.jpg");
        std::fs::write(&p, vec![0u8; 2048]).unwrap();
        assert!(!filter.should_include(&p));
    }

    #[test]
    fn test_custom_min_size() {
        let filter = FileFilter::new().with_min_size(5000);
        let f = make_temp_file_with_size("jpg", 2048);
        assert!(!filter.should_include(f.path()));

        let f2 = make_temp_file_with_size("jpg", 6000);
        assert!(filter.should_include(f2.path()));
    }

    // ── Pattern JSON helpers ────────────────────────────────────────────────

    #[test]
    fn parse_patterns_json_none() {
        assert!(parse_patterns_json(None).is_empty());
    }

    #[test]
    fn parse_patterns_json_empty_string() {
        assert!(parse_patterns_json(Some("")).is_empty());
        assert!(parse_patterns_json(Some("   ")).is_empty());
    }

    #[test]
    fn parse_patterns_json_valid_array() {
        let v = parse_patterns_json(Some(r#"["*.jpg","**/raw/**"]"#));
        assert_eq!(v, vec!["*.jpg".to_string(), "**/raw/**".to_string()]);
    }

    #[test]
    fn parse_patterns_json_trims_and_drops_empty() {
        let v = parse_patterns_json(Some(r#"["  *.jpg  ","",""]"#));
        assert_eq!(v, vec!["*.jpg".to_string()]);
    }

    #[test]
    fn parse_patterns_json_malformed_returns_empty() {
        // Malformed JSON should not panic, just yield an empty list.
        assert!(parse_patterns_json(Some("not json")).is_empty());
        assert!(parse_patterns_json(Some(r#"{"k":"v"}"#)).is_empty());
    }

    #[test]
    fn patterns_to_json_roundtrip() {
        let patterns = vec!["*.jpg".to_string(), "**/raw/**".to_string()];
        let json = patterns_to_json(&patterns).expect("should serialize");
        let parsed = parse_patterns_json(Some(&json));
        assert_eq!(parsed, patterns);
    }

    #[test]
    fn patterns_to_json_empty_yields_none() {
        assert!(patterns_to_json(&[]).is_none());
        assert!(patterns_to_json(&["".to_string(), "  ".to_string()]).is_none());
    }

    // ── Filter combinations ─────────────────────────────────────────────────

    #[test]
    fn test_default_empty_patterns_pass_through() {
        // Empty include + exclude → behaves identically to the default filter.
        let filter = FileFilter::new()
            .with_include_patterns(vec![])
            .with_exclude_patterns(vec![]);
        let f = make_temp_file_with_size("jpg", 2048);
        assert!(filter.should_include(f.path()));
    }

    #[test]
    fn test_include_and_exclude_combined() {
        // Include matches everything under photos/, exclude carves out thumbnails/.
        let filter = FileFilter::new()
            .with_include_patterns(vec!["**/photos/**".to_string()])
            .with_exclude_patterns(vec!["**/thumbnails/**".to_string()]);

        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        let thumbs = photos.join("thumbnails");
        std::fs::create_dir_all(&thumbs).unwrap();

        let kept = photos.join("vacation.jpg");
        let excluded = thumbs.join("vacation.jpg");
        let unrelated = dir.path().join("other.jpg");

        std::fs::write(&kept, vec![0u8; 2048]).unwrap();
        std::fs::write(&excluded, vec![0u8; 2048]).unwrap();
        std::fs::write(&unrelated, vec![0u8; 2048]).unwrap();

        assert!(filter.should_include(&kept));
        assert!(!filter.should_include(&excluded));
        assert!(!filter.should_include(&unrelated));
    }

    #[test]
    fn test_invalid_glob_pattern_does_not_panic() {
        // `[` is an unclosed character class — invalid glob syntax.
        // Should be skipped with a warning, leaving the filter usable.
        let filter = FileFilter::new()
            .with_include_patterns(vec!["[invalid".to_string()])
            .with_exclude_patterns(vec!["also[invalid".to_string()]);

        // With no valid include patterns retained, the include list is empty,
        // so the filter falls back to the "no include patterns configured" path.
        let f = make_temp_file_with_size("jpg", 2048);
        assert!(filter.should_include(f.path()));
    }

    // ── Cloud-placeholder / online-file handling ────────────────────────────

    /// Mark a file with FILE_ATTRIBUTE_OFFLINE so `should_include` treats it
    /// like a cloud placeholder. SeaDrive / OneDrive set this in production;
    /// for tests we set it manually via `SetFileAttributesW`.
    ///
    /// Returns true on success, false if the OS rejected the attribute (some
    /// filesystems clear it on the next sync — fine for unit testing where
    /// we never sync).
    #[cfg(windows)]
    fn mark_offline(path: &Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            // Preserve existing attributes (notably ARCHIVE) and add OFFLINE.
            let current = windows::Win32::Storage::FileSystem::GetFileAttributesW(
                windows::core::PCWSTR(wide.as_ptr()),
            );
            let combined = current | super::FILE_ATTRIBUTE_OFFLINE;
            windows::Win32::Storage::FileSystem::SetFileAttributesW(
                windows::core::PCWSTR(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(combined),
            )
            .is_ok()
        }
    }

    #[cfg(windows)]
    #[test]
    fn test_online_file_skipped_by_default() {
        let f = make_temp_file_with_size("jpg", 4096);
        assert!(mark_offline(f.path()), "Failed to mark file offline");

        let filter = FileFilter::new();
        assert!(
            !filter.should_include(f.path()),
            "Default filter should skip cloud-placeholder files"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_online_file_included_when_disabled() {
        let f = make_temp_file_with_size("jpg", 4096);
        assert!(mark_offline(f.path()), "Failed to mark file offline");

        let filter = FileFilter::new().with_ignore_online_files(false);
        assert!(
            filter.should_include(f.path()),
            "Filter with ignore_online_files=false should include placeholders"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_normal_file_passes_with_ignore_on() {
        let f = make_temp_file_with_size("jpg", 4096);
        // Don't mark it offline — it's a real local file.
        let filter = FileFilter::new();
        assert!(
            filter.should_include(f.path()),
            "Filter with default ignore_online_files=true should still include local files"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_is_online_file_helper() {
        let f = make_temp_file_with_size("jpg", 4096);
        let meta = std::fs::metadata(f.path()).unwrap();
        assert!(
            !super::is_online_file(&meta),
            "Plain temp file should not be marked online"
        );

        assert!(mark_offline(f.path()));
        let meta = std::fs::metadata(f.path()).unwrap();
        assert!(
            super::is_online_file(&meta),
            "File flagged FILE_ATTRIBUTE_OFFLINE should be detected as online"
        );
    }
}

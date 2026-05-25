//! EXIF extraction and filesystem timestamp utilities.
//!
//! For image files: attempts to read `DateTimeOriginal` then `DateTime`
//! from EXIF data via `kamadak-exif`.
//!
//! For non-image files (or when EXIF is absent/unreadable): falls back
//! to the filesystem `created` and `modified` times.
//!
//! All timestamps are returned as `DateTime<Utc>`.

use std::fs;
use std::io::{self, BufReader};
use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, NaiveDateTime, Utc};
use exif::{In, Reader as ExifReader, Tag};
use thiserror::Error;
use tracing::{debug, warn};

// ─── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("Failed to read filesystem metadata for `{path}`: {source}")]
    FsMetadata { path: String, source: io::Error },

    #[error("Filesystem metadata contains no creation or modification time for `{path}`")]
    NoTimestamp { path: String },
}

// ─── Types ───────────────────────────────────────────────────────────────────

/// Timestamps extracted from a file.
#[derive(Debug, Clone)]
pub struct FileMetadata {
    /// When the file content was created (from EXIF if available,
    /// otherwise filesystem created time).
    pub created_at: DateTime<Utc>,

    /// When the file was last modified (filesystem `mtime`).
    pub modified_at: DateTime<Utc>,
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Extract timestamps from a file.
///
/// Tries EXIF first for image files. If EXIF is absent, unreadable, or
/// the file is not an image, falls back to filesystem timestamps.
pub fn extract_metadata(path: &Path) -> Result<FileMetadata, MetadataError> {
    // Try EXIF for images. Any failure is non-fatal — we just fall through.
    if let Some(exif_dt) = try_exif_datetime(path) {
        debug!(
            path = %path.display(),
            datetime = %exif_dt,
            "Using EXIF DateTimeOriginal"
        );

        // Use the EXIF date for created_at; get modified_at from filesystem.
        let modified_at = fs_modified(path)?;
        return Ok(FileMetadata {
            created_at: exif_dt,
            modified_at,
        });
    }

    // Fall back to filesystem timestamps.
    let fs_meta = fs::metadata(path).map_err(|source| MetadataError::FsMetadata {
        path: path.display().to_string(),
        source,
    })?;

    let modified_at =
        system_time_to_utc(fs_meta.modified().ok()).ok_or_else(|| MetadataError::NoTimestamp {
            path: path.display().to_string(),
        })?;

    // `created()` is not available on all platforms (e.g. some Linux
    // filesystems), so fall back to `modified` when it's absent.
    let created_at = system_time_to_utc(fs_meta.created().ok()).unwrap_or(modified_at);

    debug!(
        path = %path.display(),
        created = %created_at,
        modified = %modified_at,
        "Using filesystem timestamps (no EXIF)"
    );

    Ok(FileMetadata {
        created_at,
        modified_at,
    })
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// Attempt to read `DateTimeOriginal` or `DateTime` from EXIF.
///
/// Returns `None` on any failure (not an image, no EXIF, parse error).
fn try_exif_datetime(path: &Path) -> Option<DateTime<Utc>> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(&file);

    let exif = ExifReader::new().read_from_container(&mut reader).ok()?;

    // Prefer DateTimeOriginal (when shutter was pressed) over DateTime
    // (which may reflect when the file was last edited).
    let field = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .or_else(|| exif.get_field(Tag::DateTime, In::PRIMARY))?;

    let raw = field.display_value().to_string();
    parse_exif_datetime(&raw)
}

/// Parse an EXIF datetime string (`"YYYY:MM:DD HH:MM:SS"`) into `DateTime<Utc>`.
///
/// EXIF dates use colons as date separators, which `chrono` does not parse
/// natively, so we reformat before parsing.
fn parse_exif_datetime(s: &str) -> Option<DateTime<Utc>> {
    // The kamadak-exif display_value for datetime fields looks like:
    // "2023:06:15 14:30:00"
    // Normalise to "2023-06-15 14:30:00" for chrono.
    //
    // Format: YYYY:MM:DD HH:MM:SS
    //         0123456789...
    // Positions 4 and 7 are the date-part colons that need to become dashes.
    let normalised: String = s
        .char_indices()
        .map(|(i, ch)| {
            if ch == ':' && (i == 4 || i == 7) {
                '-'
            } else {
                ch
            }
        })
        .collect();

    let naive = NaiveDateTime::parse_from_str(&normalised, "%Y-%m-%d %H:%M:%S")
        .map_err(|e| {
            warn!(
                raw = s,
                normalised = %normalised,
                error = %e,
                "Failed to parse EXIF datetime"
            );
        })
        .ok()?;

    Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

/// Read just the filesystem `mtime` for a path.
fn fs_modified(path: &Path) -> Result<DateTime<Utc>, MetadataError> {
    let meta = fs::metadata(path).map_err(|source| MetadataError::FsMetadata {
        path: path.display().to_string(),
        source,
    })?;

    system_time_to_utc(meta.modified().ok()).ok_or_else(|| MetadataError::NoTimestamp {
        path: path.display().to_string(),
    })
}

/// Convert an `Option<SystemTime>` to `Option<DateTime<Utc>>`.
fn system_time_to_utc(t: Option<SystemTime>) -> Option<DateTime<Utc>> {
    t.and_then(|st| {
        st.duration_since(SystemTime::UNIX_EPOCH).ok().map(|d| {
            // `chrono::DateTime::from_timestamp` is the non-deprecated form
            // for chrono >= 0.4.27 and returns Option<DateTime<Utc>>.
            chrono::DateTime::from_timestamp(d.as_secs() as i64, d.subsec_nanos())
                .unwrap_or_else(Utc::now)
        })
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn non_image_file_returns_fs_timestamps() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"not an image").unwrap();
        tmp.flush().unwrap();
        let meta = extract_metadata(tmp.path()).unwrap();
        // Both timestamps should be reasonable (after year 2000).
        let epoch_2000 = chrono::DateTime::from_timestamp(946_684_800, 0).unwrap();
        assert!(meta.created_at > epoch_2000);
        assert!(meta.modified_at > epoch_2000);
    }

    #[test]
    fn missing_file_returns_error() {
        let result = extract_metadata(Path::new("/nonexistent/path/file.jpg"));
        assert!(result.is_err());
    }

    #[test]
    fn parse_exif_datetime_valid() {
        let dt = parse_exif_datetime("2023:06:15 14:30:00").unwrap();
        assert_eq!(
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2023-06-15 14:30:00"
        );
    }

    #[test]
    fn parse_exif_datetime_invalid_returns_none() {
        assert!(parse_exif_datetime("not a date").is_none());
        assert!(parse_exif_datetime("").is_none());
    }
}

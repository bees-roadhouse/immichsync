//! SHA-1 file hashing for deduplication.
//!
//! Streams files in 8 KB chunks to avoid loading entire files into memory.
//! The resulting hex digest matches Immich's internal checksum format.

use std::io::{self, Read};
use std::path::Path;

use sha1::{Digest, Sha1};
use thiserror::Error;

// ─── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum HasherError {
    #[error("Failed to open file `{path}` for hashing: {source}")]
    Open { path: String, source: io::Error },

    #[error("Failed to read file `{path}` while hashing: {source}")]
    Read { path: String, source: io::Error },
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Compute the SHA-1 digest of a file, streaming it in 8 KB chunks.
///
/// Returns the digest as a lowercase hex string, e.g.
/// `"da39a3ee5e6b4b0d3255bfef95601890afd80709"`.
///
/// The file is never fully loaded into memory; the buffer is reused
/// across iterations.
pub fn hash_file(path: &Path) -> Result<String, HasherError> {
    let path_str = path.display().to_string();

    let mut file = std::fs::File::open(path).map_err(|source| HasherError::Open {
        path: path_str.clone(),
        source,
    })?;

    let mut hasher = Sha1::new();
    let mut buf = [0u8; 8192];

    loop {
        let n = file.read(&mut buf).map_err(|source| HasherError::Read {
            path: path_str.clone(),
            source,
        })?;

        if n == 0 {
            break;
        }

        hasher.update(&buf[..n]);
    }

    let digest = hasher.finalize();
    Ok(format!("{:x}", digest))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn empty_file_produces_known_sha1() {
        let tmp = NamedTempFile::new().unwrap();
        let hash = hash_file(tmp.path()).unwrap();
        // SHA-1 of empty content
        assert_eq!(hash, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn known_content_produces_correct_hash() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"hello world").unwrap();
        tmp.flush().unwrap();
        let hash = hash_file(tmp.path()).unwrap();
        // echo -n "hello world" | sha1sum
        assert_eq!(hash, "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed");
    }

    #[test]
    fn missing_file_returns_open_error() {
        let result = hash_file(Path::new("/nonexistent/path/file.jpg"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, HasherError::Open { .. }));
    }

    #[test]
    fn hash_is_lowercase_hex() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"ImmichSync test data").unwrap();
        tmp.flush().unwrap();
        let hash = hash_file(tmp.path()).unwrap();
        assert_eq!(hash, hash.to_lowercase());
        assert_eq!(hash.len(), 40);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

// Watched-folder path normalisation.
//
// Everything that reads files works on canonical paths (the watcher, the
// folder map, the initial scan, `uploaded_files`). Folder rows used to keep
// whatever was typed or picked (`~/Pictures`, `~/desktop`, a symlink), and
// comparisons between the two forms failed: a `~` folder never synced, and
// trash flattened subfolders (#99). Folders are therefore normalised once,
// when they are added, and existing rows are migrated at startup.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FolderPathError {
    #[error("no folder given")]
    Empty,
    #[error("“{0}” does not exist")]
    Missing(String),
    #[error("“{0}” is not a folder")]
    NotADirectory(String),
}

/// Turn a typed or picked folder into the canonical form stored in the DB:
/// trimmed, leading `~` expanded, then resolved per OS (see `resolve`).
/// Fails if it doesn't exist or isn't a folder.
pub fn normalize_folder(input: &str) -> Result<String, FolderPathError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(FolderPathError::Empty);
    }
    let expanded = expand_tilde(trimmed);
    if !expanded.exists() {
        return Err(FolderPathError::Missing(trimmed.to_string()));
    }
    if !expanded.is_dir() {
        return Err(FolderPathError::NotADirectory(trimmed.to_string()));
    }
    resolve(&expanded).ok_or_else(|| FolderPathError::Missing(trimmed.to_string()))
}

/// Unix: `canonicalize` (realpath): symlinks resolved, and on macOS the
/// on-disk case, matching the paths the watcher reports.
#[cfg(not(windows))]
fn resolve(p: &std::path::Path) -> Option<String> {
    std::fs::canonicalize(p)
        .ok()
        .map(|c| c.to_string_lossy().into_owned())
}

/// Windows: absolute only. `canonicalize` there would turn a mapped drive
/// (`Z:\Photos`) into `\\?\UNC\server\share\Photos` and change what
/// the network-share logic sees, so links and mappings are left alone.
#[cfg(windows)]
fn resolve(p: &std::path::Path) -> Option<String> {
    std::path::absolute(p).ok().map(|a| {
        let s = a.to_string_lossy();
        trim_trailing_separators(s.strip_prefix(r"\\?\").unwrap_or(&s)).to_string()
    })
}

/// `C:\Users\me\` → `C:\Users\me`, keeping a bare root (`C:\`, `\\`)
/// intact. (`~` expands via `home.join("")`, which adds a trailing one, and
/// Windows `absolute` keeps whatever separators it was given.)
#[cfg_attr(not(windows), allow(dead_code))]
fn trim_trailing_separators(s: &str) -> &str {
    let trimmed = s.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        // Root: give back one separator.
        &s[..(trimmed.len() + 1).min(s.len())]
    } else {
        trimmed
    }
}

fn expand_tilde(p: &str) -> PathBuf {
    let rest = if p == "~" {
        Some("")
    } else {
        p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\"))
    };
    match (rest, dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expands_to_home() {
        // The home folder in the same form normalize_folder produces.
        let want = resolve(&dirs::home_dir().unwrap()).unwrap();
        assert!(!want.ends_with(['\\', '/']), "trailing separator: {want}");
        assert_eq!(normalize_folder("~").unwrap(), want);
        assert_eq!(normalize_folder("  ~  ").unwrap(), want);
    }

    #[test]
    fn trailing_separators_are_trimmed_but_roots_kept() {
        assert_eq!(trim_trailing_separators(r"C:\Users\me\"), r"C:\Users\me");
        assert_eq!(trim_trailing_separators("/Users/me//"), "/Users/me");
        assert_eq!(trim_trailing_separators(r"C:\"), r"C:\");
        assert_eq!(trim_trailing_separators("/"), "/");
    }

    #[test]
    fn missing_empty_and_file_paths_are_refused() {
        assert_eq!(normalize_folder("   "), Err(FolderPathError::Empty));
        assert!(matches!(
            normalize_folder("/definitely/not/here/immichsync"),
            Err(FolderPathError::Missing(_))
        ));
        let f = tempfile::NamedTempFile::new().unwrap();
        assert!(matches!(
            normalize_folder(&f.path().to_string_lossy()),
            Err(FolderPathError::NotADirectory(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_resolve_to_the_real_folder() {
        let real = tempfile::tempdir().unwrap();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("photos-link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        let want = std::fs::canonicalize(real.path()).unwrap();
        assert_eq!(
            normalize_folder(&link.to_string_lossy()).unwrap(),
            want.to_string_lossy()
        );
    }

    /// APFS is case-insensitive by default: a typed `desktop` becomes the
    /// folder's real `Desktop`. Skipped on a case-sensitive volume.
    #[cfg(target_os = "macos")]
    #[test]
    fn case_is_corrected_on_case_insensitive_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("Desktop");
        std::fs::create_dir(&real).unwrap();
        let typed = dir.path().join("desktop");
        if !typed.exists() {
            return; // case-sensitive volume
        }
        let got = normalize_folder(&typed.to_string_lossy()).unwrap();
        assert!(got.ends_with("/Desktop"), "{got}");
    }
}

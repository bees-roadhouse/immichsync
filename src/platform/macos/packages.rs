// Package directories (macOS).
//
// Finder shows a package (an `.app`, a Photos/iPhoto/Photo Booth/iMovie/Final
// Cut library, a `.bundle` …) as a single file. Its insides belong to the app
// that owns it: ImmichSync must neither upload them nor, with After Upload =
// Trash/Delete, move files out of them and corrupt the library (#96). Whether
// a directory is a package comes from the system (`NSURLIsPackageKey`: a
// registered package type or the bundle bit), not from a list of extensions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2_foundation::{NSNumber, NSString, NSURLIsPackageKey, NSURL};

/// Results are cached per directory: the filter asks about every ancestor of
/// every file. The cache is dropped wholesale if it ever grows past this.
const CACHE_LIMIT: usize = 50_000;

fn cache() -> &'static Mutex<HashMap<PathBuf, bool>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// True when `dir` is a package directory. Unknown or unreadable → false.
pub fn is_package(dir: &Path) -> bool {
    if let Some(&hit) = cache().lock().unwrap().get(dir) {
        return hit;
    }
    let answer = query(dir);
    let mut c = cache().lock().unwrap();
    if c.len() >= CACHE_LIMIT {
        c.clear();
    }
    c.insert(dir.to_path_buf(), answer);
    answer
}

fn query(dir: &Path) -> bool {
    let Some(s) = dir.to_str() else { return false };
    let url = NSURL::fileURLWithPath(&NSString::from_str(s));
    let mut value = None;
    // SAFETY: reading one documented resource key into an out-parameter.
    let ok = unsafe { url.getResourceValue_forKey_error(&mut value, NSURLIsPackageKey) };
    if ok.is_err() {
        return false;
    }
    value
        .and_then(|v| Retained::downcast::<NSNumber>(v).ok())
        .is_some_and(|n| n.boolValue())
}

#[cfg(test)]
mod tests {
    use super::is_package;

    #[test]
    fn app_and_bundle_dirs_are_packages_plain_dirs_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Thing.app");
        let bundle = dir.path().join("Plugin.bundle");
        let plain = dir.path().join("Holiday 2026");
        for d in [&app, &bundle, &plain] {
            std::fs::create_dir_all(d).unwrap();
        }
        assert!(is_package(&app));
        assert!(is_package(&bundle));
        assert!(!is_package(&plain));
        assert!(!is_package(&dir.path().join("missing")));
    }
}

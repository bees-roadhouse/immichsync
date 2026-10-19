// Self-update for the macOS `.app` bundle (#75).
//
// The Windows updater renames the running `.exe` aside. Here the unit is the
// whole bundle: unpack the release zip next to it, check the signature and
// bundle id, rename the running bundle aside, move the new one in, and
// relaunch through LaunchServices. The new instance deletes the old bundle
// at startup.
//
// Staging lives beside the bundle, not in $TMPDIR, so the final step is a
// rename on one volume (an app run from an external disk would otherwise
// fail with EXDEV half-way). Both side paths are dotfiles so Finder and
// Launchpad never show two ImmichSyncs.
//
// Quarantine: a file written by reqwest carries no `com.apple.quarantine`
// (only LaunchServices-aware downloaders such as browsers set it, and our
// Info.plist doesn't opt in with LSFileQuarantineEnabled), so Gatekeeper
// doesn't assess the relaunch. `verify` checks that and strips it if some
// future path ever adds it.
//
// Signing is ad-hoc until #93, so `codesign --verify` proves the bundle is
// intact, not who built it; the Developer ID team check goes here with #93.

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;
use tracing::{info, warn};

/// CFBundleIdentifier from packaging/macos/Info.plist.
pub const BUNDLE_ID: &str = "com.beesroadhouse.immichsync";

const QUARANTINE_XATTR: &str = "com.apple.quarantine";

#[derive(Debug, Error)]
pub enum BundleError {
    /// The running copy can't be replaced; nothing was changed.
    #[error("{0}")]
    CantUpdate(String),

    #[error("could not unpack the update: {0}")]
    Unpack(String),

    #[error("the downloaded update failed verification: {0}")]
    Verify(String),

    #[error("file error: {0}")]
    Io(#[from] std::io::Error),
}

/// The `.app` that contains `exe` (`X.app/Contents/MacOS/<exe>`), if any.
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos_dir = exe.parent()?;
    let contents = macos_dir.parent()?;
    let app = contents.parent()?;
    let is_app = app.extension().is_some_and(|e| e == "app");
    (macos_dir.file_name()? == "MacOS" && contents.file_name()? == "Contents" && is_app)
        .then(|| app.to_path_buf())
}

/// The bundle this process runs from, or why it can't be updated.
pub fn running_bundle() -> Result<PathBuf, BundleError> {
    let exe = std::env::current_exe()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    bundle_of(&exe).ok_or_else(|| {
        BundleError::CantUpdate(format!(
            "ImmichSync isn't running from ImmichSync.app ({}), so it can't update itself. \
             Download the new version from GitHub Releases.",
            exe.display()
        ))
    })
}

fn side_path(bundle: &Path, suffix: &str) -> PathBuf {
    let name = bundle
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ImmichSync.app".into());
    bundle.with_file_name(format!(".{name}.{suffix}"))
}

/// Where the new bundle is unpacked: `<parent>/.ImmichSync.app.update`.
pub fn staging_dir(bundle: &Path) -> PathBuf {
    side_path(bundle, "update")
}

/// Where the running bundle is parked: `<parent>/.ImmichSync.app.old`.
pub fn old_bundle_path(bundle: &Path) -> PathBuf {
    side_path(bundle, "old")
}

/// Refuse up front, before downloading, when the bundle can't be swapped:
/// an App Translocation copy (run straight from Downloads or a DMG while
/// quarantined) or a folder this user can't write.
pub fn preflight(bundle: &Path) -> Result<(), BundleError> {
    if bundle.to_string_lossy().contains("/AppTranslocation/") {
        return Err(BundleError::CantUpdate(
            "macOS is running ImmichSync from a temporary read-only copy. Move ImmichSync.app \
             to Applications, open it from there, then update."
                .into(),
        ));
    }
    let parent = bundle
        .parent()
        .ok_or_else(|| BundleError::CantUpdate("the app bundle has no parent folder".into()))?;
    // A real probe, not access(2): it also catches read-only volumes.
    let probe = side_path(bundle, "probe");
    match std::fs::create_dir(&probe).and_then(|()| std::fs::remove_dir(&probe)) {
        Ok(()) => Ok(()),
        Err(e) => Err(BundleError::CantUpdate(format!(
            "ImmichSync can't replace itself in {} ({e}). Move ImmichSync.app to a folder you \
             can write, such as Applications, then update.",
            parent.display()
        ))),
    }
}

/// Unpack `zip` into a fresh `staging` dir and return the `.app` inside it.
pub fn unpack(zip: &Path, staging: &Path) -> Result<PathBuf, BundleError> {
    if staging.exists() {
        std::fs::remove_dir_all(staging)?;
    }
    std::fs::create_dir_all(staging)?;
    let out = Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(zip)
        .arg(staging)
        .output()?;
    if !out.status.success() {
        return Err(BundleError::Unpack(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let apps: Vec<PathBuf> = std::fs::read_dir(staging)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .collect();
    match apps.as_slice() {
        [app] => Ok(app.clone()),
        _ => Err(BundleError::Unpack(format!(
            "expected one .app in the zip, found {}",
            apps.len()
        ))),
    }
}

/// Check the unpacked bundle: intact signature, our bundle id, and no
/// quarantine flag left to trip Gatekeeper on relaunch.
pub fn verify(app: &Path) -> Result<(), BundleError> {
    let out = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict"])
        .arg(app)
        .output()?;
    if !out.status.success() {
        return Err(BundleError::Verify(format!(
            "codesign: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    let plist = app.join("Contents/Info.plist");
    let out = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(&plist)
        .output()?;
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || id != BUNDLE_ID {
        return Err(BundleError::Verify(format!(
            "bundle id is '{id}', expected '{BUNDLE_ID}'"
        )));
    }

    if has_quarantine(app) {
        warn!(app = %app.display(), "Update carries com.apple.quarantine; removing it");
        let _ = Command::new("/usr/bin/xattr")
            .args(["-dr", QUARANTINE_XATTR])
            .arg(app)
            .status();
    }
    Ok(())
}

/// Whether `path` has the quarantine extended attribute.
pub fn has_quarantine(path: &Path) -> bool {
    Command::new("/usr/bin/xattr")
        .args(["-p", QUARANTINE_XATTR])
        .arg(path)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Swap `new_app` into `bundle`'s place, parking the running bundle at
/// `old_bundle_path`. If the second rename fails, the old bundle goes back,
/// so a failure never leaves the user without an app.
pub fn swap(bundle: &Path, new_app: &Path) -> Result<(), BundleError> {
    let old = old_bundle_path(bundle);
    if old.exists() {
        std::fs::remove_dir_all(&old)?;
    }
    info!(from = %bundle.display(), to = %old.display(), "Parking running bundle");
    std::fs::rename(bundle, &old)?;
    if let Err(e) = std::fs::rename(new_app, bundle) {
        warn!(error = %e, "Moving the new bundle in failed; restoring the old one");
        std::fs::rename(&old, bundle)?;
        return Err(e.into());
    }
    let staging = staging_dir(bundle);
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    info!(bundle = %bundle.display(), "New bundle in place");
    Ok(())
}

/// The whole apply step: preflight, unpack, verify, swap.
pub fn apply_zip(zip: &Path, bundle: &Path) -> Result<(), BundleError> {
    preflight(bundle)?;
    if has_quarantine(zip) {
        info!(zip = %zip.display(), "Downloaded zip is quarantined");
    }
    let staging = staging_dir(bundle);
    let result = unpack(zip, &staging).and_then(|app| {
        verify(&app)?;
        swap(bundle, &app)
    });
    if result.is_err() && staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

/// Remove what a previous update left: the parked old bundle and any
/// half-unpacked staging dir. Called by the new instance at startup.
pub fn cleanup(bundle: &Path) {
    for path in [old_bundle_path(bundle), staging_dir(bundle)] {
        if path.exists() {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => info!(path = %path.display(), "Removed leftover from last update"),
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "Could not remove update leftover")
                }
            }
        }
    }
}

/// Relaunch `bundle` through LaunchServices once this process has exited,
/// then exit. `open` while we're still running would just re-activate us,
/// and the new instance would lose the single-instance lock race.
pub fn relaunch(bundle: &Path) -> ! {
    let pid = std::process::id().to_string();
    info!(bundle = %bundle.display(), "Relaunching via open after exit");
    let spawned = {
        use std::os::unix::process::CommandExt;
        Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; exec /usr/bin/open "$2""#)
            .arg("sh")
            .arg(&pid)
            .arg(bundle)
            .process_group(0)
            .spawn()
    };
    if let Err(e) = spawned {
        warn!(error = %e, "Could not schedule relaunch");
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A minimal ad-hoc-signed bundle with `marker` as its payload. Ad-hoc
    /// signing touches no Keychain, so this runs headless in CI.
    fn make_bundle(dir: &Path, name: &str, id: &str, marker: &str) -> PathBuf {
        let app = dir.join(name);
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::copy("/usr/bin/true", app.join("Contents/MacOS/immichsync")).unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleExecutable</key><string>immichsync</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#
            ),
        )
        .unwrap();
        fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        fs::write(app.join("Contents/Resources/marker"), marker).unwrap();
        let ok = Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&app)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ad-hoc codesign of the test bundle failed");
        app
    }

    fn zip_bundle(app: &Path, zip: &Path) {
        let ok = Command::new("/usr/bin/ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(app)
            .arg(zip)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }

    fn marker(app: &Path) -> String {
        fs::read_to_string(app.join("Contents/Resources/marker")).unwrap()
    }

    #[test]
    fn bundle_of_finds_the_app_and_rejects_bare_binaries() {
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/ImmichSync.app/Contents/MacOS/immichsync"
            )),
            Some(PathBuf::from("/Applications/ImmichSync.app"))
        );
        assert_eq!(
            bundle_of(Path::new("/Users/x/target/release/immichsync")),
            None
        );
        assert_eq!(
            bundle_of(Path::new("/x/Foo/Contents/MacOS/immichsync")),
            None
        );
    }

    #[test]
    fn bundle_id_matches_the_shipped_info_plist() {
        let plist = include_str!("../../../packaging/macos/Info.plist");
        assert!(
            plist.contains(&format!("<string>{BUNDLE_ID}</string>")),
            "BUNDLE_ID drifted from packaging/macos/Info.plist"
        );
    }

    #[test]
    fn side_paths_are_hidden_siblings() {
        let b = Path::new("/Applications/ImmichSync.app");
        assert_eq!(
            staging_dir(b),
            PathBuf::from("/Applications/.ImmichSync.app.update")
        );
        assert_eq!(
            old_bundle_path(b),
            PathBuf::from("/Applications/.ImmichSync.app.old")
        );
    }

    #[test]
    fn apply_zip_swaps_the_bundle_and_cleanup_removes_the_old_one() {
        let tmp = tempfile::tempdir().unwrap();
        let apps = tmp.path().join("Applications");
        let build = tmp.path().join("build");
        fs::create_dir_all(&apps).unwrap();
        fs::create_dir_all(&build).unwrap();

        let installed = make_bundle(&apps, "ImmichSync.app", BUNDLE_ID, "old");
        let new = make_bundle(&build, "ImmichSync.app", BUNDLE_ID, "new");
        let zip = tmp.path().join("ImmichSync-9.9.9-macos-arm64.zip");
        zip_bundle(&new, &zip);

        apply_zip(&zip, &installed).unwrap();

        assert_eq!(marker(&installed), "new");
        assert_eq!(marker(&old_bundle_path(&installed)), "old");
        assert!(!staging_dir(&installed).exists());
        assert!(!has_quarantine(&installed));

        cleanup(&installed);
        assert!(!old_bundle_path(&installed).exists());
        assert_eq!(marker(&installed), "new");
    }

    #[test]
    fn a_tampered_bundle_fails_verify_and_leaves_the_running_one_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let installed = make_bundle(tmp.path(), "ImmichSync.app", BUNDLE_ID, "old");
        let build = tmp.path().join("build");
        fs::create_dir_all(&build).unwrap();
        let new = make_bundle(&build, "ImmichSync.app", BUNDLE_ID, "new");
        fs::write(new.join("Contents/Resources/marker"), "tampered").unwrap();
        let zip = tmp.path().join("u.zip");
        zip_bundle(&new, &zip);

        let err = apply_zip(&zip, &installed).unwrap_err();
        assert!(matches!(err, BundleError::Verify(_)), "{err}");
        assert_eq!(marker(&installed), "old");
        assert!(!staging_dir(&installed).exists());
        assert!(!old_bundle_path(&installed).exists());
    }

    #[test]
    fn a_foreign_bundle_id_fails_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let installed = make_bundle(tmp.path(), "ImmichSync.app", BUNDLE_ID, "old");
        let build = tmp.path().join("build");
        fs::create_dir_all(&build).unwrap();
        let other = make_bundle(&build, "ImmichSync.app", "com.example.other", "new");
        let zip = tmp.path().join("u.zip");
        zip_bundle(&other, &zip);

        let err = apply_zip(&zip, &installed).unwrap_err();
        assert!(matches!(err, BundleError::Verify(_)), "{err}");
        assert_eq!(marker(&installed), "old");
    }

    #[test]
    fn swap_restores_the_old_bundle_when_the_new_one_cannot_move_in() {
        let tmp = tempfile::tempdir().unwrap();
        let installed = make_bundle(tmp.path(), "ImmichSync.app", BUNDLE_ID, "old");
        let missing = tmp.path().join("nope/ImmichSync.app");

        assert!(swap(&installed, &missing).is_err());
        assert_eq!(marker(&installed), "old");
        assert!(!old_bundle_path(&installed).exists());
    }

    #[test]
    fn verify_detects_and_strips_quarantine() {
        let tmp = tempfile::tempdir().unwrap();
        let app = make_bundle(tmp.path(), "ImmichSync.app", BUNDLE_ID, "new");
        let ok = Command::new("/usr/bin/xattr")
            .args(["-w", QUARANTINE_XATTR, "0081;00000000;Safari;"])
            .arg(&app)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert!(has_quarantine(&app));
        verify(&app).unwrap();
        assert!(!has_quarantine(&app));
    }

    /// The quarantine claim in the module header, checked against a real
    /// download through the same reqwest client the updater uses. Network,
    /// so opt-in: `cargo test -- --ignored download_is_not_quarantined`.
    #[test]
    #[ignore]
    fn download_is_not_quarantined() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("download.zip");
        let bytes = reqwest::blocking::get(
            "https://github.com/bees-roadhouse/immichsync/archive/refs/heads/main.zip",
        )
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .unwrap();
        fs::write(&dest, &bytes).unwrap();
        assert!(dest.metadata().unwrap().len() > 0);
        assert!(!has_quarantine(&dest), "reqwest download was quarantined");
    }

    #[test]
    fn preflight_refuses_a_read_only_folder_and_translocation() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let ro = tmp.path().join("ro");
        fs::create_dir_all(ro.join("ImmichSync.app")).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        let result = preflight(&ro.join("ImmichSync.app"));
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(result, Err(BundleError::CantUpdate(_))),
            "{result:?}"
        );

        let translocated =
            Path::new("/private/var/folders/x/AppTranslocation/ABC/d/ImmichSync.app");
        assert!(matches!(
            preflight(translocated),
            Err(BundleError::CantUpdate(_))
        ));

        let rw = tmp.path().join("rw/ImmichSync.app");
        fs::create_dir_all(&rw).unwrap();
        preflight(&rw).unwrap();
    }
}

// PhotoKit, read-only (docs/adr/0002-photokit-library-source.md).
//
// Access: PhotoKit has no read-only level, so we ask for `.readWrite` and
// never change anything: there is no `PHAssetChangeRequest` (or any other
// change request) in this codebase, and review greps for it.
//
// Prompts: `request_access` raises the system prompt and is called only
// from the Settings toggle. Every other function checks `access()` first,
// because touching the library while access is undetermined would raise the
// prompt on its own, at a moment the user didn't choose.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, Context as _};
use block2::RcBlock;
use chrono::{DateTime, TimeZone, Utc};
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSBundle, NSDate, NSError, NSSortDescriptor, NSString, NSURL};
use objc2_photos::{
    PHAccessLevel, PHAsset, PHAssetMediaType, PHAssetResource, PHAssetResourceManager,
    PHAssetResourceRequestOptions, PHAssetResourceType, PHAssetSourceType, PHAuthorizationStatus,
    PHFetchOptions, PHPhotoLibrary, PHPhotosError,
};
use tracing::{debug, info};

use crate::upload::worker::{LibraryExport, LibraryExportError, LibraryExporter};

/// Photos library access, as macOS reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
    /// The user shared a selection of items, not the whole library.
    Limited,
}

impl Access {
    fn from_status(s: PHAuthorizationStatus) -> Self {
        match s {
            PHAuthorizationStatus::Authorized => Access::Authorized,
            PHAuthorizationStatus::Limited => Access::Limited,
            PHAuthorizationStatus::Denied => Access::Denied,
            PHAuthorizationStatus::Restricted => Access::Restricted,
            _ => Access::NotDetermined,
        }
    }

    /// Whether the library may be read without raising a prompt.
    pub fn can_read(self) -> bool {
        matches!(self, Access::Authorized | Access::Limited)
    }
}

/// Current access. Never prompts.
pub fn access() -> Access {
    Access::from_status(unsafe {
        PHPhotoLibrary::authorizationStatusForAccessLevel(PHAccessLevel::ReadWrite)
    })
}

/// Whether this process can show the permission prompt at all: it must be
/// the `.app` bundle with `NSPhotoLibraryUsageDescription`. A bare binary
/// that asks is killed by TCC, so callers check this first.
pub fn can_prompt() -> bool {
    let bundle = NSBundle::mainBundle();
    bundle.bundleIdentifier().is_some()
        && bundle
            .objectForInfoDictionaryKey(&NSString::from_str("NSPhotoLibraryUsageDescription"))
            .is_some()
}

/// Ask for access, raising the system prompt if macOS hasn't asked before.
/// Blocks until the user answers. Call only in response to the user's own
/// click (the Settings toggle), never at startup.
pub fn request_access() -> anyhow::Result<Access> {
    if !can_prompt() {
        return Err(anyhow!(
            "the Photos library needs ImmichSync.app (no usage description in this binary)"
        ));
    }
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |status: PHAuthorizationStatus| {
        let _ = tx.send(status);
    });
    unsafe {
        PHPhotoLibrary::requestAuthorizationForAccessLevel_handler(
            PHAccessLevel::ReadWrite,
            &handler,
        );
    }
    let status = rx.recv().context("authorization handler never ran")?;
    let access = Access::from_status(status);
    info!(?access, "Photos library access answered");
    Ok(access)
}

/// Fetch options for what ImmichSync syncs: the user's own library, images
/// and videos, no hidden items, burst picks only, newest first.
fn sync_fetch_options() -> Retained<PHFetchOptions> {
    unsafe {
        let options = PHFetchOptions::new();
        options.setIncludeHiddenAssets(false);
        options.setIncludeAllBurstAssets(false);
        options.setIncludeAssetSourceTypes(PHAssetSourceType::TypeUserLibrary);
        let newest_first = NSSortDescriptor::sortDescriptorWithKey_ascending(
            Some(&NSString::from_str("creationDate")),
            false,
        );
        options.setSortDescriptors(Some(&NSArray::from_retained_slice(&[newest_first])));
        options
    }
}

/// `localIdentifier`s of the newest `limit` images and videos. Empty, with
/// no prompt, unless access is already granted.
pub fn newest_asset_ids(limit: usize) -> Vec<String> {
    if !access().can_read() {
        debug!("Photos library not readable; not fetching");
        return Vec::new();
    }
    let mut ids = Vec::new();
    unsafe {
        let result = PHAsset::fetchAssetsWithOptions(Some(&sync_fetch_options()));
        for i in 0..result.count() {
            if ids.len() >= limit {
                break;
            }
            let asset = result.objectAtIndex(i);
            let kind = asset.mediaType();
            if kind == PHAssetMediaType::Image || kind == PHAssetMediaType::Video {
                ids.push(asset.localIdentifier().to_string());
            }
        }
    }
    ids
}

/// How long one export may take before it is treated as failed. Local
/// originals are disk copies; large videos still take a while.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The original resource of an asset: `.photo` for images, `.video` for
/// videos. Never the rendered edit (`.fullSizePhoto`); ADR 0002 §Originals.
fn original_resource(asset: &PHAsset) -> Option<(Retained<PHAssetResource>, &'static str)> {
    let resources = unsafe { PHAssetResource::assetResourcesForAsset(asset) };
    let want = if unsafe { asset.mediaType() } == PHAssetMediaType::Video {
        (PHAssetResourceType::Video, "video")
    } else {
        (PHAssetResourceType::Photo, "photo")
    };
    resources
        .iter()
        .find(|r| unsafe { r.r#type() } == want.0)
        .map(|r| (r, want.1))
}

fn to_utc(date: Option<Retained<NSDate>>) -> DateTime<Utc> {
    date.and_then(|d| {
        let secs = d.timeIntervalSince1970();
        Utc.timestamp_opt(secs.trunc() as i64, (secs.fract() * 1e9) as u32)
            .single()
    })
    .unwrap_or_else(Utc::now)
}

/// A file name safe to create: the last path component of PhotoKit's
/// `originalFilename`, never empty.
fn safe_file_name(original: &str) -> String {
    let name = std::path::Path::new(original)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.is_empty() || name == "." || name == ".." {
        "original".to_string()
    } else {
        name
    }
}

/// Where exports are written: `$TMPDIR/immichsync-photos/<uuid>/<name>`.
/// Nothing else writes here.
pub fn export_root() -> PathBuf {
    std::env::temp_dir().join("immichsync-photos")
}

/// Delete exports a crash or force-quit left behind (between export and the
/// entry settling). They are private originals, so they don't wait for the
/// OS to clean `$TMPDIR`. Call at startup, before the pipeline runs.
pub fn sweep_stale_exports() {
    let removed = sweep(&export_root());
    if removed > 0 {
        info!(
            removed,
            "Removed leftover Photos exports from a previous run"
        );
    }
}

/// Remove leftovers under `root`; returns how many entries were removed.
fn sweep(root: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            let p = e.path();
            let gone = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
            gone.is_ok()
        })
        .count()
}

/// Exports originals with `PHAssetResourceManager`, local copies only.
pub struct PhotoKitExporter;

impl LibraryExporter for PhotoKitExporter {
    fn export_original(&self, local_id: &str) -> Result<LibraryExport, LibraryExportError> {
        if !access().can_read() {
            return Err(LibraryExportError::Other(anyhow!(
                "Photos library access is not granted"
            )));
        }

        let ids = NSArray::from_retained_slice(&[NSString::from_str(local_id)]);
        let asset = unsafe {
            PHAsset::fetchAssetsWithLocalIdentifiers_options(&ids, Some(&sync_fetch_options()))
                .firstObject()
        }
        .ok_or(LibraryExportError::Gone)?;
        let (resource, kind) = original_resource(&asset).ok_or_else(|| {
            LibraryExportError::Other(anyhow!("asset has no original photo or video resource"))
        })?;

        let name = safe_file_name(&unsafe { resource.originalFilename() }.to_string());
        let dir: PathBuf = export_root().join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create {}", dir.display()))
            .map_err(LibraryExportError::Other)?;
        let path = dir.join(&name);

        let options = unsafe { PHAssetResourceRequestOptions::new() };
        // Local originals only in this slice; iCloud downloads are slice 3.
        unsafe { options.setNetworkAccessAllowed(false) };

        let (tx, rx) = mpsc::channel::<Option<(isize, String)>>();
        let done = RcBlock::new(move |err: *mut NSError| {
            let outcome =
                unsafe { err.as_ref() }.map(|e| (e.code(), e.localizedDescription().to_string()));
            let _ = tx.send(outcome);
        });
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        unsafe {
            PHAssetResourceManager::defaultManager()
                .writeDataForAssetResource_toFile_options_completionHandler(
                    &resource,
                    &url,
                    Some(&options),
                    &done,
                );
        }

        let outcome = rx.recv_timeout(EXPORT_TIMEOUT);
        let cleanup = |dir: &PathBuf| {
            let _ = std::fs::remove_dir_all(dir);
        };
        match outcome {
            Ok(None) => {
                debug!(%local_id, path = %path.display(), "Photos original exported");
                Ok(LibraryExport {
                    path,
                    resource: kind,
                    created_at: to_utc(unsafe { asset.creationDate() }),
                    modified_at: to_utc(unsafe { asset.modificationDate() }),
                })
            }
            Ok(Some((code, _))) if code == PHPhotosError::NetworkAccessRequired.0 => {
                cleanup(&dir);
                Err(LibraryExportError::CloudOnly)
            }
            Ok(Some((code, msg))) => {
                cleanup(&dir);
                Err(LibraryExportError::Other(anyhow!(
                    "export failed ({code}): {msg}"
                )))
            }
            Err(_) => {
                // PhotoKit's write may still be in flight; removing the
                // directory makes it fail harmlessly rather than land.
                cleanup(&dir);
                Err(LibraryExportError::Other(anyhow!(
                    "export timed out after {}s",
                    EXPORT_TIMEOUT.as_secs()
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sweep_removes_leftover_exports_and_tolerates_no_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("immichsync-photos");
        assert_eq!(sweep(&root), 0, "a missing root is fine");

        for id in ["a", "b"] {
            std::fs::create_dir_all(root.join(id)).unwrap();
            std::fs::write(root.join(id).join("IMG.HEIC"), b"private").unwrap();
        }
        assert_eq!(sweep(&root), 2);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn safe_file_name_keeps_only_the_last_component() {
        assert_eq!(safe_file_name("IMG_0001.HEIC"), "IMG_0001.HEIC");
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name(""), "original");
        assert_eq!(safe_file_name(".."), "original");
    }

    #[test]
    fn only_authorized_and_limited_can_read() {
        assert!(Access::Authorized.can_read());
        assert!(Access::Limited.can_read());
        assert!(!Access::NotDetermined.can_read());
        assert!(!Access::Denied.can_read());
        assert!(!Access::Restricted.can_read());
    }

    /// Read-only, enforced: a PhotoKit change request (the only way to edit
    /// or delete a photo) can only be applied through `performChanges`, so no
    /// source file may call it. Comments naming the rule are fine.
    #[test]
    fn nothing_in_src_can_change_the_photos_library() {
        fn walk(dir: &std::path::Path, hits: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, hits);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for (n, line) in text.lines().enumerate() {
                        let code = line.split("//").next().unwrap_or("");
                        let needle = ["perform", "Changes"].concat();
                        if code.contains(&needle)
                            || code.contains(&["ChangeRequest", "::"].concat())
                        {
                            hits.push(format!("{}:{}", path.display(), n + 1));
                        }
                    }
                }
            }
        }
        let mut hits = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut hits,
        );
        assert!(
            hits.is_empty(),
            "Photos library write paths found: {hits:?}"
        );
    }

    /// The test binary is not a bundle, so it must never be allowed to
    /// prompt (TCC would kill it), and `request_access` must refuse.
    #[test]
    fn a_bare_binary_cannot_prompt() {
        assert!(!can_prompt());
        assert!(request_access().is_err());
    }
}

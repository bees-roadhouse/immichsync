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
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, ClassType, DefinedClass, Message};
use objc2_foundation::{
    NSArray, NSBundle, NSData, NSDate, NSError, NSKeyedArchiver, NSKeyedUnarchiver, NSSet,
    NSSortDescriptor, NSString, NSURL,
};
use objc2_photos::{
    PHAccessLevel, PHAsset, PHAssetCollection, PHAssetCollectionSubtype, PHAssetCollectionType,
    PHAssetMediaType, PHAssetResource, PHAssetResourceManager, PHAssetResourceRequestOptions,
    PHAssetResourceType, PHAssetSourceType, PHAuthorizationStatus, PHChange, PHCollection,
    PHCollectionList, PHFetchOptions, PHObjectType, PHPersistentChange, PHPersistentChangeToken,
    PHPhotoLibrary, PHPhotoLibraryChangeObserver, PHPhotosError,
};
use tracing::{debug, info};

use crate::upload::worker::{LibraryAlbum, LibraryExport, LibraryExportError, LibraryExporter};

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
fn sync_fetch_options(newest_first: bool) -> Retained<PHFetchOptions> {
    unsafe {
        let options = PHFetchOptions::new();
        options.setIncludeHiddenAssets(false);
        options.setIncludeAllBurstAssets(false);
        options.setIncludeAssetSourceTypes(PHAssetSourceType::TypeUserLibrary);
        let order = NSSortDescriptor::sortDescriptorWithKey_ascending(
            Some(&NSString::from_str("creationDate")),
            !newest_first,
        );
        options.setSortDescriptors(Some(&NSArray::from_retained_slice(&[order])));
        options
    }
}

/// The images and videos in a fetch result, as `localIdentifier`s.
fn syncable_ids(result: &objc2_photos::PHFetchResult<PHAsset>) -> Vec<String> {
    let mut ids = Vec::new();
    for i in 0..unsafe { result.count() } {
        let asset = unsafe { result.objectAtIndex(i) };
        let kind = unsafe { asset.mediaType() };
        if kind == PHAssetMediaType::Image || kind == PHAssetMediaType::Video {
            ids.push(unsafe { asset.localIdentifier() }.to_string());
        }
    }
    ids
}

/// The library through PhotoKit, for `watch::photos::sync_pass` (#81).
/// Every method refuses unless access is already granted, so a sync pass can
/// never raise the permission prompt.
pub struct PhotoKitLibrary;

fn require_access() -> anyhow::Result<()> {
    if access().can_read() {
        Ok(())
    } else {
        Err(anyhow!("Photos library access is not granted"))
    }
}

fn archive(token: &PHPersistentChangeToken) -> anyhow::Result<Vec<u8>> {
    let data = unsafe {
        NSKeyedArchiver::archivedDataWithRootObject_requiringSecureCoding_error(token, true)
    }
    .map_err(|e| anyhow!("archive change token: {}", e.localizedDescription()))?;
    Ok(data.to_vec())
}

fn unarchive(bytes: &[u8]) -> Option<Retained<PHPersistentChangeToken>> {
    let data = NSData::with_bytes(bytes);
    let obj = unsafe {
        NSKeyedUnarchiver::unarchivedObjectOfClass_fromData_error(
            PHPersistentChangeToken::class(),
            &data,
        )
    }
    .ok()?;
    obj.downcast::<PHPersistentChangeToken>().ok()
}

fn nsset_strings(set: &NSSet<NSString>) -> Vec<String> {
    set.iter().map(|s| s.to_string()).collect()
}

impl crate::watch::photos::PhotosLibrary for PhotoKitLibrary {
    fn current_token(&self) -> anyhow::Result<Vec<u8>> {
        require_access()?;
        let token = unsafe { PHPhotoLibrary::sharedPhotoLibrary().currentChangeToken() };
        archive(&token)
    }

    fn all_asset_ids(&self, newest_first: bool) -> anyhow::Result<Vec<String>> {
        require_access()?;
        let result =
            unsafe { PHAsset::fetchAssetsWithOptions(Some(&sync_fetch_options(newest_first))) };
        Ok(syncable_ids(&result))
    }

    fn changes_since(
        &self,
        token: &[u8],
    ) -> Result<crate::watch::photos::LibraryChanges, crate::watch::photos::ChangesError> {
        use crate::watch::photos::{ChangesError, LibraryChanges};
        require_access()?;
        // A token we can't read back is as good as expired: enumerate.
        let Some(since) = unarchive(token) else {
            return Err(ChangesError::Expired);
        };
        let fetched = unsafe {
            PHPhotoLibrary::sharedPhotoLibrary().fetchPersistentChangesSinceToken_error(&since)
        };
        let result = match fetched {
            Ok(r) => r,
            Err(e)
                if e.code() == PHPhotosError::PersistentChangeTokenExpired.0
                    || e.code() == PHPhotosError::PersistentChangeDetailsUnavailable.0 =>
            {
                return Err(ChangesError::Expired)
            }
            Err(e) => {
                return Err(ChangesError::Other(anyhow!(
                    "fetch persistent changes: {}",
                    e.localizedDescription()
                )))
            }
        };

        let collected = std::cell::RefCell::new((Vec::new(), Vec::new(), Vec::new(), None));
        let block = RcBlock::new(|change: std::ptr::NonNull<PHPersistentChange>, _stop| {
            let change = unsafe { change.as_ref() };
            let mut c = collected.borrow_mut();
            if let Ok(d) = unsafe { change.changeDetailsForObjectType_error(PHObjectType::Asset) } {
                let (ins, upd, del) = unsafe {
                    (
                        d.insertedLocalIdentifiers(),
                        d.updatedLocalIdentifiers(),
                        d.deletedLocalIdentifiers(),
                    )
                };
                c.0.extend(nsset_strings(&ins));
                c.1.extend(nsset_strings(&upd));
                c.2.extend(nsset_strings(&del));
            }
            c.3 = Some(unsafe { change.changeToken() });
        });
        unsafe { result.enumerateChangesWithBlock(&block) };
        drop(block);
        let (inserted, updated, deleted, last) = collected.into_inner();

        // Inserts go through the same filter as a full pass (user library,
        // no hidden, burst picks, images and videos); an insert deleted since
        // isn't returned at all.
        let inserted = if inserted.is_empty() {
            Vec::new()
        } else {
            let ids: Vec<Retained<NSString>> =
                inserted.iter().map(|s| NSString::from_str(s)).collect();
            let result = unsafe {
                PHAsset::fetchAssetsWithLocalIdentifiers_options(
                    &NSArray::from_retained_slice(&ids),
                    Some(&sync_fetch_options(true)),
                )
            };
            syncable_ids(&result)
        };
        let token = match last {
            Some(t) => archive(&t)?,
            None => token.to_vec(),
        };
        Ok(LibraryChanges {
            inserted,
            updated,
            deleted,
            token,
        })
    }
}

/// Whether a Photos album subtype is mirrored to Immich: only albums the
/// user made. Shared albums (ADR 0002), albums synced from iTunes/Finder,
/// camera-import albums and My Photo Stream are not; smart albums are a
/// different collection type and never fetched.
fn is_mirrored_subtype(subtype: PHAssetCollectionSubtype) -> bool {
    subtype == PHAssetCollectionSubtype::AlbumRegular
}

/// Folder titles above `collection`, outermost first.
fn folder_path(collection: &PHCollection) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Retained<PHCollection> = collection.retain();
    // Folders nest; 32 levels is far beyond any real library and stops a
    // cycle, should PhotoKit ever report one.
    for _ in 0..32 {
        let parents = unsafe {
            PHCollectionList::fetchCollectionListsContainingCollection_options(&current, None)
        };
        let Some(parent) = (unsafe { parents.firstObject() }) else {
            break;
        };
        out.push(
            unsafe { parent.localizedTitle() }
                .map(|t| t.to_string())
                .unwrap_or_default(),
        );
        current = Retained::into_super(parent);
    }
    out.reverse();
    out
}

/// The user albums `asset` is in, named for Immich.
fn user_albums(asset: &PHAsset) -> Vec<LibraryAlbum> {
    let collections = unsafe {
        PHAssetCollection::fetchAssetCollectionsContainingAsset_withType_options(
            asset,
            PHAssetCollectionType::Album,
            None,
        )
    };
    let mut albums = Vec::new();
    for i in 0..unsafe { collections.count() } {
        let c = unsafe { collections.objectAtIndex(i) };
        if !is_mirrored_subtype(unsafe { c.assetCollectionSubtype() }) {
            continue;
        }
        let title = unsafe { c.localizedTitle() }
            .map(|t| t.to_string())
            .unwrap_or_default();
        let folders = folder_path(&c);
        albums.push(LibraryAlbum {
            local_id: unsafe { c.localIdentifier() }.to_string(),
            title: crate::watch::photos::album_title(&folders, &title),
        });
    }
    albums
}

// ── Change observer (#81) ──────────────────────────────────────────────────

struct ObserverIvars {
    tx: std::sync::Mutex<mpsc::Sender<()>>,
}

objc2::define_class!(
    // PhotoKit delivers change notifications on a background queue; the
    // only state touched there is the mutex-guarded sender.
    #[unsafe(super(NSObject))]
    #[name = "ImmichSyncPhotosChangeObserver"]
    #[ivars = ObserverIvars]
    struct ChangeObserver;

    unsafe impl NSObjectProtocol for ChangeObserver {}

    unsafe impl PHPhotoLibraryChangeObserver for ChangeObserver {
        #[unsafe(method(photoLibraryDidChange:))]
        fn photo_library_did_change(&self, _change: &PHChange) {
            if let Ok(tx) = self.ivars().tx.lock() {
                let _ = tx.send(());
            }
        }
    }
);

/// A registered change observer; unregistered on drop. Keep it on the
/// thread that made it.
pub struct ChangeWatch(Retained<ChangeObserver>);

impl Drop for ChangeWatch {
    fn drop(&mut self) {
        unsafe {
            PHPhotoLibrary::sharedPhotoLibrary()
                .unregisterChangeObserver(ProtocolObject::from_ref(&*self.0));
        }
    }
}

/// Send `()` on `tx` whenever the library changes. `None` (and nothing
/// registered) unless access is already granted, so it never prompts.
pub fn watch_changes(tx: mpsc::Sender<()>) -> Option<ChangeWatch> {
    if !access().can_read() {
        return None;
    }
    let this = ChangeObserver::alloc().set_ivars(ObserverIvars {
        tx: std::sync::Mutex::new(tx),
    });
    let observer: Retained<ChangeObserver> = unsafe { objc2::msg_send![super(this), init] };
    unsafe {
        PHPhotoLibrary::sharedPhotoLibrary()
            .registerChangeObserver(ProtocolObject::from_ref(&*observer));
    }
    Some(ChangeWatch(observer))
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
            PHAsset::fetchAssetsWithLocalIdentifiers_options(&ids, Some(&sync_fetch_options(true)))
                .firstObject()
        }
        .ok_or(LibraryExportError::Gone)?;
        let (resource, kind) = original_resource(&asset).ok_or_else(|| {
            LibraryExportError::Other(anyhow!("asset has no original photo or video resource"))
        })?;

        let dir: PathBuf = export_root().join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create {}", dir.display()))
            .map_err(LibraryExportError::Other)?;
        let cleanup = |dir: &PathBuf| {
            let _ = std::fs::remove_dir_all(dir);
        };

        let name = safe_file_name(&unsafe { resource.originalFilename() }.to_string());
        let path = dir.join(&name);
        if let Err(e) = write_resource(&resource, &path) {
            cleanup(&dir);
            return Err(e);
        }

        // A Live Photo's motion part: its original `.pairedVideo`, never the
        // edited `.fullSizePairedVideo` (#83). Both parts or neither.
        let paired_video = match paired_video_resource(&asset) {
            None => None,
            Some(video) => {
                let mut vname = safe_file_name(&unsafe { video.originalFilename() }.to_string());
                if vname == name {
                    vname = format!("motion-{vname}");
                }
                let vpath = dir.join(vname);
                if let Err(e) = write_resource(&video, &vpath) {
                    cleanup(&dir);
                    return Err(e);
                }
                Some(vpath)
            }
        };

        debug!(%local_id, path = %path.display(), live = paired_video.is_some(), "Photos original exported");
        Ok(LibraryExport {
            path,
            resource: kind,
            created_at: to_utc(unsafe { asset.creationDate() }),
            modified_at: to_utc(unsafe { asset.modificationDate() }),
            albums: user_albums(&asset),
            paired_video,
        })
    }
}

/// Write one resource's bytes to `path`, local copies only.
fn write_resource(
    resource: &PHAssetResource,
    path: &std::path::Path,
) -> Result<(), LibraryExportError> {
    let options = unsafe { PHAssetResourceRequestOptions::new() };
    // Local originals only for now; iCloud downloads are #82.
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
                resource,
                &url,
                Some(&options),
                &done,
            );
    }

    match rx.recv_timeout(EXPORT_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some((code, _))) if code == PHPhotosError::NetworkAccessRequired.0 => {
            Err(LibraryExportError::CloudOnly)
        }
        Ok(Some((code, msg))) => Err(LibraryExportError::Other(anyhow!(
            "export failed ({code}): {msg}"
        ))),
        // PhotoKit's write may still be in flight; the caller removes the
        // directory, which makes it fail harmlessly rather than land.
        Err(_) => Err(LibraryExportError::Other(anyhow!(
            "export timed out after {}s",
            EXPORT_TIMEOUT.as_secs()
        ))),
    }
}

/// A Live Photo still's original motion video, if it has one.
fn paired_video_resource(asset: &PHAsset) -> Option<Retained<PHAssetResource>> {
    if unsafe { asset.mediaType() } != PHAssetMediaType::Image {
        return None;
    }
    let resources = unsafe { PHAssetResource::assetResourcesForAsset(asset) };
    resources
        .iter()
        .find(|r| unsafe { r.r#type() } == PHAssetResourceType::PairedVideo)
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

    /// The stored token survives the archive → bytes → unarchive trip, and
    /// junk bytes read back as "no token" (which the sync treats as
    /// expired: enumerate). No library access involved.
    #[test]
    fn change_tokens_archive_and_unarchive() {
        let token = unsafe { PHPersistentChangeToken::new() };
        let bytes = archive(&token).unwrap();
        assert!(!bytes.is_empty());
        assert!(unarchive(&bytes).is_some());
        assert!(unarchive(b"not an archive").is_none());
    }

    #[test]
    fn only_regular_user_albums_are_mirrored() {
        assert!(is_mirrored_subtype(PHAssetCollectionSubtype::AlbumRegular));
        for other in [
            PHAssetCollectionSubtype::AlbumCloudShared,
            PHAssetCollectionSubtype::AlbumSyncedAlbum,
            PHAssetCollectionSubtype::AlbumSyncedEvent,
            PHAssetCollectionSubtype::AlbumSyncedFaces,
            PHAssetCollectionSubtype::AlbumImported,
            PHAssetCollectionSubtype::AlbumMyPhotoStream,
            PHAssetCollectionSubtype::SmartAlbumFavorites,
        ] {
            assert!(!is_mirrored_subtype(other), "{other:?}");
        }
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

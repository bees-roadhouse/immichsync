// The one write path into the Photos library (ADR 0002 Amendment 1, #116).
//
// Everything else in ImmichSync only reads the library; the guard test in
// `platform::photos` fails if a change request or `performChanges` appears
// anywhere but this file, or if `delete_assets` is called from anywhere but
// `PhotoKitDeleteLibrary`, which `watch::photos::delete::execute` drives.
//
// macOS shows its own confirmation for every call ("Allow ImmichSync to
// delete N photos?"); a "Don't Allow" comes back as `Ok(false)`. Deleted
// photos go to Recently Deleted for 30 days.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::anyhow;
use block2::RcBlock;
use objc2::runtime::{Bool, ProtocolObject};
use objc2_foundation::{NSArray, NSError, NSString};
use objc2_photos::{PHAsset, PHAssetChangeRequest, PHPhotoLibrary, PHPhotosError};

/// How long to wait for the user to answer the confirmation.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Move a verified batch to Recently Deleted, after macOS's confirmation.
/// `Ok(true)` deleted, `Ok(false)` the user declined. Blocks until answered.
/// Takes a `VerifiedBatch`, which only `watch::photos::delete::execute` can
/// build, so nothing else can reach this.
pub(super) fn delete_assets(
    batch: &crate::watch::photos::delete::VerifiedBatch,
) -> anyhow::Result<bool> {
    let local_ids = batch.local_ids();
    if local_ids.is_empty() {
        return Ok(true);
    }
    let ids: Vec<_> = local_ids.iter().map(|s| NSString::from_str(s)).collect();
    let assets = unsafe {
        PHAsset::fetchAssetsWithLocalIdentifiers_options(&NSArray::from_retained_slice(&ids), None)
    };
    if unsafe { assets.count() } != local_ids.len() {
        return Err(anyhow!(
            "{} of {} photos are no longer in the library; nothing deleted",
            local_ids.len() - unsafe { assets.count() },
            local_ids.len()
        ));
    }

    let change = RcBlock::new(move || unsafe {
        PHAssetChangeRequest::deleteAssets(ProtocolObject::from_ref(&*assets));
    });
    let (tx, rx) = mpsc::channel::<Result<bool, (isize, String)>>();
    let done = RcBlock::new(move |ok: Bool, err: *mut NSError| {
        let outcome = if ok.as_bool() {
            Ok(true)
        } else {
            Err(unsafe { err.as_ref() }
                .map(|e| (e.code(), e.localizedDescription().to_string()))
                .unwrap_or((0, "unknown error".to_string())))
        };
        let _ = tx.send(outcome);
    });
    unsafe {
        PHPhotoLibrary::sharedPhotoLibrary()
            .performChanges_completionHandler(RcBlock::as_ptr(&change), Some(&done));
    }

    match rx.recv_timeout(ANSWER_TIMEOUT) {
        Ok(Ok(_)) => Ok(true),
        Ok(Err((code, _))) if code == PHPhotosError::UserCancelled.0 => Ok(false),
        Ok(Err((code, msg))) => Err(anyhow!("delete failed ({code}): {msg}")),
        Err(_) => Err(anyhow!("no answer to the delete confirmation")),
    }
}

// Photos library source (docs/adr/0002-photokit-library-source.md).
//
// When the library is enabled and access is granted, queue its assets. A
// queue entry names the asset by `localIdentifier`; the worker exports its
// original when it picks the entry up (`upload::worker::process_library_item`).
//
// Sync (#81, slice 2): the first pass, or one after the stored change token
// has expired, enumerates the whole library; every later pass asks PhotoKit
// for persistent changes since the stored token and queues only the
// inserts. Edits are ignored (we upload originals, which an edit doesn't
// change) and deletes are never propagated. The token is written only after
// its pass's assets are queued, so a pass cut short just runs again, and
// `photos_asset_seen` keeps it from queuing anything twice.
//
// PhotoKit itself lives in `platform::photos`; this module only decides
// what to queue, so it is tested without a library or a TCC grant.

use tracing::{debug, info, warn};

use crate::upload::queue::QueueStore;

/// How long the source waits after a change notification for more to
/// arrive before syncing (an import fires many).
pub const CHANGE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(5);

/// The Photos source loop: one pass now, then one per burst of change
/// signals (debounced), until the signal sender is dropped. Passes are
/// skipped while `enabled` is false (the toggle was turned off). Errors are
/// logged and retried on the next signal.
pub fn run_source(
    library: &dyn PhotosLibrary,
    store: &dyn QueueStore,
    newest_first: bool,
    signals: std::sync::mpsc::Receiver<()>,
    enabled: &std::sync::atomic::AtomicBool,
    debounce: std::time::Duration,
) {
    use std::sync::atomic::Ordering;
    let pass = || {
        if !enabled.load(Ordering::SeqCst) {
            debug!("Photos library sync is off; skipping pass");
            return;
        }
        if let Err(e) = sync_pass(library, store, newest_first) {
            warn!(error = %e, "Photos library sync pass failed; will retry on the next change");
        }
    };

    pass();
    while signals.recv().is_ok() {
        // Let the burst finish: keep absorbing signals until it's quiet.
        while signals.recv_timeout(debounce).is_ok() {}
        pass();
    }
    debug!("Photos source stopped");
}

/// Immich album name for a Photos album: Immich has no nesting, so an album
/// inside folders is flattened to `Folder / Sub / Album` (outermost first).
/// Blank parts are dropped; an untitled album is "Untitled album".
pub fn album_title(folders_outermost_first: &[String], title: &str) -> String {
    let title = title.trim();
    let title = if title.is_empty() {
        "Untitled album"
    } else {
        title
    };
    folders_outermost_first
        .iter()
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .chain(std::iter::once(title))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// What PhotoKit reported since a change token. Ids are already filtered to
/// what ImmichSync syncs (user library, no hidden, burst picks, images and
/// videos).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryChanges {
    pub inserted: Vec<String>,
    /// Edited assets. Ignored: originals don't change on edit.
    pub updated: Vec<String>,
    /// Deleted assets. Never propagated to Immich.
    pub deleted: Vec<String>,
    /// The token to store once these changes are queued.
    pub token: Vec<u8>,
}

/// Why changes since a token couldn't be read.
#[derive(Debug, thiserror::Error)]
pub enum ChangesError {
    /// The token is too old (or from another library): enumerate everything.
    #[error("the change token has expired")]
    Expired,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// The library as the sync pass needs it; `platform::photos` implements it
/// with PhotoKit, tests with a fake.
pub trait PhotosLibrary {
    /// The library's current change token, archived.
    fn current_token(&self) -> anyhow::Result<Vec<u8>>;
    /// Every syncable asset, newest first or oldest first.
    fn all_asset_ids(&self, newest_first: bool) -> anyhow::Result<Vec<String>>;
    /// Changes since an archived token.
    fn changes_since(&self, token: &[u8]) -> Result<LibraryChanges, ChangesError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncKind {
    /// Enumerated the whole library.
    Full,
    /// Read persistent changes since the stored token.
    Incremental,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    pub kind: SyncKind,
    /// Assets considered (all of them, or the inserts).
    pub seen: usize,
    pub queued: usize,
}

/// One sync pass; see the module comment.
pub fn sync_pass(
    library: &dyn PhotosLibrary,
    store: &dyn QueueStore,
    newest_first: bool,
) -> anyhow::Result<SyncOutcome> {
    if let Some(token) = store.photos_change_token()? {
        match library.changes_since(&token) {
            Ok(changes) => {
                if !changes.updated.is_empty() || !changes.deleted.is_empty() {
                    debug!(
                        updated = changes.updated.len(),
                        deleted = changes.deleted.len(),
                        "Photos edits and deletes ignored (originals only, no delete sync)"
                    );
                }
                let queued = store.enqueue_photos_new(&changes.inserted)?;
                store.set_photos_change_token(&changes.token)?;
                info!(
                    inserted = changes.inserted.len(),
                    queued, "Photos library: incremental sync"
                );
                return Ok(SyncOutcome {
                    kind: SyncKind::Incremental,
                    seen: changes.inserted.len(),
                    queued,
                });
            }
            Err(ChangesError::Expired) => {
                info!("Photos change token expired; enumerating the whole library");
            }
            Err(ChangesError::Other(e)) => return Err(e),
        }
    }

    // Take the token before enumerating, so anything added while we walk the
    // library is picked up by the next incremental pass (a repeat is
    // harmless: it's already queued).
    let token = library.current_token()?;
    let ids = library.all_asset_ids(newest_first)?;
    let queued = store.enqueue_photos_new(&ids)?;
    store.set_photos_change_token(&token)?;
    info!(seen = ids.len(), queued, "Photos library: full sync");
    Ok(SyncOutcome {
        kind: SyncKind::Full,
        seen: ids.len(),
        queued,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A library whose assets and change log the test controls. Tokens are
    /// the change-log position as one byte.
    #[derive(Default)]
    struct FakeLibrary {
        assets: RefCell<Vec<String>>,
        log: RefCell<Vec<(&'static str, String)>>,
        expired: RefCell<bool>,
        broken: RefCell<bool>,
    }

    impl FakeLibrary {
        fn with(ids: &[&str]) -> Self {
            let lib = Self::default();
            for id in ids {
                lib.assets.borrow_mut().push(id.to_string());
            }
            lib
        }
        fn change(&self, kind: &'static str, id: &str) {
            if kind == "insert" {
                self.assets.borrow_mut().push(id.to_string());
            }
            self.log.borrow_mut().push((kind, id.to_string()));
        }
    }

    impl PhotosLibrary for FakeLibrary {
        fn current_token(&self) -> anyhow::Result<Vec<u8>> {
            Ok(vec![self.log.borrow().len() as u8])
        }
        fn all_asset_ids(&self, newest_first: bool) -> anyhow::Result<Vec<String>> {
            let mut ids = self.assets.borrow().clone();
            if newest_first {
                ids.reverse();
            }
            Ok(ids)
        }
        fn changes_since(&self, token: &[u8]) -> Result<LibraryChanges, ChangesError> {
            if *self.broken.borrow() {
                return Err(ChangesError::Other(anyhow::anyhow!(
                    "photolibraryd is away"
                )));
            }
            if *self.expired.borrow() {
                return Err(ChangesError::Expired);
            }
            let mut out = LibraryChanges::default();
            for (kind, id) in self.log.borrow().iter().skip(token[0] as usize) {
                match *kind {
                    "insert" => out.inserted.push(id.clone()),
                    "update" => out.updated.push(id.clone()),
                    _ => out.deleted.push(id.clone()),
                }
            }
            out.token = vec![self.log.borrow().len() as u8];
            Ok(out)
        }
    }

    fn store() -> DbStore {
        DbStore::new(Database::open_in_memory().unwrap())
    }

    fn queued_ids(store: &DbStore) -> Vec<String> {
        let mut v: Vec<String> = store
            .inner()
            .lock()
            .unwrap()
            .dequeue_pending(1000)
            .unwrap()
            .into_iter()
            .filter_map(|e| e.photos_local_id)
            .collect();
        v.sort();
        v
    }

    /// `run_source` with a library shared across threads.
    struct SharedLibrary(std::sync::Mutex<FakeLibrary>);
    // The fake's RefCells are only touched under the mutex.
    unsafe impl Sync for SharedLibrary {}
    unsafe impl Send for SharedLibrary {}
    impl PhotosLibrary for SharedLibrary {
        fn current_token(&self) -> anyhow::Result<Vec<u8>> {
            self.0.lock().unwrap().current_token()
        }
        fn all_asset_ids(&self, n: bool) -> anyhow::Result<Vec<String>> {
            self.0.lock().unwrap().all_asset_ids(n)
        }
        fn changes_since(&self, t: &[u8]) -> Result<LibraryChanges, ChangesError> {
            self.0.lock().unwrap().changes_since(t)
        }
    }

    #[test]
    fn the_source_syncs_at_start_and_after_each_burst_of_changes() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let lib = Arc::new(SharedLibrary(std::sync::Mutex::new(FakeLibrary::with(&[
            "A",
        ]))));
        let store = Arc::new(store());
        let enabled = Arc::new(AtomicBool::new(true));
        let (tx, rx) = std::sync::mpsc::channel();
        let t = {
            let (lib, store, enabled) = (lib.clone(), store.clone(), enabled.clone());
            std::thread::spawn(move || {
                run_source(
                    lib.as_ref(),
                    store.as_ref(),
                    true,
                    rx,
                    &enabled,
                    std::time::Duration::from_millis(20),
                )
            })
        };
        let wait_for = |n: u64| {
            for _ in 0..200 {
                if store.get_queue_stats().unwrap().pending == n {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            false
        };
        assert!(wait_for(1), "the start pass queues the library");

        // A burst of three notifications for two inserts: one more pass.
        lib.0.lock().unwrap().change("insert", "B");
        lib.0.lock().unwrap().change("insert", "C");
        for _ in 0..3 {
            tx.send(()).unwrap();
        }
        assert!(wait_for(3), "the burst is synced");

        // Turned off: changes are not picked up.
        enabled.store(false, std::sync::atomic::Ordering::SeqCst);
        lib.0.lock().unwrap().change("insert", "D");
        tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(store.get_queue_stats().unwrap().pending, 3);

        drop(tx);
        t.join().unwrap();
    }

    #[test]
    fn the_first_pass_enumerates_everything_and_stores_the_token() {
        let lib = FakeLibrary::with(&["A", "B", "C"]);
        let store = store();
        let out = sync_pass(&lib, &store, true).unwrap();
        assert_eq!(
            out,
            SyncOutcome {
                kind: SyncKind::Full,
                seen: 3,
                queued: 3
            }
        );
        assert_eq!(store.photos_change_token().unwrap(), Some(vec![0]));
        assert_eq!(queued_ids(&store), ["A", "B", "C"]);
    }

    #[test]
    fn later_passes_queue_only_inserts_and_ignore_edits_and_deletes() {
        let lib = FakeLibrary::with(&["A", "B"]);
        let store = store();
        sync_pass(&lib, &store, true).unwrap();
        lib.change("insert", "C");
        lib.change("update", "A");
        lib.change("delete", "B");

        let out = sync_pass(&lib, &store, true).unwrap();
        assert_eq!(
            out,
            SyncOutcome {
                kind: SyncKind::Incremental,
                seen: 1,
                queued: 1
            }
        );
        assert_eq!(
            queued_ids(&store),
            ["A", "B", "C"],
            "nothing re-queued, nothing removed"
        );
        assert_eq!(store.photos_change_token().unwrap(), Some(vec![3]));

        // Nothing new: an empty incremental pass.
        let out = sync_pass(&lib, &store, true).unwrap();
        assert_eq!(out.queued, 0);
    }

    #[test]
    fn an_expired_token_falls_back_to_a_full_pass_without_requeuing() {
        let lib = FakeLibrary::with(&["A", "B"]);
        let store = store();
        sync_pass(&lib, &store, true).unwrap();
        lib.change("insert", "C");
        *lib.expired.borrow_mut() = true;

        let out = sync_pass(&lib, &store, true).unwrap();
        assert_eq!(
            out,
            SyncOutcome {
                kind: SyncKind::Full,
                seen: 3,
                queued: 1
            }
        );
    }

    /// A restart before the token is written (a pass cut short) simply runs
    /// the full pass again; already-queued or uploaded assets aren't queued
    /// a second time.
    #[test]
    fn a_pass_cut_short_resumes_without_requeuing_or_reuploading() {
        let lib = FakeLibrary::with(&["A", "B", "C"]);
        let store = store();
        // The interrupted pass got as far as queuing A and B ...
        store
            .enqueue_photos_new(&["A".to_string(), "B".to_string()])
            .unwrap();
        // ... and A had even uploaded.
        store
            .inner()
            .lock()
            .unwrap()
            .record_photos_asset("A", "photo", "sha", "imm-a", None)
            .unwrap();

        let out = sync_pass(&lib, &store, true).unwrap();
        assert_eq!(
            out,
            SyncOutcome {
                kind: SyncKind::Full,
                seen: 3,
                queued: 1
            }
        );
        assert_eq!(queued_ids(&store), ["A", "B", "C"]);
    }

    #[test]
    fn a_failed_change_read_keeps_the_token_and_queues_nothing() {
        let lib = FakeLibrary::with(&["A"]);
        let store = store();
        sync_pass(&lib, &store, true).unwrap();
        lib.change("insert", "B");
        *lib.broken.borrow_mut() = true;

        assert!(sync_pass(&lib, &store, true).is_err());
        assert_eq!(store.photos_change_token().unwrap(), Some(vec![0]));
        assert_eq!(queued_ids(&store), ["A"]);
    }

    #[test]
    fn the_full_pass_queues_in_the_configured_order() {
        let lib = FakeLibrary::with(&["old", "mid", "new"]);
        let store = store();
        sync_pass(&lib, &store, true).unwrap();
        let order: Vec<String> = store
            .inner()
            .lock()
            .unwrap()
            .get_recent_queue_entries(10)
            .unwrap()
            .into_iter()
            .rev()
            .filter_map(|e| e.photos_local_id)
            .collect();
        assert_eq!(order, ["new", "mid", "old"]);
    }

    #[test]
    fn album_titles_flatten_folders_outermost_first() {
        assert_eq!(album_title(&[], "Hawaii 2026"), "Hawaii 2026");
        assert_eq!(
            album_title(&["Trips".into(), "2026".into()], "Hawaii"),
            "Trips / 2026 / Hawaii"
        );
        assert_eq!(
            album_title(&[" ".into(), "Trips".into()], " Hawaii "),
            "Trips / Hawaii"
        );
        assert_eq!(album_title(&[], "  "), "Untitled album");
    }
    use crate::db::{Database, DbStore};

    #[test]
    fn queues_each_new_asset_once() {
        let store = DbStore::new(Database::open_in_memory().unwrap());
        let ids: Vec<String> = ["C", "B", "A"].iter().map(|s| s.to_string()).collect();

        assert_eq!(store.enqueue_photos_new(&ids).unwrap(), 3);
        assert_eq!(
            store.enqueue_photos_new(&ids).unwrap(),
            0,
            "a second pass queues nothing"
        );

        let newer: Vec<String> = ["D", "C", "B"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            store.enqueue_photos_new(&newer).unwrap(),
            1,
            "only the new asset"
        );
        assert_eq!(store.get_queue_stats().unwrap().pending, 4);
    }

    #[test]
    fn an_asset_recorded_as_uploaded_is_not_queued() {
        let store = DbStore::new(Database::open_in_memory().unwrap());
        store
            .inner()
            .lock()
            .unwrap()
            .record_photos_asset("A", "photo", "sha", "imm", None)
            .unwrap();
        assert_eq!(store.enqueue_photos_new(&["A".to_string()]).unwrap(), 0);
    }
}

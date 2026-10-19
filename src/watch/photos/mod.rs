// Photos library source (docs/adr/0002-photokit-library-source.md).
//
// Slice 1, the tracer (#80): when the library is enabled and access is
// granted, queue the newest few assets. A queue entry names the asset by
// `localIdentifier`; the worker exports its original when it picks the
// entry up (see `upload::worker::process_library_item`). Full and
// incremental sync (persistent change tokens) is slice 2.
//
// PhotoKit itself lives in `platform::photos`; this module only decides
// what to queue, so it is tested without a library or a TCC grant.

use tracing::{debug, info, warn};

use crate::upload::queue::QueueStore;

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

/// The tracer's temporary cap: how many of the newest assets are queued.
/// Removed in slice 2.
pub const TRACER_CAP: usize = 3;

/// Queue each asset in `newest_first` that isn't already queued or
/// recorded. `newest_first` is already capped by the caller's fetch.
/// Returns how many were queued.
pub fn enqueue_new(store: &dyn QueueStore, newest_first: &[String]) -> usize {
    let mut queued = 0;
    for local_id in newest_first {
        match store.photos_asset_seen(local_id) {
            Ok(true) => debug!(%local_id, "Photos asset already queued or uploaded"),
            Ok(false) => match store.enqueue_photos(local_id) {
                Ok(id) => {
                    queued += 1;
                    debug!(%local_id, id, "Photos asset queued");
                }
                Err(e) => warn!(%local_id, error = %e, "Failed to queue Photos asset"),
            },
            Err(e) => warn!(%local_id, error = %e, "Failed to check Photos asset"),
        }
    }
    info!(
        seen = newest_first.len(),
        queued, "Photos library: queued newest assets"
    );
    queued
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert_eq!(enqueue_new(&store, &ids), 3);
        assert_eq!(enqueue_new(&store, &ids), 0, "a second pass queues nothing");

        let newer: Vec<String> = ["D", "C", "B"].iter().map(|s| s.to_string()).collect();
        assert_eq!(enqueue_new(&store, &newer), 1, "only the new asset");
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
        assert_eq!(enqueue_new(&store, &["A".to_string()]), 0);
    }
}

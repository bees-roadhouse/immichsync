// Removable-media decisions: what to do when a volume mounts.
//
// `platform::volumes` reports mounts and unmounts; this module decides
// whether a mount becomes a watched folder. The decision is a pure function
// of the `[devices]` config and what the volume looks like, so the whole
// table is unit-tested without hardware. Starting and stopping the actual
// watcher is left to the app's folder reconcile, which already converges
// the engine on "enabled rows whose path exists": a row for `E:\DCIM` or
// `/Volumes/CARD/DCIM` starts watching when the card mounts and stops when
// it goes away. See #65 / #89.

use std::path::{Path, PathBuf};

use crate::config::DevicesConfig;
use crate::db::{Database, DbError};

/// `[devices] on_insert`, parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnInsert {
    /// Start watching without asking.
    Auto,
    /// Ask the user first. The prompt is slice 2 (#90); until then this
    /// behaves like `Ignore`, with a log line saying so.
    Ask,
    /// Never watch inserted volumes.
    Ignore,
}

impl OnInsert {
    /// Parse the config string. Unknown values are treated as `Ignore`, so a
    /// typo never starts uploading a volume the user didn't mean to.
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => OnInsert::Auto,
            "ask" => OnInsert::Ask,
            _ => OnInsert::Ignore,
        }
    }
}

/// What to do with one mounted volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountDecision {
    /// Watch this folder (`<root>/DCIM`, or the volume root).
    Watch(PathBuf),
    /// Leave the volume alone, for the reason given (logged).
    Skip(&'static str),
}

/// Decide what a newly mounted volume becomes.
///
/// - `auto_watch = false` or `on_insert = ignore` → skip.
/// - `on_insert = ask` → skip until #90 adds the prompt.
/// - `look_for_dcim = true` → watch `<root>/DCIM` if it exists, else skip.
/// - `look_for_dcim = false` → watch the whole volume, but only one the OS
///   calls removable: a fixed disk is never uploaded wholesale.
pub fn decide_mount(
    cfg: &DevicesConfig,
    root: &Path,
    has_dcim: bool,
    removable: bool,
) -> MountDecision {
    if !cfg.auto_watch {
        return MountDecision::Skip("devices.auto_watch is off");
    }
    match OnInsert::from_config(&cfg.on_insert) {
        OnInsert::Ignore => return MountDecision::Skip("devices.on_insert is ignore"),
        OnInsert::Ask => {
            return MountDecision::Skip(
                "devices.on_insert is ask; the prompt is not built yet (#90)",
            )
        }
        OnInsert::Auto => {}
    }
    if cfg.look_for_dcim {
        if has_dcim {
            MountDecision::Watch(root.join("DCIM"))
        } else {
            MountDecision::Skip("no DCIM folder at the volume root")
        }
    } else if removable {
        MountDecision::Watch(root.to_path_buf())
    } else {
        MountDecision::Skip("whole-volume watching is limited to removable media")
    }
}

/// What [`register_mount`] did to the DB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registered {
    /// A new `auto_added` row was inserted.
    Added(i64),
    /// A row for this path already exists (seen before, or added by hand);
    /// it is left as it is, including a user's choice to disable it.
    Existing(i64),
}

/// Make sure `folder` has a watched-folder row, inserting one with
/// `auto_added = true` if it doesn't.
pub fn register_mount(
    db: &Database,
    folder: &Path,
    label: Option<&str>,
) -> Result<Registered, DbError> {
    let path = folder.display().to_string();
    if let Some(existing) = db.get_folder_by_path(&path)? {
        return Ok(Registered::Existing(existing.id));
    }
    db.add_folder(&path, label, true).map(Registered::Added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(auto_watch: bool, look_for_dcim: bool, on_insert: &str) -> DevicesConfig {
        DevicesConfig {
            auto_watch,
            look_for_dcim,
            on_insert: on_insert.to_string(),
        }
    }

    fn root() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            "E:\\"
        } else {
            "/Volumes/CARD"
        })
    }

    #[test]
    fn on_insert_parses_known_values_and_rejects_the_rest() {
        assert_eq!(OnInsert::from_config("auto"), OnInsert::Auto);
        assert_eq!(OnInsert::from_config(" Ask "), OnInsert::Ask);
        assert_eq!(OnInsert::from_config("ignore"), OnInsert::Ignore);
        assert_eq!(OnInsert::from_config("yes please"), OnInsert::Ignore);
    }

    /// The whole `on_insert` × `look_for_dcim` × has-DCIM table (× removable
    /// where it matters), one row per case.
    #[test]
    fn decision_table() {
        let dcim = root().join("DCIM");
        let watch_dcim = MountDecision::Watch(dcim);
        let watch_root = MountDecision::Watch(root());
        let skip = |d: &MountDecision| matches!(d, MountDecision::Skip(_));

        #[rustfmt::skip]
        let rows: [(&str, bool, bool, bool, Option<&MountDecision>); 16] = [
            // on_insert, look_for_dcim, has_dcim, removable → expected (None = skip)
            ("auto",   true,  true,  true,  Some(&watch_dcim)),
            ("auto",   true,  true,  false, Some(&watch_dcim)),
            ("auto",   true,  false, true,  None),
            ("auto",   true,  false, false, None),
            ("auto",   false, true,  true,  Some(&watch_root)),
            ("auto",   false, false, true,  Some(&watch_root)),
            ("auto",   false, true,  false, None),
            ("auto",   false, false, false, None),
            ("ask",    true,  true,  true,  None),
            ("ask",    false, true,  true,  None),
            ("ask",    true,  false, true,  None),
            ("ask",    false, false, true,  None),
            ("ignore", true,  true,  true,  None),
            ("ignore", false, true,  true,  None),
            ("ignore", true,  false, true,  None),
            ("ignore", false, false, true,  None),
        ];

        for (on_insert, look, has_dcim, removable, want) in rows {
            let got = decide_mount(&cfg(true, look, on_insert), &root(), has_dcim, removable);
            match want {
                Some(w) => assert_eq!(
                    &got, w,
                    "{on_insert} look={look} dcim={has_dcim} rm={removable}"
                ),
                None => assert!(
                    skip(&got),
                    "{on_insert} look={look} dcim={has_dcim} rm={removable}: {got:?}"
                ),
            }
        }
    }

    #[test]
    fn auto_watch_off_skips_everything() {
        for look in [true, false] {
            let got = decide_mount(&cfg(false, look, "auto"), &root(), true, true);
            assert!(matches!(got, MountDecision::Skip(_)), "{got:?}");
        }
    }

    #[test]
    fn register_mount_inserts_once_as_auto_added() {
        let db = Database::open_in_memory().unwrap();
        let folder = root().join("DCIM");

        let first = register_mount(&db, &folder, Some("CARD")).unwrap();
        let Registered::Added(id) = first else {
            panic!("expected Added, got {first:?}")
        };
        assert_eq!(
            register_mount(&db, &folder, Some("CARD")).unwrap(),
            Registered::Existing(id)
        );

        let folders = db.get_folders().unwrap();
        assert_eq!(folders.len(), 1);
        assert!(folders[0].auto_added);
        assert!(folders[0].enabled);
        assert_eq!(folders[0].label.as_deref(), Some("CARD"));
        assert_eq!(folders[0].path, folder.display().to_string());
    }

    #[test]
    fn register_mount_leaves_a_disabled_row_disabled() {
        let db = Database::open_in_memory().unwrap();
        let folder = root().join("DCIM");
        let path = folder.display().to_string();
        let id = db.add_folder(&path, None, false).unwrap();
        let f = db.get_folder(id).unwrap().unwrap();
        db.update_folder(
            id,
            None,
            false,
            &f.watch_mode,
            f.poll_interval_secs,
            &f.album_mode,
            None,
            None,
            None,
            &f.post_upload,
            f.ignore_online_files,
        )
        .unwrap();

        assert_eq!(
            register_mount(&db, &folder, None).unwrap(),
            Registered::Existing(id)
        );
        assert!(!db.get_folder(id).unwrap().unwrap().enabled);
    }
}

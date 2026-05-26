// Log retention.
//
// `tracing_appender::rolling::daily` rotates per day but never deletes old
// files, so a long-running install accumulates log history forever. We cap
// the `logs/` directory at a fixed byte budget and delete the oldest files
// when the total exceeds the cap.
//
// Issue #49: paired with the reconcile-loop fix. A runaway loop made the
// log noise visible enough to surface the missing retention, so both ship
// in the same patch.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

/// Default cap: 1 GiB. Conservative for a tray app whose normal-day logs are
/// well under 1 MiB. If a session ever blows past this it's a signal worth
/// surfacing in itself.
pub const DEFAULT_LOG_CAP_BYTES: u64 = 1024 * 1024 * 1024;

/// Walk `dir`, sum file sizes, and delete oldest files until the total drops
/// under `cap_bytes`. Never touches today's file (matches the prefix of the
/// active rolling appender) or non-log files.
///
/// "Oldest" is the date suffix on the filename (`immichsync.log.YYYY-MM-DD`)
/// when parseable; otherwise filesystem mtime. Ties broken by filename.
///
/// Errors are logged but never propagated ... retention is best-effort. The
/// app's job is to upload photos, not to fight a permission error on log
/// cleanup at startup.
pub fn prune_logs(dir: &Path, cap_bytes: u64) {
    if !dir.exists() {
        return;
    }

    let entries = match collect_log_entries(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "Log retention: cannot read logs dir");
            return;
        }
    };

    let today_suffix = today_date_suffix();
    let total_before: u64 = entries.iter().map(|e| e.size).sum();

    if total_before <= cap_bytes {
        return;
    }

    // If today's file alone exceeds the cap, there's a runaway logger
    // somewhere. Surface it and don't delete anything ... the noise is the
    // evidence we need.
    if let Some(today) = entries.iter().find(|e| e.is_today(&today_suffix)) {
        if today.size > cap_bytes {
            warn!(
                today = %today.path.display(),
                size_mib = today.size / (1024 * 1024),
                cap_mib = cap_bytes / (1024 * 1024),
                "Log retention: today's file exceeds cap alone; not pruning (investigate the logger)"
            );
            return;
        }
    }

    // Sort oldest-first, then attempt to delete from the front while still
    // over budget. Today's file is filtered out so it can never be picked.
    let mut candidates: Vec<&LogEntry> = entries
        .iter()
        .filter(|e| !e.is_today(&today_suffix))
        .collect();
    candidates.sort_by(LogEntry::cmp_age);

    let mut total = total_before;
    let mut pruned = 0u64;
    let mut pruned_bytes = 0u64;
    for entry in candidates {
        if total <= cap_bytes {
            break;
        }
        match std::fs::remove_file(&entry.path) {
            Ok(()) => {
                total = total.saturating_sub(entry.size);
                pruned += 1;
                pruned_bytes += entry.size;
            }
            Err(e) => {
                warn!(path = %entry.path.display(), error = %e, "Log retention: delete failed");
            }
        }
    }

    if pruned > 0 {
        info!(
            pruned_files = pruned,
            pruned_mib = pruned_bytes / (1024 * 1024),
            current_total_mib = total / (1024 * 1024),
            cap_mib = cap_bytes / (1024 * 1024),
            "Log retention: pruned old log files"
        );
    }
}

/// A single log file we've collected for retention accounting.
struct LogEntry {
    path: PathBuf,
    /// Date suffix parsed from `immichsync.log.YYYY-MM-DD`, if present.
    date_suffix: Option<String>,
    mtime: std::time::SystemTime,
    size: u64,
}

impl LogEntry {
    fn is_today(&self, today_suffix: &str) -> bool {
        self.date_suffix.as_deref() == Some(today_suffix)
    }

    /// Oldest-first ordering. Prefers the parsed date suffix (lexicographic
    /// works on `YYYY-MM-DD`), falls back to mtime, then filename as a tie
    /// breaker so the test ordering is deterministic.
    fn cmp_age(a: &&Self, b: &&Self) -> Ordering {
        match (&a.date_suffix, &b.date_suffix) {
            (Some(da), Some(db)) => da.cmp(db).then_with(|| a.path.cmp(&b.path)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.mtime.cmp(&b.mtime).then_with(|| a.path.cmp(&b.path)),
        }
    }
}

fn collect_log_entries(dir: &Path) -> std::io::Result<Vec<LogEntry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        // Only consider files this app actually writes. Anything else in the
        // logs dir (tools, manual copies) is left alone.
        if !is_immichsync_log(name) {
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        let size = meta.len();
        let date_suffix = parse_date_suffix(name);

        out.push(LogEntry {
            path,
            date_suffix,
            mtime,
            size,
        });
    }
    Ok(out)
}

fn is_immichsync_log(name: &str) -> bool {
    name == "immichsync.log" || name.starts_with("immichsync.log.")
}

/// Pull `YYYY-MM-DD` off the tail of an `immichsync.log.YYYY-MM-DD` filename,
/// validating the shape so we don't accept garbage trailing extensions.
fn parse_date_suffix(name: &str) -> Option<String> {
    let suffix = name.strip_prefix("immichsync.log.")?;
    if suffix.len() != 10 {
        return None;
    }
    let bytes = suffix.as_bytes();
    let digit = |i: usize| bytes[i].is_ascii_digit();
    let dash = |i: usize| bytes[i] == b'-';
    if digit(0)
        && digit(1)
        && digit(2)
        && digit(3)
        && dash(4)
        && digit(5)
        && digit(6)
        && dash(7)
        && digit(8)
        && digit(9)
    {
        Some(suffix.to_string())
    } else {
        None
    }
}

fn today_date_suffix() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn write_file(path: &Path, bytes: usize) {
        let mut f = fs::File::create(path).expect("create");
        // Deterministic body — content doesn't matter, only size.
        let buf = vec![b'a'; bytes];
        f.write_all(&buf).expect("write");
    }

    #[test]
    fn parses_date_suffix_only_on_valid_shape() {
        assert_eq!(
            parse_date_suffix("immichsync.log.2026-05-26"),
            Some("2026-05-26".to_string())
        );
        assert_eq!(parse_date_suffix("immichsync.log"), None);
        assert_eq!(parse_date_suffix("immichsync.log.bak"), None);
        assert_eq!(parse_date_suffix("immichsync.log.2026-5-26"), None);
        assert_eq!(parse_date_suffix("other.log.2026-05-26"), None);
    }

    #[test]
    fn prune_under_cap_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_file(&dir.join("immichsync.log.2026-05-20"), 1024);
        write_file(&dir.join("immichsync.log.2026-05-21"), 1024);

        prune_logs(dir, 1024 * 1024); // 1 MiB cap, total ~2 KiB

        assert!(dir.join("immichsync.log.2026-05-20").exists());
        assert!(dir.join("immichsync.log.2026-05-21").exists());
    }

    #[test]
    fn prune_deletes_oldest_first_until_under_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // 4x 50 KiB = 200 KiB total. Cap at 100 KiB → oldest two must go.
        write_file(&dir.join("immichsync.log.2026-05-20"), 50 * 1024);
        write_file(&dir.join("immichsync.log.2026-05-21"), 50 * 1024);
        write_file(&dir.join("immichsync.log.2026-05-22"), 50 * 1024);
        write_file(&dir.join("immichsync.log.2026-05-23"), 50 * 1024);

        prune_logs(dir, 100 * 1024);

        assert!(
            !dir.join("immichsync.log.2026-05-20").exists(),
            "oldest should be pruned"
        );
        assert!(
            !dir.join("immichsync.log.2026-05-21").exists(),
            "second-oldest should be pruned"
        );
        assert!(dir.join("immichsync.log.2026-05-22").exists());
        assert!(dir.join("immichsync.log.2026-05-23").exists());
    }

    #[test]
    fn prune_never_deletes_today() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let today = today_date_suffix();
        let today_name = format!("immichsync.log.{today}");

        // 200 KiB today + 50 KiB old. Cap at 100 KiB. Today must survive.
        write_file(&dir.join(&today_name), 200 * 1024);
        write_file(&dir.join("immichsync.log.2026-05-20"), 50 * 1024);

        prune_logs(dir, 100 * 1024);

        // Today's file exceeds the cap alone → the function leaves
        // EVERYTHING in place and just warns (so the oversized file remains
        // diagnosable). This is the documented behavior.
        assert!(dir.join(&today_name).exists(), "today must survive");
        assert!(
            dir.join("immichsync.log.2026-05-20").exists(),
            "when today exceeds cap alone, prune leaves everything"
        );
    }

    #[test]
    fn prune_when_today_under_cap_but_total_over() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let today = today_date_suffix();
        let today_name = format!("immichsync.log.{today}");

        // 30 KiB today + 80 KiB old. Cap at 100 KiB. Today fits; old gets pruned.
        write_file(&dir.join(&today_name), 30 * 1024);
        write_file(&dir.join("immichsync.log.2026-05-20"), 80 * 1024);

        prune_logs(dir, 100 * 1024);

        assert!(dir.join(&today_name).exists());
        assert!(!dir.join("immichsync.log.2026-05-20").exists());
    }

    #[test]
    fn prune_ignores_non_immichsync_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        write_file(&dir.join("notes.txt"), 200 * 1024);
        write_file(&dir.join("immichsync.log.2026-05-20"), 200 * 1024);

        // Cap at 50 KiB. Should attempt to prune the old immichsync log,
        // not the unrelated text file.
        prune_logs(dir, 50 * 1024);

        assert!(dir.join("notes.txt").exists(), "non-log files untouched");
        assert!(
            !dir.join("immichsync.log.2026-05-20").exists(),
            "old log file pruned"
        );
    }
}

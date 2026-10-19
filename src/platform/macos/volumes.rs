// Volume mount/unmount events on macOS via DiskArbitration.
//
// A dedicated thread owns a `DASession` scheduled on its own CFRunLoop and
// registers three callbacks:
//
// - disk appeared: fires once per disk already present at registration and
//   then for each new one. A disk often appears before it is mounted, so
//   this alone is not "mounted".
// - description changed, watching the volume path: fires when a disk's
//   volume gets a mount point (attach, `diskutil mount`) or loses one
//   (`diskutil unmount` without ejecting).
// - disk disappeared: eject or detach.
//
// Each callback reads the disk's description and compares its volume path
// with the one last seen for that BSD name, so a mount is reported once
// however many callbacks mention it. No entitlement or TCC prompt is
// involved: DiskArbitration notifications are open to any process.
//
// Only mounts directly under `/Volumes` are reported. DiskArbitration also
// sees the boot volume, `/System/Volumes/*`, cryptexes and the simulator
// runtimes Xcode mounts under `/Library/Developer/CoreSimulator`, and the
// last two call themselves removable. A card always lands in `/Volumes`.
//
// Disk images report removable too; they are reported as not removable
// here, so a mounted `.dmg` is only watched for its DCIM folder, never as
// a whole volume.

use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::mpsc;

use anyhow::Context as _;
use objc2_core_foundation::{
    kCFRunLoopDefaultMode, CFArray, CFBoolean, CFDictionary, CFRetained, CFRunLoop, CFString,
    CFType, CFURL,
};
use objc2_disk_arbitration::{
    kDADiskDescriptionDeviceModelKey, kDADiskDescriptionMediaEjectableKey,
    kDADiskDescriptionMediaRemovableKey, kDADiskDescriptionVolumeNameKey,
    kDADiskDescriptionVolumePathKey, kDADiskDescriptionVolumeUUIDKey,
    kDADiskDescriptionWatchVolumePath, DADisk, DARegisterDiskAppearedCallback,
    DARegisterDiskDescriptionChangedCallback, DARegisterDiskDisappearedCallback, DASession,
};
use tracing::{debug, info, warn};

use crate::platform::volumes::VolumeEvent;

/// What the callbacks share: the outbound channel and the last mount point
/// seen per BSD name (`disk4s1` → `/Volumes/CARD`).
struct CallbackState {
    tx: mpsc::Sender<VolumeEvent>,
    mounted: HashMap<String, PathBuf>,
}

/// The monitor thread's run loop, handed back so `stop` can end it.
///
/// objc2 marks `CFRunLoop` not thread-safe as a whole, but `CFRunLoopStop`
/// is documented as callable from any thread, and stopping is the only
/// thing done with this handle off its own thread.
struct RunLoopHandle(CFRetained<CFRunLoop>);
unsafe impl Send for RunLoopHandle {}

/// How long one run-loop pass waits before re-checking the stop flag.
/// `CFRunLoopStop` before the thread enters the loop is a no-op, so the
/// flag is what guarantees `stop()` never hangs.
const RUN_SLICE_SECS: f64 = 0.5;

/// Watches for volume mounts and unmounts; see the module comment.
pub struct VolumeMonitor {
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
    run_loop: Option<RunLoopHandle>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VolumeMonitor {
    /// Start the monitor thread. Events for volumes already mounted are sent
    /// first, then one per mount or unmount until [`stop`](Self::stop).
    pub fn start(tx: mpsc::Sender<VolumeEvent>) -> anyhow::Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel::<anyhow::Result<RunLoopHandle>>();
        let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag = stopping.clone();

        let thread = std::thread::Builder::new()
            .name("volume-monitor".into())
            .spawn(move || run(tx, ready_tx, stop_flag))
            .context("spawn volume-monitor thread")?;

        let run_loop = ready_rx
            .recv()
            .context("volume-monitor thread exited before starting")??;

        info!("Volume monitor started (DiskArbitration)");
        Ok(Self {
            stopping,
            run_loop: Some(run_loop),
            thread: Some(thread),
        })
    }

    /// Stop the run loop and wait for the thread to exit.
    pub fn stop(&mut self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(rl) = self.run_loop.take() {
            rl.0.stop();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
            info!("Volume monitor stopped");
        }
    }
}

impl Drop for VolumeMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    tx: mpsc::Sender<VolumeEvent>,
    ready: mpsc::Sender<anyhow::Result<RunLoopHandle>>,
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let Some(session) = (unsafe { DASession::new(None) }) else {
        let _ = ready.send(Err(anyhow::anyhow!("DASessionCreate returned NULL")));
        return;
    };
    let Some(run_loop) = CFRunLoop::current() else {
        let _ = ready.send(Err(anyhow::anyhow!("no CFRunLoop for this thread")));
        return;
    };
    let Some(mode) = (unsafe { kCFRunLoopDefaultMode }) else {
        let _ = ready.send(Err(anyhow::anyhow!("kCFRunLoopDefaultMode is NULL")));
        return;
    };

    // Lives until the run loop returns; the callbacks get it as `context`.
    let state = Box::into_raw(Box::new(CallbackState {
        tx,
        mounted: HashMap::new(),
    }));
    let ctx = state.cast::<c_void>();

    unsafe {
        let watch: &CFArray = kDADiskDescriptionWatchVolumePath;
        DARegisterDiskAppearedCallback(&session, None, Some(disk_appeared), ctx);
        DARegisterDiskDescriptionChangedCallback(
            &session,
            None,
            Some(watch),
            Some(disk_changed),
            ctx,
        );
        DARegisterDiskDisappearedCallback(&session, None, Some(disk_disappeared), ctx);
        session.schedule_with_run_loop(&run_loop, mode);
    }

    let _ = ready.send(Ok(RunLoopHandle(run_loop.clone())));
    debug!("Volume monitor run loop entered");
    // Short slices rather than CFRunLoop::run(): a stop() that lands before
    // the loop is entered is still seen on the next pass.
    while !stopping.load(std::sync::atomic::Ordering::SeqCst) {
        CFRunLoop::run_in_mode(Some(mode), RUN_SLICE_SECS, false);
    }

    unsafe {
        session.unschedule_from_run_loop(&run_loop, mode);
        drop(session);
        drop(Box::from_raw(state));
    }
    debug!("Volume monitor run loop exited");
}

// ── callbacks ───────────────────────────────────────────────────────────────

unsafe extern "C-unwind" fn disk_appeared(disk: NonNull<DADisk>, ctx: *mut c_void) {
    unsafe { reconcile_disk(disk.as_ref(), ctx, false) }
}

unsafe extern "C-unwind" fn disk_changed(
    disk: NonNull<DADisk>,
    _keys: NonNull<CFArray>,
    ctx: *mut c_void,
) {
    unsafe { reconcile_disk(disk.as_ref(), ctx, false) }
}

unsafe extern "C-unwind" fn disk_disappeared(disk: NonNull<DADisk>, ctx: *mut c_void) {
    unsafe { reconcile_disk(disk.as_ref(), ctx, true) }
}

/// Compare the disk's current mount point with the last one seen and send
/// whatever changed. `gone` forces "no mount point" (disk disappeared).
unsafe fn reconcile_disk(disk: &DADisk, ctx: *mut c_void, gone: bool) {
    // SAFETY: `ctx` is the `CallbackState` box from `run`, alive for as long
    // as the session is scheduled, and only touched from this thread.
    let state = unsafe { &mut *ctx.cast::<CallbackState>() };

    let bsd_ptr = unsafe { disk.bsd_name() };
    if bsd_ptr.is_null() {
        return;
    }
    let bsd = unsafe { CStr::from_ptr(bsd_ptr) }
        .to_string_lossy()
        .into_owned();

    let desc = if gone {
        None
    } else {
        unsafe { disk.description() }
    };
    let now = desc
        .as_deref()
        .and_then(|d| unsafe { url_path(d, kDADiskDescriptionVolumePathKey) })
        .filter(|p| is_user_volume(p));
    let before = state.mounted.get(&bsd).cloned();

    if before == now {
        return;
    }

    if let Some(root) = before {
        debug!(bsd = %bsd, root = %root.display(), "Volume unmounted");
        state.mounted.remove(&bsd);
        send(state, VolumeEvent::Unmounted { root });
    }

    if let (Some(root), Some(d)) = (now, desc.as_deref()) {
        let label = unsafe { string(d, kDADiskDescriptionVolumeNameKey) };
        let id = unsafe { uuid_string(d) }.unwrap_or_else(|| bsd.clone());
        let disk_image = unsafe { string(d, kDADiskDescriptionDeviceModelKey) }
            .is_some_and(|p| p == "Disk Image");
        let removable = !disk_image
            && unsafe {
                flag(d, kDADiskDescriptionMediaRemovableKey)
                    || flag(d, kDADiskDescriptionMediaEjectableKey)
            };
        debug!(bsd = %bsd, root = %root.display(), removable, "Volume mounted");
        state.mounted.insert(bsd, root.clone());
        send(
            state,
            VolumeEvent::Mounted {
                root,
                label,
                id,
                removable,
            },
        );
    }
}

/// A mount point a user-facing volume would use: `/Volumes/<name>`.
fn is_user_volume(path: &Path) -> bool {
    path.parent() == Some(Path::new("/Volumes"))
}

fn send(state: &CallbackState, event: VolumeEvent) {
    if state.tx.send(event).is_err() {
        warn!("Volume event dropped: receiver is gone");
    }
}

// ── description dictionary helpers ──────────────────────────────────────────

/// Look up `key` in a disk description and return it as a `CFType`.
unsafe fn value<'a>(desc: &'a CFDictionary, key: &CFString) -> Option<&'a CFType> {
    let key: *const CFString = key;
    let raw = unsafe { desc.value(key.cast()) };
    // SAFETY: description values are CF objects owned by `desc`.
    unsafe { raw.cast::<CFType>().as_ref() }
}

unsafe fn url_path(desc: &CFDictionary, key: &CFString) -> Option<PathBuf> {
    unsafe { value(desc, key) }?
        .downcast_ref::<CFURL>()?
        .to_file_path()
}

unsafe fn string(desc: &CFDictionary, key: &CFString) -> Option<String> {
    let s = unsafe { value(desc, key) }?
        .downcast_ref::<CFString>()?
        .to_string();
    (!s.is_empty()).then_some(s)
}

unsafe fn flag(desc: &CFDictionary, key: &CFString) -> bool {
    unsafe { value(desc, key) }
        .and_then(|v| v.downcast_ref::<CFBoolean>())
        .is_some_and(|b| b.as_bool())
}

/// The volume UUID, which DiskArbitration stores as a `CFUUID`.
unsafe fn uuid_string(desc: &CFDictionary) -> Option<String> {
    let v = unsafe { value(desc, kDADiskDescriptionVolumeUUIDKey) }?;
    let uuid = v.downcast_ref::<objc2_core_foundation::CFUUID>()?;
    objc2_core_foundation::CFUUID::new_string(None, Some(uuid)).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    /// Attach a throwaway disk image with a DCIM folder and expect a Mounted
    /// event for it, then detach and expect Unmounted. Real DiskArbitration,
    /// no hardware, no prompts. Ignored by default because it mounts a
    /// volume on the machine running it:
    /// `cargo test hdiutil_round_trip -- --ignored`.
    #[test]
    fn only_mounts_directly_under_volumes_are_user_volumes() {
        assert!(is_user_volume(Path::new("/Volumes/CARD")));
        assert!(is_user_volume(Path::new("/Volumes/NO NAME")));
        assert!(!is_user_volume(Path::new("/")));
        assert!(!is_user_volume(Path::new("/Volumes")));
        assert!(!is_user_volume(Path::new("/System/Volumes/Data")));
        assert!(!is_user_volume(Path::new("/Volumes/CARD/DCIM")));
        assert!(!is_user_volume(Path::new(
            "/Library/Developer/CoreSimulator/Volumes/iOS_23D8133"
        )));
    }

    #[test]
    #[ignore]
    fn hdiutil_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("DCIM/100TEST")).unwrap();
        let img = dir.path().join("card.dmg");
        let label = format!("ISYNC{}", std::process::id());
        let ok = Command::new("hdiutil")
            .args([
                "create", "-quiet", "-size", "20m", "-fs", "ExFAT", "-volname", &label,
            ])
            .arg("-srcfolder")
            .arg(&src)
            .arg(&img)
            .status()
            .unwrap()
            .success();
        assert!(ok, "hdiutil create failed");

        // Stopping right after start must not hang (the run loop may not
        // have been entered yet).
        let (tx0, _rx0) = mpsc::channel();
        let t0 = std::time::Instant::now();
        VolumeMonitor::start(tx0).unwrap().stop();
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "stop() right after start hung"
        );

        let (tx, rx) = mpsc::channel();
        let mut monitor = VolumeMonitor::start(tx).unwrap();
        // Drain the events for volumes that were already mounted.
        std::thread::sleep(Duration::from_millis(500));
        while rx.try_recv().is_ok() {}

        let ok = Command::new("hdiutil")
            .args(["attach", "-nobrowse", "-quiet"])
            .arg(&img)
            .status()
            .unwrap()
            .success();
        assert!(ok, "hdiutil attach failed");

        let want_root = PathBuf::from("/Volumes").join(&label);
        let mounted = wait_for(
            &rx,
            |e| matches!(e, VolumeEvent::Mounted { root, .. } if *root == want_root),
        );
        let saw_dcim = crate::platform::volumes::has_dcim(&want_root);
        let detach = Command::new("hdiutil")
            .arg("detach")
            .arg(&want_root)
            .status();

        let mounted = mounted.expect("no Mounted event for the image");
        let VolumeEvent::Mounted {
            label: got_label,
            removable,
            ..
        } = &mounted
        else {
            unreachable!()
        };
        assert_eq!(got_label.as_deref(), Some(label.as_str()));
        assert!(saw_dcim, "DCIM not visible at {}", want_root.display());
        assert!(!removable, "a disk image must not count as removable");

        assert!(detach.unwrap().success(), "hdiutil detach failed");
        let unmounted = wait_for(
            &rx,
            |e| matches!(e, VolumeEvent::Unmounted { root } if *root == want_root),
        );
        assert!(unmounted.is_some(), "no Unmounted event for the image");
        monitor.stop();
    }

    fn wait_for(
        rx: &mpsc::Receiver<VolumeEvent>,
        want: impl Fn(&VolumeEvent) -> bool,
    ) -> Option<VolumeEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(e) if want(&e) => return Some(e),
                Ok(e) => eprintln!("other event: {e:?}"),
                Err(_) => return None,
            }
        }
        None
    }
}

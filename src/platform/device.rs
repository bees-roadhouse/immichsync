// Volume mount/unmount events on Windows (WM_DEVICECHANGE).
//
// Revived from `watch/device.rs`, deleted as dead code in #42 because it was
// never wired in, and put behind `platform::volumes` (#89). Two changes from
// the original:
//
// - The window is a hidden *top-level* window, not a message-only one.
//   `DBT_DEVICEARRIVAL` / `DBT_DEVICEREMOVECOMPLETE` for volumes are
//   broadcast, and message-only windows (`HWND_MESSAGE`) never receive
//   broadcasts, so the original could not have seen a card arrive.
// - Events are `VolumeEvent`s on a std channel (the app's main loop polls),
//   and drives already present at start are sent first, so a card inserted
//   before ImmichSync launched is picked up too.

use std::sync::mpsc;

use anyhow::Context as _;
use tracing::{debug, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW,
    RegisterClassW, TranslateMessage, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_QUIT,
    WNDCLASSW,
};

use super::drives::{
    drive_info, drive_letters_from_mask, drive_root, list_drives, DriveInfo, DriveType,
};
use super::volumes::VolumeEvent;

// Win32 constants not in the windows crate's typed wrappers.
const WM_DEVICECHANGE: u32 = 0x0219;
const DBT_DEVICEARRIVAL: u32 = 0x8000;
const DBT_DEVICEREMOVECOMPLETE: u32 = 0x8004;
const DBT_DEVTYP_VOLUME: u32 = 0x0000_0002;
/// `dbcv_flags`: the volume is a network drive. Network shares are watched
/// as explicit folders, never auto-added.
const DBTF_NET: u16 = 0x0002;

/// DEV_BROADCAST_HDR layout (first 12 bytes of the LPARAM structure).
#[repr(C)]
#[allow(non_snake_case)]
struct DevBroadcastHdr {
    dbch_size: u32,
    dbch_devicetype: u32,
    dbch_reserved: u32,
}

/// DEV_BROADCAST_VOLUME layout for DBT_DEVTYP_VOLUME notifications.
#[repr(C)]
#[allow(non_snake_case)]
struct DevBroadcastVolume {
    dbcv_size: u32,
    dbcv_devicetype: u32,
    dbcv_reserved: u32,
    dbcv_unitmask: u32,
    dbcv_flags: u16,
}

// HWND wraps a raw pointer which isn't Send by default, but PostMessageW is
// safe to call from any thread.
struct SendHwnd(HWND);
unsafe impl Send for SendHwnd {}

/// Watches for volume arrival and removal; see the module comment.
pub struct VolumeMonitor {
    hwnd: Option<SendHwnd>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VolumeMonitor {
    /// Start the monitor thread. Events for drives already present are sent
    /// first, then one per arrival or removal until [`stop`](Self::stop).
    pub fn start(tx: mpsc::Sender<VolumeEvent>) -> anyhow::Result<Self> {
        for drive in list_drives() {
            if drive.drive_type != DriveType::Network {
                let _ = tx.send(mounted(&drive));
            }
        }

        let (ready_tx, ready_rx) = mpsc::channel::<anyhow::Result<SendHwnd>>();
        let thread = std::thread::Builder::new()
            .name("volume-monitor".into())
            .spawn(move || run(tx, ready_tx))
            .context("spawn volume-monitor thread")?;
        let hwnd = ready_rx
            .recv()
            .context("volume-monitor thread exited before starting")??;

        info!("Volume monitor started (WM_DEVICECHANGE)");
        Ok(Self {
            hwnd: Some(hwnd),
            thread: Some(thread),
        })
    }

    /// Post WM_QUIT to the monitor's message loop and wait for it to exit.
    pub fn stop(&mut self) {
        if let Some(hwnd) = self.hwnd.take() {
            unsafe {
                let _ = PostMessageW(hwnd.0, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
            info!("Volume monitor stopped");
        }
    }
}

impl Drop for VolumeMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn mounted(drive: &DriveInfo) -> VolumeEvent {
    VolumeEvent::Mounted {
        root: drive_root(drive.letter),
        label: drive.label.clone(),
        id: match drive.serial {
            Some(s) => format!("{:04X}-{:04X}", s >> 16, s & 0xFFFF),
            None => format!("{}:", drive.letter),
        },
        removable: drive.drive_type == DriveType::Removable,
    }
}

// ── Message loop thread ─────────────────────────────────────────────────────

thread_local! {
    static VOLUME_EVENT_TX: std::cell::RefCell<Option<mpsc::Sender<VolumeEvent>>> =
        const { std::cell::RefCell::new(None) };
}

fn run(tx: mpsc::Sender<VolumeEvent>, ready: mpsc::Sender<anyhow::Result<SendHwnd>>) {
    // The window proc runs on this thread; it reaches the sender here.
    VOLUME_EVENT_TX.with(|cell| *cell.borrow_mut() = Some(tx));

    let class_name_wide: Vec<u16> = "ImmichSyncDevMon\0".encode_utf16().collect();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(device_wnd_proc),
        lpszClassName: PCWSTR(class_name_wide.as_ptr()),
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&wc);
    }

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_name_wide.as_ptr()),
            PCWSTR::null(),
            // WS_OVERLAPPED with no WS_VISIBLE and no parent: top-level and
            // never shown, so it receives the volume broadcasts.
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(anyhow::anyhow!("CreateWindowExW failed: {e}")));
            return;
        }
    };
    let _ = ready.send(Ok(SendHwnd(hwnd)));
    debug!("Volume monitor window created");

    unsafe {
        let mut msg = MSG::default();
        // GetMessageW returns -1 on error; `> 0` stops there instead of
        // spinning on it as `.into()` (non-zero = true) would.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DestroyWindow(hwnd);
    }

    VOLUME_EVENT_TX.with(|cell| *cell.borrow_mut() = None);
    debug!("Volume monitor message loop exited");
}

unsafe extern "system" fn device_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Task Manager's "End task" sends WM_CLOSE to every top-level window;
    // the shutdown window owns that, so this one just ignores it.
    if msg == WM_CLOSE {
        return LRESULT(0);
    }

    if msg == WM_DEVICECHANGE {
        let event_type = wparam.0 as u32;
        if (event_type == DBT_DEVICEARRIVAL || event_type == DBT_DEVICEREMOVECOMPLETE)
            && lparam.0 != 0
        {
            let hdr = unsafe { &*(lparam.0 as *const DevBroadcastHdr) };
            if hdr.dbch_devicetype == DBT_DEVTYP_VOLUME {
                let vol = unsafe { &*(lparam.0 as *const DevBroadcastVolume) };
                if vol.dbcv_flags & DBTF_NET == 0 {
                    for letter in drive_letters_from_mask(vol.dbcv_unitmask) {
                        let event = if event_type == DBT_DEVICEARRIVAL {
                            debug!(drive = %letter, "Volume arrived");
                            mounted(&drive_info(letter))
                        } else {
                            debug!(drive = %letter, "Volume removed");
                            VolumeEvent::Unmounted {
                                root: drive_root(letter),
                            }
                        };
                        VOLUME_EVENT_TX.with(|cell| {
                            if let Some(tx) = cell.borrow().as_ref() {
                                if tx.send(event).is_err() {
                                    warn!("Volume event dropped: receiver is gone");
                                }
                            }
                        });
                    }
                }
            }
        }
    }

    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

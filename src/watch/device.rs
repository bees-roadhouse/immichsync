// Removable device monitor (WM_DEVICECHANGE)
//
// Creates a hidden message-only Win32 window on a dedicated thread to receive
// WM_DEVICECHANGE messages. Extracts drive letters from DBT_DEVICEARRIVAL /
// DBT_DEVICEREMOVECOMPLETE and forwards typed DeviceEvent values through
// a tokio channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetMessageW, PostMessageW, RegisterClassW,
    HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_QUIT, WNDCLASSW,
};

/// Events emitted by the device monitor.
#[derive(Debug, Clone)]
pub enum DeviceEvent {
    /// A removable device was plugged in.
    DeviceArrived {
        /// The drive letter assigned to the new volume (e.g. `'E'`).
        drive_letter: char,
        /// Whether the drive root contains a `DCIM` folder, which indicates
        /// a camera / phone storage device.
        has_dcim: bool,
    },
    /// A removable device was safely removed or ejected.
    DeviceRemoved {
        /// The drive letter that was in use before removal.
        drive_letter: char,
    },
}

// Win32 constants not in the windows crate's typed wrappers.
const WM_DEVICECHANGE: u32 = 0x0219;
const DBT_DEVICEARRIVAL: u32 = 0x8000;
const DBT_DEVICEREMOVECOMPLETE: u32 = 0x8004;
const DBT_DEVTYP_VOLUME: u32 = 0x00000002;

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

// HWND wraps a raw pointer which isn't Send/Sync by default. But HWNDs are
// safe to use from any thread for PostMessageW (which is thread-safe).
struct SendHwnd(HWND);
unsafe impl Send for SendHwnd {}
unsafe impl Sync for SendHwnd {}

/// Monitors the system for removable device arrival and removal.
///
/// On Windows this is driven by `WM_DEVICECHANGE` messages posted to a
/// hidden message-only window.  The monitor converts those raw messages into
/// typed [`DeviceEvent`] values and forwards them on an async channel.
pub struct DeviceMonitor {
    event_tx: mpsc::Sender<DeviceEvent>,
    running: Arc<AtomicBool>,
    hwnd: Arc<std::sync::Mutex<Option<SendHwnd>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DeviceMonitor {
    /// Create a new `DeviceMonitor`.
    ///
    /// Events will be sent on `event_tx`.  The corresponding receiver should
    /// be held by the component that acts on device arrivals (typically the
    /// watch engine orchestrator).
    pub fn new(event_tx: mpsc::Sender<DeviceEvent>) -> Self {
        Self {
            event_tx,
            running: Arc::new(AtomicBool::new(false)),
            hwnd: Arc::new(std::sync::Mutex::new(None)),
            thread: None,
        }
    }

    /// Start the device monitor.
    ///
    /// Spawns a dedicated thread that creates a hidden message-only window
    /// and pumps WM_DEVICECHANGE messages.
    pub fn start(&mut self) -> anyhow::Result<()> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        let event_tx = self.event_tx.clone();
        let running = self.running.clone();
        let hwnd_holder = self.hwnd.clone();

        running.store(true, Ordering::SeqCst);

        let thread = std::thread::spawn(move || {
            if let Err(e) = run_device_monitor_loop(event_tx, running.clone(), hwnd_holder) {
                warn!(error = %e, "Device monitor thread failed");
            }
            running.store(false, Ordering::SeqCst);
        });

        self.thread = Some(thread);
        info!("Device monitor started");
        Ok(())
    }

    /// Stop the device monitor.
    ///
    /// Posts WM_QUIT to the monitor thread's message loop and waits for the
    /// thread to exit cleanly.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);

        // Post WM_QUIT to the hidden window to break the message loop.
        if let Some(ref send_hwnd) = *self.hwnd.lock().unwrap() {
            unsafe {
                let _ = PostMessageW(send_hwnd.0, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }

        info!("Device monitor stopped");
    }
}

// ── Message loop thread ─────────────────────────────────────────────────────

fn run_device_monitor_loop(
    event_tx: mpsc::Sender<DeviceEvent>,
    _running: Arc<AtomicBool>,
    hwnd_holder: Arc<std::sync::Mutex<Option<SendHwnd>>>,
) -> anyhow::Result<()> {
    // Store the event_tx in a thread-local so the window proc can access it.
    DEVICE_EVENT_TX.with(|cell| {
        *cell.borrow_mut() = Some(event_tx);
    });

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
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            HWND_MESSAGE, // message-only window
            None,
            None,
            None,
        )?
    };

    // Store HWND so stop() can post WM_QUIT.
    *hwnd_holder.lock().unwrap() = Some(SendHwnd(hwnd));

    debug!("Device monitor message-only window created");

    // Message pump.
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            // Messages are dispatched to device_wnd_proc automatically.
        }

        let _ = DestroyWindow(hwnd);
    }

    *hwnd_holder.lock().unwrap() = None;

    // Clean up thread-local.
    DEVICE_EVENT_TX.with(|cell| {
        *cell.borrow_mut() = None;
    });

    debug!("Device monitor message loop exited");
    Ok(())
}

thread_local! {
    static DEVICE_EVENT_TX: std::cell::RefCell<Option<mpsc::Sender<DeviceEvent>>> =
        const { std::cell::RefCell::new(None) };
}

unsafe extern "system" fn device_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_DEVICECHANGE {
        let event_type = wparam.0 as u32;

        if (event_type == DBT_DEVICEARRIVAL || event_type == DBT_DEVICEREMOVECOMPLETE)
            && lparam.0 != 0
        {
            let hdr = &*(lparam.0 as *const DevBroadcastHdr);

            if hdr.dbch_devicetype == DBT_DEVTYP_VOLUME {
                let vol = &*(lparam.0 as *const DevBroadcastVolume);
                let mask = vol.dbcv_unitmask;

                // Extract drive letter(s) from the bitmask.
                for bit in 0u32..26 {
                    if mask & (1 << bit) == 0 {
                        continue;
                    }

                    let letter = char::from(b'A' + bit as u8);

                    let event = if event_type == DBT_DEVICEARRIVAL {
                        let has_dcim = crate::platform::has_dcim_folder(letter);
                        debug!(drive = %letter, has_dcim, "Device arrived");
                        DeviceEvent::DeviceArrived {
                            drive_letter: letter,
                            has_dcim,
                        }
                    } else {
                        debug!(drive = %letter, "Device removed");
                        DeviceEvent::DeviceRemoved {
                            drive_letter: letter,
                        }
                    };

                    DEVICE_EVENT_TX.with(|cell| {
                        if let Some(tx) = cell.borrow().as_ref() {
                            let _ = tx.try_send(event);
                        }
                    });
                }
            }
        }
    }

    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Extract drive letter(s) from a unit mask bitmask.
/// Bit 0 = A, bit 1 = B, ... bit 25 = Z.
#[allow(dead_code)]
fn drive_letters_from_mask(mask: u32) -> Vec<char> {
    (0u32..26)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| char::from(b'A' + bit as u8))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drive_letters_from_mask() {
        // Bit 2 = C, Bit 4 = E
        let letters = drive_letters_from_mask(0b10100);
        assert_eq!(letters, vec!['C', 'E']);
    }

    #[test]
    fn test_drive_letters_from_mask_empty() {
        let letters = drive_letters_from_mask(0);
        assert!(letters.is_empty());
    }

    #[test]
    fn test_device_monitor_create() {
        let (tx, _rx) = mpsc::channel(8);
        let monitor = DeviceMonitor::new(tx);
        assert!(!monitor.running.load(Ordering::SeqCst));
    }
}

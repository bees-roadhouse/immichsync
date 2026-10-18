// Main-thread event pump (Windows).
//
// tray-icon creates a hidden window whose context menu only works while
// its messages are dispatched. `App::run` calls `pump()` every tick.

use tracing::info;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

/// Prepare the main thread before the tray icon is created. Nothing to do on
/// Windows: tray-icon's hidden window needs no app object.
pub fn init() {}

/// Dispatch every pending Win32 message without blocking.
///
/// Returns `true` once a `WM_QUIT` is seen (posted by the shutdown window on
/// Task Manager "End task" or logoff), meaning the app should shut down.
pub fn pump() -> bool {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).into() {
            if msg.message == WM_QUIT {
                info!("WM_QUIT received");
                return true;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    false
}

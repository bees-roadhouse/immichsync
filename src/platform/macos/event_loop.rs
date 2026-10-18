// Main-thread event pump (macOS).
//
// The app is a menu-bar extra: an `NSApplication` with the Accessory
// activation policy (no Dock icon, no app menu). Instead of handing the
// thread to `[NSApp run]`, `App::run` keeps its poll-everything loop and
// calls `pump()` each tick, which drains pending AppKit events the same
// way the Windows build drains Win32 messages.

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode};

/// Create the shared `NSApplication` and finish launching it. Must run on
/// the main thread before the tray icon (an `NSStatusItem`) is created.
pub fn init() {
    let mtm = MainThreadMarker::new().expect("event_loop::init must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
}

/// Dispatch every pending AppKit event without blocking.
///
/// Returns `true` once SIGTERM/SIGINT has been received (see `shutdown`),
/// meaning the app should shut down.
pub fn pump() -> bool {
    let mtm = MainThreadMarker::new().expect("event_loop::pump must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let past = NSDate::distantPast();
    // SAFETY: an immutable Foundation constant.
    let mode = unsafe { NSDefaultRunLoopMode };
    loop {
        let event = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&past),
            mode,
            true,
        );
        match event {
            Some(event) => app.sendEvent(&event),
            None => break,
        }
    }
    app.updateWindows();
    super::shutdown::quit_requested()
}

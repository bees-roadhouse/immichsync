// Notification Center (macOS) via UserNotifications.
//
// `UNUserNotificationCenter` only works from a process with a bundle
// identifier (`ImmichSync.app`); in a bare dev binary it throws an
// Objective-C exception. Without a bundle every call here therefore logs and
// returns, so `cargo run` keeps working.
//
// macOS asks the user once for permission. A denial is remembered by the
// system: we log it and later notifications are silently dropped by
// Notification Center, never retried by us.

use std::sync::Once;

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSBundle, NSError, NSString, NSUUID};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
    UNUserNotificationCenter,
};
use tracing::{debug, info, warn};

static REQUEST_PERMISSION: Once = Once::new();

/// True when the running process has a bundle identifier.
pub fn available() -> bool {
    NSBundle::mainBundle().bundleIdentifier().is_some()
}

/// Ask for alert permission once per process (the system shows its prompt
/// only the first time ever). Safe to call repeatedly.
pub fn request_permission() {
    if !available() {
        return;
    }
    REQUEST_PERMISSION.call_once(|| {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let handler = RcBlock::new(|granted: Bool, error: *mut NSError| {
            if granted.as_bool() {
                info!("Notification permission granted");
            } else if let Some(err) = unsafe { error.as_ref() } {
                warn!(error = %err.localizedDescription(), "Notification permission request failed");
            } else {
                info!("Notification permission denied; notifications will not be shown");
            }
        });
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &handler,
        );
    });
}

/// Post a notification now. Returns false when there is no bundle.
pub fn show(title: &str, body: &str) -> bool {
    if !available() {
        debug!(title, body, "No app bundle; notification logged only");
        return false;
    }
    request_permission();

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    let id = NSUUID::UUID().UUIDString();
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);

    let done = RcBlock::new(|error: *mut NSError| {
        if let Some(err) = unsafe { error.as_ref() } {
            warn!(error = %err.localizedDescription(), "Failed to post notification");
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, Some(&done));
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_binary_has_no_bundle_so_show_is_a_no_op() {
        // cargo's test binary isn't bundled; this must not throw.
        assert!(!super::available());
        assert!(!super::show("ImmichSync test", "not shown"));
    }
}

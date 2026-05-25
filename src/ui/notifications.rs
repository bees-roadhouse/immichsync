// Windows toast notifications via winrt-notification.

use tracing::{debug, warn};

use crate::config::Config;

/// Notification manager. Respects the `config.ui.show_notifications` toggle.
pub struct Notifications {
    enabled: bool,
    notify_on_complete: bool,
}

impl Notifications {
    /// Create a new notification manager from the current config.
    pub fn new(config: &Config) -> Self {
        Self {
            enabled: config.ui.show_notifications,
            notify_on_complete: config.ui.notification_on_complete,
        }
    }

    /// Show a notification when a batch upload completes.
    pub fn notify_upload_complete(&self, count: u32) {
        if !self.enabled || !self.notify_on_complete || count == 0 {
            return;
        }

        let body = if count == 1 {
            "1 file uploaded successfully.".to_string()
        } else {
            format!("{count} files uploaded successfully.")
        };

        self.show_toast("Upload Complete", &body);
    }

    /// Show an error notification.
    pub fn notify_error(&self, msg: &str) {
        if !self.enabled {
            return;
        }
        self.show_toast("ImmichSync Error", msg);
    }

    /// Show a toast notification with a custom title and body.
    ///
    /// Respects the global notifications toggle.
    pub fn show_toast_raw(&self, title: &str, body: &str) {
        if !self.enabled {
            return;
        }
        self.show_toast(title, body);
    }

    fn show_toast(&self, title: &str, body: &str) {
        debug!(title, body, "Showing toast notification");

        if let Err(e) = winrt_notification::Toast::new(crate::platform::APP_USER_MODEL_ID)
            .title(title)
            .text1(body)
            .show()
        {
            warn!(error = %e, "Failed to show toast notification");
        }
    }
}

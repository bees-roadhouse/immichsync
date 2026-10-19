// "Sign in with <server's OAuth>" for the first-run wizard and Settings (#143).
//
// The widget probes the server for OAuth once the URL has settled, asks the
// user before anything is created, then runs api::oauth on a worker thread
// (browser sign-in, session, per-device API key, session logout) and hands
// the new key back to the caller, which stores it like a pasted one: the
// Keychain on macOS, DPAPI on Windows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::api::oauth::{self, OAuthError};

/// How long the URL must stay unchanged before the server is probed.
const PROBE_SETTLE: Duration = Duration::from_millis(700);

/// Where the key ends up, in the OS's own words.
const KEY_STORE: &str = if cfg!(target_os = "macos") {
    "your login Keychain"
} else {
    "Windows' protected storage for your account (DPAPI)"
};

enum Probe {
    Idle,
    Running(mpsc::Receiver<Option<String>>),
    Done(Option<String>),
}

enum Flow {
    Idle,
    Running {
        cancel: Arc<AtomicBool>,
        rx: mpsc::Receiver<Result<String, OAuthError>>,
    },
}

pub struct SignIn {
    probed_url: String,
    url_changed_at: Instant,
    last_url: String,
    probe: Probe,
    flow: Flow,
    /// Last outcome shown under the button.
    pub status: String,
}

impl Default for SignIn {
    fn default() -> Self {
        Self {
            probed_url: String::new(),
            url_changed_at: Instant::now(),
            last_url: String::new(),
            probe: Probe::Idle,
            flow: Flow::Idle,
            status: String::new(),
        }
    }
}

fn looks_like_url(s: &str) -> bool {
    let s = s.trim();
    (s.starts_with("https://") || s.starts_with("http://")) && s.len() > "https://x".len()
}

impl SignIn {
    /// Draw the sign-in row for `server_url`. Returns a new API key once a
    /// sign-in completes (the caller puts it in its key field).
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        server_url: &str,
    ) -> Option<String> {
        self.poll_probe(ctx, server_url);
        let key = self.poll_flow();

        match (&self.flow, &self.probe) {
            (Flow::Running { cancel, .. }, _) => {
                let cancel = cancel.clone();
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Waiting for you to sign in in your browser…");
                    if ui.button("Cancel").clicked() {
                        cancel.store(true, Ordering::SeqCst);
                    }
                });
                ctx.request_repaint_after(Duration::from_millis(200));
            }
            (Flow::Idle, Probe::Done(Some(button))) => {
                let button = button.clone();
                ui.horizontal(|ui| {
                    if ui.button(&button).clicked() {
                        self.start(server_url);
                    }
                    ui.label("or paste an API key.");
                });
            }
            _ => {}
        }
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        key
    }

    fn poll_probe(&mut self, ctx: &egui::Context, server_url: &str) {
        let url = server_url.trim().to_string();
        if url != self.last_url {
            self.last_url = url.clone();
            self.url_changed_at = Instant::now();
        }
        if let Probe::Running(rx) = &self.probe {
            match rx.try_recv() {
                Ok(button) => self.probe = Probe::Done(button),
                Err(mpsc::TryRecvError::Disconnected) => self.probe = Probe::Done(None),
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(200));
                }
            }
            return;
        }
        if url == self.probed_url || !looks_like_url(&url) {
            return;
        }
        if self.url_changed_at.elapsed() < PROBE_SETTLE {
            ctx.request_repaint_after(PROBE_SETTLE);
            return;
        }
        self.probed_url = url.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let button = oauth::oauth_button_text(&url).unwrap_or_else(|e| {
                tracing::debug!(error = %e, "OAuth probe failed; no sign-in button");
                None
            });
            let _ = tx.send(button);
        });
        self.probe = Probe::Running(rx);
    }

    fn poll_flow(&mut self) -> Option<String> {
        let Flow::Running { rx, .. } = &self.flow else {
            return None;
        };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err(OAuthError::Server(
                "the sign-in stopped unexpectedly".into(),
            )),
        };
        self.flow = Flow::Idle;
        match result {
            Ok(key) => {
                self.status = format!(
                    "Signed in. \"{}\" was created and goes into {KEY_STORE} when you save.",
                    oauth::key_name()
                );
                Some(key)
            }
            Err(e) => {
                tracing::warn!(error = %e, "OAuth sign-in failed");
                self.status = e.to_string();
                None
            }
        }
    }

    fn start(&mut self, server_url: &str) {
        let host = server_url
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        let yes = rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Info)
            .set_title("Sign in to Immich")
            .set_description(format!(
                "ImmichSync will open your browser to sign in to {host}.\n\n\
                 Afterwards it creates a long-lived API key named \"{}\", allowed only to \
                 upload photos and add them to albums, and keeps it in {KEY_STORE}. The \
                 browser session is then signed out.\n\n\
                 You can revoke the key any time in Immich: Account Settings → API Keys.\n\n\
                 Continue?",
                oauth::key_name()
            ))
            .set_buttons(rfd::MessageButtons::YesNo)
            .show();
        if yes != rfd::MessageDialogResult::Yes {
            return;
        }
        self.status.clear();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let url = server_url.trim().to_string();
        let thread_cancel = cancel.clone();
        std::thread::spawn(move || {
            let r = oauth::sign_in_and_create_key(
                &url,
                |idp| open::that(idp),
                &thread_cancel,
                oauth::SIGN_IN_TIMEOUT,
            );
            let _ = tx.send(r);
        });
        self.flow = Flow::Running { cancel, rx };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_urls_are_probed() {
        assert!(looks_like_url("https://pics.beesroadhouse.com"));
        assert!(looks_like_url("http://10.2.4.5:2283"));
        assert!(!looks_like_url("pics.beesroadhouse.com"));
        assert!(!looks_like_url("https://"));
        assert!(!looks_like_url(""));
    }
}

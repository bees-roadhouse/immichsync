// System tray icon and context menu via the `tray-icon` crate (0.19).
//
// The tray icon must be created on the main thread after the winit event loop
// has been set up (or on Windows it can be created before the loop starts,
// but must be driven from the main thread).
//
// Icon generation: since we ship no .ico files yet, each state is represented
// by a hand-generated 16×16 RGBA bitmap.  The generator fills a solid-colour
// circle on a transparent background — cheap, dependency-free, and readable.

use std::sync::mpsc;
use thiserror::Error;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIcon, TrayIconAttributes,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The logical state of the tray icon.
#[derive(Debug, Clone, PartialEq)]
pub enum TrayState {
    /// Connected and watching; nothing in queue.
    Idle,
    /// Actively uploading files.
    Syncing { current: u32, total: u32 },
    /// User has paused syncing.
    Paused,
    /// An error has occurred (server unreachable, auth failure, etc.).
    Error(String),
    /// No network / server not reachable.
    Offline,
}

/// User actions emitted by the tray menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    Pause,
    Resume,
    UploadNow,
    OpenSettings,
    About,
    ViewLog,
    ViewTrash,
    CheckForUpdates,
    OpenUpdateDialog,
    RestartToUpdate,
    Quit,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum TrayError {
    #[error("failed to create tray icon: {0}")]
    IconCreate(String),

    #[error("failed to build tray icon: {0}")]
    Build(String),
}

// ---------------------------------------------------------------------------
// Internal menu item IDs
// ---------------------------------------------------------------------------

const ID_PAUSE: &str = "pause";
const ID_RESUME: &str = "resume";
const ID_UPLOAD_NOW: &str = "upload_now";
const ID_SETTINGS: &str = "settings";
const ID_ABOUT: &str = "about";
const ID_VIEW_LOG: &str = "view_log";
const ID_VIEW_TRASH: &str = "view_trash";
const ID_CHECK_UPDATES: &str = "check_updates";
const ID_UPDATE_AVAILABLE: &str = "update_available";
const ID_RESTART_TO_UPDATE: &str = "restart_to_update";
const ID_QUIT: &str = "quit";

// ---------------------------------------------------------------------------
// Icon generation
// ---------------------------------------------------------------------------

/// Generate a 16×16 RGBA image with a filled circle of the given colour on a
/// transparent background.  Alpha is 255 for pixels inside the circle.
fn generate_icon_rgba(r: u8, g: u8, b: u8) -> Vec<u8> {
    const SIZE: usize = 16;
    const CENTER: f32 = (SIZE as f32 - 1.0) / 2.0;
    const RADIUS: f32 = (SIZE as f32 / 2.0) - 1.0;

    let mut rgba = vec![0u8; SIZE * SIZE * 4];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - CENTER;
            let dy = y as f32 - CENTER;
            let dist = (dx * dx + dy * dy).sqrt();

            let idx = (y * SIZE + x) * 4;
            if dist <= RADIUS {
                rgba[idx] = r;
                rgba[idx + 1] = g;
                rgba[idx + 2] = b;
                rgba[idx + 3] = 255;
            }
            // else: transparent (already zeroed)
        }
    }

    rgba
}

fn make_icon(r: u8, g: u8, b: u8) -> Result<Icon, TrayError> {
    let rgba = generate_icon_rgba(r, g, b);
    Icon::from_rgba(rgba, 16, 16).map_err(|e| TrayError::IconCreate(e.to_string()))
}

// Pre-generated icon colours:
//   Idle     — green  (0x4C, 0xAF, 0x50)
//   Syncing  — blue   (0x21, 0x96, 0xF3)
//   Paused   — yellow (0xFF, 0xC1, 0x07)
//   Error    — red    (0xF4, 0x43, 0x36)
//   Offline  — gray   (0x9E, 0x9E, 0x9E)

fn icon_for_state(state: &TrayState) -> Result<Icon, TrayError> {
    match state {
        TrayState::Idle => make_icon(0x4C, 0xAF, 0x50),
        TrayState::Syncing { .. } => make_icon(0x21, 0x96, 0xF3),
        TrayState::Paused => make_icon(0xFF, 0xC1, 0x07),
        TrayState::Error(_) => make_icon(0xF4, 0x43, 0x36),
        TrayState::Offline => make_icon(0x9E, 0x9E, 0x9E),
    }
}

fn tooltip_for_state(state: &TrayState) -> String {
    match state {
        TrayState::Idle => "ImmichSync — Idle".to_owned(),
        TrayState::Syncing { current, total } => {
            format!("ImmichSync — Uploading {}/{}", current, total)
        }
        TrayState::Paused => "ImmichSync — Paused".to_owned(),
        TrayState::Error(msg) => format!("ImmichSync — Error: {}", msg),
        TrayState::Offline => "ImmichSync — Offline".to_owned(),
    }
}

fn status_text_for_state(state: &TrayState) -> String {
    match state {
        TrayState::Idle => "Idle — watching for new files".to_owned(),
        TrayState::Syncing { current, total } => {
            format!("Uploading {}/{} files…", current, total)
        }
        TrayState::Paused => "Paused".to_owned(),
        TrayState::Error(msg) => format!("Error: {}", msg),
        TrayState::Offline => "Server offline".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// TrayApp
// ---------------------------------------------------------------------------

/// Manages the system tray icon, context menu, and action dispatch.
///
/// # Lifetime
///
/// `TrayApp` must live on the main thread for the duration of the application.
/// Drop it when the event loop exits.
pub struct TrayApp {
    /// The live tray icon handle.  Dropping it removes the icon from the
    /// system tray.
    _tray: TrayIcon,

    /// Current logical state (used for icon / menu updates).
    state: TrayState,

    /// Current server display string (e.g. "photos.example.com [connected]").
    /// Cached for reconnect / status queries; not read internally after assignment.
    #[allow(dead_code)]
    server_status: String,

    /// Menu items that may be shown/hidden depending on state.
    pause_item: MenuItem,
    resume_item: MenuItem,
    status_item: MenuItem,
    server_item: MenuItem,
    #[allow(dead_code)]
    check_updates_item: MenuItem,
    update_available_item: MenuItem,
    restart_to_update_item: MenuItem,

    /// Sender half of the action channel.  Cloned into the menu-event handler.
    action_tx: mpsc::Sender<TrayAction>,
}

impl TrayApp {
    /// Create the system tray icon and set up the context menu.
    ///
    /// Returns `(TrayApp, receiver)`.  Poll the receiver on the event loop to
    /// handle user actions.
    pub fn new() -> Result<(Self, mpsc::Receiver<TrayAction>), TrayError> {
        let initial_state = TrayState::Idle;
        let icon = icon_for_state(&initial_state)?;

        // ---- Build menu items ----

        // Status line — disabled (not clickable), updated dynamically.
        let status_item = MenuItem::new(
            status_text_for_state(&initial_state),
            false, // not enabled (display-only)
            None,
        );

        // Server line — disabled, updated dynamically.
        let server_item = MenuItem::new("Server: (not configured)", false, None);

        // Action items.
        let pause_item = MenuItem::with_id(ID_PAUSE, "Pause Sync", true, None);
        let resume_item = MenuItem::with_id(ID_RESUME, "Resume Sync", false, None);
        let upload_now_item = MenuItem::with_id(ID_UPLOAD_NOW, "Upload Now", true, None);
        let settings_item = MenuItem::with_id(ID_SETTINGS, "Settings", true, None);
        let view_log_item = MenuItem::with_id(ID_VIEW_LOG, "View Upload Log", true, None);
        let view_trash_item = MenuItem::with_id(ID_VIEW_TRASH, "View Trash", true, None);
        let about_item = MenuItem::with_id(ID_ABOUT, "About ImmichSync", true, None);
        let check_updates_item =
            MenuItem::with_id(ID_CHECK_UPDATES, "Check for Updates", true, None);
        let update_available_item =
            MenuItem::with_id(ID_UPDATE_AVAILABLE, "Update Available!", false, None);
        let restart_to_update_item =
            MenuItem::with_id(ID_RESTART_TO_UPDATE, "Restart to Update", false, None);
        let quit_item = MenuItem::with_id(ID_QUIT, "Quit", true, None);

        // ---- Assemble context menu ----

        let menu = Menu::new();

        // "ImmichSync" header — disabled label.
        let header_item = MenuItem::new("ImmichSync", false, None);

        menu.append(&header_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&status_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&pause_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&resume_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&upload_now_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&settings_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&view_log_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&view_trash_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&about_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&check_updates_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&update_available_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&restart_to_update_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&server_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(|e| TrayError::Build(e.to_string()))?;
        menu.append(&quit_item)
            .map_err(|e| TrayError::Build(e.to_string()))?;

        // ---- Build the tray icon ----

        let tray = TrayIcon::new(TrayIconAttributes {
            tooltip: Some(tooltip_for_state(&initial_state)),
            menu: Some(Box::new(menu)),
            icon: Some(icon),
            ..Default::default()
        })
        .map_err(|e| TrayError::Build(e.to_string()))?;

        // ---- Action channel ----

        let (action_tx, action_rx) = mpsc::channel::<TrayAction>();

        // Subscribe to menu events and forward them through the action channel.
        // tray-icon 0.19 uses a static receiver from MenuEvent::receiver().
        // We spawn a dedicated thread to forward events.
        {
            let tx = action_tx.clone();
            std::thread::spawn(move || {
                let receiver = MenuEvent::receiver();
                loop {
                    match receiver.recv() {
                        Ok(event) => {
                            let action = match event.id().0.as_str() {
                                ID_PAUSE => Some(TrayAction::Pause),
                                ID_RESUME => Some(TrayAction::Resume),
                                ID_UPLOAD_NOW => Some(TrayAction::UploadNow),
                                ID_SETTINGS => Some(TrayAction::OpenSettings),
                                ID_ABOUT => Some(TrayAction::About),
                                ID_VIEW_LOG => Some(TrayAction::ViewLog),
                                ID_VIEW_TRASH => Some(TrayAction::ViewTrash),
                                ID_CHECK_UPDATES => Some(TrayAction::CheckForUpdates),
                                ID_UPDATE_AVAILABLE => Some(TrayAction::OpenUpdateDialog),
                                ID_RESTART_TO_UPDATE => Some(TrayAction::RestartToUpdate),
                                ID_QUIT => Some(TrayAction::Quit),
                                _ => None,
                            };
                            if let Some(a) = action {
                                tracing::debug!("tray action: {:?}", a);
                                if tx.send(a).is_err() {
                                    // Main thread has dropped the receiver; exit.
                                    break;
                                }
                            }
                        }
                        Err(_) => break, // channel closed
                    }
                }
            });
        }

        tracing::info!("system tray icon created");

        let app = Self {
            _tray: tray,
            state: initial_state,
            server_status: "Server: (not configured)".to_owned(),
            pause_item,
            resume_item,
            status_item,
            server_item,
            check_updates_item,
            update_available_item,
            restart_to_update_item,
            action_tx,
        };

        Ok((app, action_rx))
    }

    /// Update the tray icon and status text based on the new state.
    pub fn update_state(&mut self, state: TrayState) {
        tracing::debug!("tray state -> {:?}", state);

        // Update icon.
        if let Ok(icon) = icon_for_state(&state) {
            let _ = self._tray.set_icon(Some(icon));
        }

        // Update tooltip.
        let _ = self._tray.set_tooltip(Some(tooltip_for_state(&state)));

        // Update status line text.
        self.status_item.set_text(status_text_for_state(&state));

        // Show pause vs. resume depending on state.
        match &state {
            TrayState::Paused => {
                self.pause_item.set_enabled(false);
                self.resume_item.set_enabled(true);
            }
            _ => {
                self.pause_item.set_enabled(true);
                self.resume_item.set_enabled(false);
            }
        }

        self.state = state;
    }

    /// Update the server connection line in the tray menu.
    ///
    /// `url` should be just the hostname or a short display form
    /// (e.g. `"photos.example.com"`).
    pub fn set_server_status(&mut self, url: &str, connected: bool) {
        let status = if connected {
            format!("Server: {} [connected]", url)
        } else {
            format!("Server: {} [offline]", url)
        };
        tracing::debug!("tray server status: {}", status);
        self.server_item.set_text(&status);
        self.server_status = status;
    }

    /// Show or hide the "Update Available!" menu item.
    ///
    /// When `version` is `Some`, the item is shown with the version in the
    /// label text. When `None`, the item is hidden (disabled).
    pub fn set_update_available(&mut self, version: Option<&str>) {
        match version {
            Some(v) => {
                self.update_available_item
                    .set_text(&format!("Update Available (v{v})!"));
                self.update_available_item.set_enabled(true);
            }
            None => {
                self.update_available_item.set_text("Update Available!");
                self.update_available_item.set_enabled(false);
            }
        }
    }

    /// Show or hide the "Restart to Update" menu item.
    ///
    /// When `version` is `Some`, the item is shown and the "Update Available!"
    /// item is hidden (download is already done). When `None`, the item is hidden.
    pub fn set_restart_to_update(&mut self, version: Option<&str>) {
        match version {
            Some(v) => {
                self.restart_to_update_item
                    .set_text(&format!("Restart to Update (v{v})"));
                self.restart_to_update_item.set_enabled(true);
                // Hide the "Update Available" item since the download is done.
                self.update_available_item.set_text("Update Available!");
                self.update_available_item.set_enabled(false);
            }
            None => {
                self.restart_to_update_item.set_text("Restart to Update");
                self.restart_to_update_item.set_enabled(false);
            }
        }
    }

    /// Return a clone of the action sender, useful for injecting actions
    /// programmatically (e.g. from a device event thread).
    pub fn action_sender(&self) -> mpsc::Sender<TrayAction> {
        self.action_tx.clone()
    }
}

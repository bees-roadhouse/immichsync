// Settings window (egui/eframe).
//
// Tabbed UI: Connection, Watch Folders, Upload, Advanced.
// Runs on its own thread via `eframe::run_native()` so it doesn't block the
// main Win32 message loop.
//
// When the user clicks Save, the modified config is sent back to the main
// app via a `std::sync::mpsc::Sender<Config>` so the running app can
// diff and apply the changes live.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use eframe::egui;
use tracing::info;

use crate::config::Config;
use crate::db::{AlbumMode, Database, PostUpload, WatchMode, WatchedFolder};
use crate::watch::filter::{parse_patterns_json, patterns_to_json};

/// Open the settings window (blocking the calling thread).
///
/// Safe to call from any thread — uses `with_any_thread(true)` so eframe can
/// create its event loop off the main thread.  The `App` spawns this on a
/// dedicated thread so the tray stays responsive.
///
/// When the user clicks Save, the modified config is sent through `result_tx`
/// so the `App` can apply changes at runtime.
pub fn show_settings(config: Config, result_tx: Option<Sender<Config>>) {
    let options = eframe::NativeOptions {
        // Windows only: macOS windows run on their subprocess's main thread.
        #[cfg(windows)]
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            builder.with_any_thread(true);
        })),
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 420.0])
            .with_title("ImmichSync Settings")
            .with_icon(crate::ui::window_icon::brand_icon_data()),
        ..Default::default()
    };

    // Load watched folders from database for the Watch Folders tab.
    let folders = load_folders();

    // Hydrate per-folder editable pattern text (one glob per line) from the
    // JSON-encoded DB columns.
    let mut include_text: HashMap<i64, String> = HashMap::new();
    let mut exclude_text: HashMap<i64, String> = HashMap::new();
    for f in &folders {
        include_text.insert(f.id, patterns_text_from_json(f.include_patterns.as_deref()));
        exclude_text.insert(f.id, patterns_text_from_json(f.exclude_patterns.as_deref()));
    }

    let app = SettingsApp {
        // Connection tab
        server_url: config.server.url.clone(),
        api_key: config.server.api_key.clone(),
        device_name: config.server.device_id.clone(),
        test_status: String::new(),

        // Upload tab
        concurrency: config.upload.concurrency,
        upload_order: config.upload.order.clone(),
        max_retries: config.upload.max_retries,
        bandwidth_limit: config.upload.bandwidth_limit_kbps,

        // Advanced tab
        poll_interval: config.advanced.poll_interval_secs,
        log_level: config.advanced.log_level.clone(),
        autostart: config.ui.start_with_windows,
        minimize_to_tray: config.ui.minimize_to_tray,
        show_notifications: config.ui.show_notifications,
        notify_on_complete: config.ui.notification_on_complete,
        write_settle_ms: config.advanced.write_settle_ms,
        check_for_updates: config.advanced.check_for_updates,
        update_check_interval_hours: config.advanced.update_check_interval_hours,
        update_repo: config.advanced.update_repo.clone(),
        update_channel_mode: ChannelMode::from_str(&config.advanced.update_channel),
        update_tag_pin: ChannelMode::extract_tag(&config.advanced.update_channel),
        update_channel_error: String::new(),

        // Photos library tab (macOS)
        #[cfg(target_os = "macos")]
        photos: PhotosTabState::new(&config.photos),

        // Watch Folders tab
        folders,
        folder_to_remove: None,
        apply_existing: HashMap::new(),
        include_text,
        exclude_text,

        // State
        active_tab: Tab::Connection,
        config,
        result_tx,
    };

    if let Err(e) = eframe::run_native(
        "ImmichSync Settings",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    ) {
        tracing::error!(error = %e, "Failed to open Settings window");
    }
}

// ── Tab enum ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Connection,
    WatchFolders,
    #[cfg(target_os = "macos")]
    Photos,
    Upload,
    Advanced,
}

// ── Channel mode (UI representation of update_channel) ──────────────────────

/// UI-side channel selector. Maps onto `config.advanced.update_channel`
/// (which is the raw string the updater consumes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelMode {
    Latest,
    Prerelease,
    PinToTag,
    Disabled,
}

impl ChannelMode {
    fn from_str(s: &str) -> Self {
        match s.trim() {
            "prerelease" => ChannelMode::Prerelease,
            "none" => ChannelMode::Disabled,
            "" | "latest" => ChannelMode::Latest,
            other if other.starts_with('v') => ChannelMode::PinToTag,
            _ => ChannelMode::Latest,
        }
    }

    /// Pull the tag out if the raw channel string is a pin; otherwise empty.
    fn extract_tag(s: &str) -> String {
        if s.trim().starts_with('v') {
            s.trim().to_string()
        } else {
            String::new()
        }
    }

    fn label(&self) -> &'static str {
        match self {
            ChannelMode::Latest => "Latest stable",
            ChannelMode::Prerelease => "Latest (including prereleases)",
            ChannelMode::PinToTag => "Pin to tag",
            ChannelMode::Disabled => "Disabled",
        }
    }
}

// ── Settings app state ──────────────────────────────────────────────────────

struct SettingsApp {
    // Connection tab
    server_url: String,
    api_key: String,
    device_name: String,
    test_status: String,

    // Upload tab
    concurrency: u32,
    upload_order: String,
    max_retries: u32,
    bandwidth_limit: u64,

    // Advanced tab
    poll_interval: u64,
    log_level: String,
    autostart: bool,
    minimize_to_tray: bool,
    show_notifications: bool,
    notify_on_complete: bool,
    write_settle_ms: u64,
    check_for_updates: bool,
    update_check_interval_hours: u32,
    update_repo: String,
    update_channel_mode: ChannelMode,
    update_tag_pin: String,
    update_channel_error: String,

    #[cfg(target_os = "macos")]
    photos: PhotosTabState,

    // Watch Folders tab
    folders: Vec<WatchedFolder>,
    folder_to_remove: Option<i64>,
    /// Transient per-folder checkbox: apply trash/delete to already-uploaded files on Save.
    apply_existing: HashMap<i64, bool>,
    /// Transient per-folder include-patterns text buffer (one glob per line).
    include_text: HashMap<i64, String>,
    /// Transient per-folder exclude-patterns text buffer (one glob per line).
    exclude_text: HashMap<i64, String>,

    // State
    active_tab: Tab,
    config: Config,
    result_tx: Option<Sender<Config>>,
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Tab selector at the top.
        egui::TopBottomPanel::top("tab_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.active_tab, Tab::Connection, "Connection");
                ui.selectable_value(&mut self.active_tab, Tab::WatchFolders, "Watch Folders");
                #[cfg(target_os = "macos")]
                ui.selectable_value(&mut self.active_tab, Tab::Photos, "Photos Library");
                ui.selectable_value(&mut self.active_tab, Tab::Upload, "Upload");
                ui.selectable_value(&mut self.active_tab, Tab::Advanced, "Advanced");
            });
        });

        // Bottom panel: Save/Cancel.
        egui::TopBottomPanel::bottom("button_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() && self.save_config() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Cancel").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.add_space(4.0);
        });

        // Tab content.
        egui::CentralPanel::default().show(ctx, |ui| match self.active_tab {
            Tab::Connection => self.show_connection_tab(ui),
            Tab::WatchFolders => self.show_watch_folders_tab(ui),
            #[cfg(target_os = "macos")]
            Tab::Photos => self.photos.show(ui, &self.server_url, &self.api_key),
            Tab::Upload => self.show_upload_tab(ui),
            Tab::Advanced => self.show_advanced_tab(ui),
        });

        // Process deferred folder removal (avoid borrow conflict).
        if let Some(id) = self.folder_to_remove.take() {
            self.remove_folder(id);
        }
    }
}

// ── Tab rendering ───────────────────────────────────────────────────────────

impl SettingsApp {
    fn show_connection_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Server Connection");
        ui.add_space(8.0);

        egui::Grid::new("connection_grid")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Server URL:");
                ui.add_sized(
                    [340.0, 20.0],
                    egui::TextEdit::singleline(&mut self.server_url)
                        .hint_text("https://photos.example.com"),
                );
                ui.end_row();

                ui.label("API Key:");
                ui.add_sized(
                    [340.0, 20.0],
                    egui::TextEdit::singleline(&mut self.api_key).password(true),
                );
                ui.end_row();

                ui.label("Device Name:");
                ui.add_sized(
                    [340.0, 20.0],
                    egui::TextEdit::singleline(&mut self.device_name)
                        .hint_text("immichsync-hostname"),
                );
                ui.end_row();
            });

        ui.add_space(12.0);

        ui.horizontal(|ui| {
            if ui.button("Test Connection").clicked() {
                self.test_connection();
            }
            if !self.test_status.is_empty() {
                ui.add_space(8.0);
                ui.label(&self.test_status);
            }
        });
    }

    fn show_watch_folders_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Watch Folders");
        ui.add_space(8.0);

        if ui.button("Add Folder…").clicked() {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                self.add_folder(path);
            }
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        let mut remove_id = None;

        egui::ScrollArea::vertical()
            .max_height(260.0)
            .show(ui, |ui| {
                for folder in &mut self.folders {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut folder.enabled, "");
                            ui.label(&folder.path);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("Remove").clicked() {
                                        remove_id = Some(folder.id);
                                    }
                                },
                            );
                        });

                        ui.horizontal(|ui| {
                            ui.label("Album:");
                            let album_mode_str = folder.album_mode.as_str();
                            let mut selected = album_mode_str.to_string();
                            egui::ComboBox::from_id_salt(format!("album_{}", folder.id))
                                .width(120.0)
                                .selected_text(&selected)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut selected, "none".to_string(), "None");
                                    ui.selectable_value(
                                        &mut selected,
                                        "folder".to_string(),
                                        "Folder Name",
                                    );
                                    ui.selectable_value(
                                        &mut selected,
                                        "subfolder".to_string(),
                                        "Subfolder Name",
                                    );
                                    ui.selectable_value(
                                        &mut selected,
                                        "date".to_string(),
                                        "Date (YYYY-MM)",
                                    );
                                    ui.selectable_value(
                                        &mut selected,
                                        "fixed".to_string(),
                                        "Fixed Album",
                                    );
                                });
                            folder.album_mode = AlbumMode::from_str(&selected);

                            if folder.album_mode == AlbumMode::Fixed {
                                let name = folder.album_name.get_or_insert_with(String::new);
                                ui.label("Name:");
                                ui.add_sized([120.0, 18.0], egui::TextEdit::singleline(name));
                            }
                        });

                        ui.horizontal(|ui| {
                            ui.label("After Upload:");
                            let post_str = folder.post_upload.as_str();
                            let mut post_selected = post_str.to_string();
                            egui::ComboBox::from_id_salt(format!("post_{}", folder.id))
                                .width(120.0)
                                .selected_text(&post_selected)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut post_selected,
                                        "keep".to_string(),
                                        "Keep",
                                    );
                                    ui.selectable_value(
                                        &mut post_selected,
                                        "trash".to_string(),
                                        "Move to Trash",
                                    );
                                    ui.selectable_value(
                                        &mut post_selected,
                                        "delete".to_string(),
                                        "Delete",
                                    );
                                });
                            folder.post_upload = PostUpload::from_str(&post_selected);
                        });

                        // Show checkbox to apply action to already-uploaded files.
                        if folder.post_upload != PostUpload::Keep {
                            let apply = self.apply_existing.entry(folder.id).or_insert(false);
                            let label = match folder.post_upload {
                                PostUpload::Trash => "Also trash already-uploaded files",
                                PostUpload::Delete => "Also delete already-uploaded files",
                                PostUpload::Keep => unreachable!(),
                            };
                            ui.checkbox(apply, label);
                        } else {
                            // Reset if user switches back to Keep.
                            self.apply_existing.remove(&folder.id);
                        }

                        // Per-folder include/exclude glob patterns. One pattern per line.
                        // Empty buffers leave the filter at default (extension-only).
                        ui.add_space(4.0);
                        ui.label("Include patterns (one glob per line, blank = no restriction):");
                        let include_buf = self.include_text.entry(folder.id).or_default();
                        ui.add(
                            egui::TextEdit::multiline(include_buf)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .hint_text("e.g. **/raw/** or *.cr3"),
                        );

                        ui.label("Exclude patterns (one glob per line):");
                        let exclude_buf = self.exclude_text.entry(folder.id).or_default();
                        ui.add(
                            egui::TextEdit::multiline(exclude_buf)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .hint_text("e.g. **/thumbnails/**"),
                        );

                        // Skip cloud-storage placeholder files (OneDrive Files
                        // On-Demand, iCloud, SeaDrive, Google Drive, etc.).
                        // Default ON ... reading a placeholder triggers a
                        // cloud download as a side effect, which can re-pull
                        // an entire offloaded library on first scan.
                        ui.checkbox(
                            &mut folder.ignore_online_files,
                            "Skip cloud-only files (placeholders, not materialized locally)",
                        );
                    });
                    ui.add_space(2.0);
                }
            });

        if let Some(id) = remove_id {
            self.folder_to_remove = Some(id);
        }
    }

    fn show_upload_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Upload Settings");
        ui.add_space(8.0);

        egui::Grid::new("upload_grid")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Concurrency:");
                ui.add(egui::Slider::new(&mut self.concurrency, 1..=8));
                ui.end_row();

                ui.label("Upload Order:");
                egui::ComboBox::from_id_salt("upload_order")
                    .selected_text(if self.upload_order == "oldest_first" {
                        "Oldest First"
                    } else {
                        "Newest First"
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.upload_order,
                            "newest_first".to_string(),
                            "Newest First",
                        );
                        ui.selectable_value(
                            &mut self.upload_order,
                            "oldest_first".to_string(),
                            "Oldest First",
                        );
                    });
                ui.end_row();

                ui.label("Max Retries:");
                ui.add(egui::Slider::new(&mut self.max_retries, 1..=20));
                ui.end_row();

                ui.label("Bandwidth Limit (KB/s):");
                ui.horizontal(|ui| {
                    let mut limit_i32 = self.bandwidth_limit as i32;
                    ui.add(egui::DragValue::new(&mut limit_i32).range(0..=100_000));
                    self.bandwidth_limit = limit_i32.max(0) as u64;
                    if self.bandwidth_limit == 0 {
                        ui.label("(unlimited)");
                    }
                });
                ui.end_row();
            });
    }

    fn show_advanced_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Advanced Settings");
        ui.add_space(8.0);

        egui::Grid::new("advanced_grid")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Log Level:").on_hover_text(
                    "Applies the next time ImmichSync starts. RUST_LOG overrides it.",
                );
                egui::ComboBox::from_id_salt("log_level")
                    .selected_text(&self.log_level)
                    .show_ui(ui, |ui| {
                        for level in &["trace", "debug", "info", "warn", "error"] {
                            ui.selectable_value(&mut self.log_level, level.to_string(), *level);
                        }
                    });
                ui.end_row();
            });

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        ui.checkbox(&mut self.autostart, crate::platform::AUTOSTART_LABEL);
        ui.checkbox(&mut self.show_notifications, "Show notifications");
        ui.checkbox(
            &mut self.check_for_updates,
            "Automatically check for updates",
        );
        ui.add_enabled_ui(self.check_for_updates, |ui| {
            ui.indent("update_indent", |ui| {
                ui.horizontal(|ui| {
                    ui.label("Check every");
                    let mut hours_i32 = self.update_check_interval_hours as i32;
                    ui.add(egui::DragValue::new(&mut hours_i32).range(1..=168));
                    self.update_check_interval_hours = hours_i32.max(1) as u32;
                    ui.label("hours");
                });
                ui.horizontal(|ui| {
                    ui.label("Repository:");
                    ui.text_edit_singleline(&mut self.update_repo);
                });
                ui.horizontal(|ui| {
                    ui.label("Channel:");
                    egui::ComboBox::from_id_salt("update_channel")
                        .selected_text(self.update_channel_mode.label())
                        .show_ui(ui, |ui| {
                            for mode in [
                                ChannelMode::Latest,
                                ChannelMode::Prerelease,
                                ChannelMode::PinToTag,
                                ChannelMode::Disabled,
                            ] {
                                ui.selectable_value(
                                    &mut self.update_channel_mode,
                                    mode,
                                    mode.label(),
                                );
                            }
                        });
                });
                if self.update_channel_mode == ChannelMode::PinToTag {
                    ui.horizontal(|ui| {
                        ui.label("Tag:");
                        ui.text_edit_singleline(&mut self.update_tag_pin);
                        ui.label("(e.g. v0.1.8)");
                    });
                }
                if !self.update_channel_error.is_empty() {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 80, 80),
                        &self.update_channel_error,
                    );
                }
            });
        });
        ui.add_enabled_ui(self.show_notifications, |ui| {
            ui.indent("notify_indent", |ui| {
                ui.checkbox(&mut self.notify_on_complete, "Notify when uploads complete");
            });
        });
    }
}

// ── Config save / test ──────────────────────────────────────────────────────

impl SettingsApp {
    fn test_connection(&mut self) {
        if self.server_url.is_empty() || self.api_key.is_empty() {
            self.test_status = "Enter URL and API key first".to_string();
            return;
        }

        match crate::api::ImmichClient::new(&self.server_url, &self.api_key) {
            Ok(client) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match rt {
                    Ok(rt) => match rt.block_on(client.ping()) {
                        Ok(true) => {
                            self.test_status = "Connected!".to_string();
                        }
                        Ok(false) => {
                            self.test_status = "Server responded but not ready".to_string();
                        }
                        Err(e) => {
                            self.test_status = format!("Failed: {e}");
                        }
                    },
                    Err(e) => {
                        self.test_status = format!("Runtime error: {e}");
                    }
                }
            }
            Err(e) => {
                self.test_status = format!("Invalid config: {e}");
            }
        }
    }

    fn save_config(&mut self) -> bool {
        // Validate update settings before touching config. Bad values keep the
        // window open with a visible error so the user can correct them.
        self.update_channel_error.clear();
        let repo_trim = self.update_repo.trim().to_string();
        if !repo_trim.is_empty() && !crate::updater::is_valid_repo(&repo_trim) {
            self.update_channel_error =
                "Repository must be in 'owner/name' format (public github.com only).".to_string();
            self.active_tab = Tab::Advanced;
            return false;
        }
        self.update_repo = repo_trim;

        if self.update_channel_mode == ChannelMode::PinToTag {
            let tag = self.update_tag_pin.trim().to_string();
            if crate::updater::Channel::parse(&tag).is_none() {
                self.update_channel_error =
                    "Tag pin must look like 'vX.Y.Z' (or 'vX.Y.Z-prerelease').".to_string();
                self.active_tab = Tab::Advanced;
                return false;
            }
            self.update_tag_pin = tag;
        }

        // Apply UI state back to config.
        self.config.server.url = self.server_url.clone();
        self.config.server.api_key = self.api_key.clone();
        self.config.server.device_id = self.device_name.clone();

        #[cfg(target_os = "macos")]
        {
            self.config.photos.enabled = self.photos.enabled;
            self.config.photos.icloud_originals = if self.photos.download_icloud {
                crate::config::ICloudOriginals::Download
            } else {
                crate::config::ICloudOriginals::Skip
            };
            let (sync_albums, all_in) = self.photos.albums.to_config();
            self.config.photos.sync_albums = sync_albums;
            self.config.photos.all_in_album = all_in;
        }

        self.config.upload.concurrency = self.concurrency;
        self.config.upload.order = self.upload_order.clone();
        self.config.upload.max_retries = self.max_retries;
        self.config.upload.bandwidth_limit_kbps = self.bandwidth_limit;

        self.config.advanced.poll_interval_secs = self.poll_interval;
        self.config.advanced.log_level = self.log_level.clone();
        self.config.advanced.write_settle_ms = self.write_settle_ms;
        self.config.advanced.check_for_updates = self.check_for_updates;
        self.config.advanced.update_check_interval_hours = self.update_check_interval_hours;
        self.config.advanced.update_repo = self.update_repo.clone();
        self.config.advanced.update_channel = match self.update_channel_mode {
            ChannelMode::Latest => "latest".to_string(),
            ChannelMode::Prerelease => "prerelease".to_string(),
            ChannelMode::Disabled => "none".to_string(),
            ChannelMode::PinToTag => self.update_tag_pin.trim().to_string(),
        };

        self.config.ui.start_with_windows = self.autostart;
        self.config.ui.minimize_to_tray = self.minimize_to_tray;
        self.config.ui.show_notifications = self.show_notifications;
        self.config.ui.notification_on_complete = self.notify_on_complete;

        // Save config to disk.
        if let Err(e) = self.config.save() {
            tracing::error!(error = %e, "Failed to save config");
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("ImmichSync")
                .set_description(format!("Settings were not saved: {e}"))
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
        } else {
            info!("Config saved");
        }

        // Save folder changes to database.
        self.save_folders();

        // Toggle autostart.
        if let Err(e) = crate::platform::set_autostart(self.autostart) {
            tracing::warn!(error = %e, "Failed to set autostart");
            // Say so: a silently failed "off" leaves the app starting at
            // login while the checkbox says it won't.
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Warning)
                .set_title("ImmichSync")
                .set_description(format!(
                    "Couldn't change \"{}\": {e}",
                    crate::platform::AUTOSTART_LABEL
                ))
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
        }

        // Notify the running app of the new config so it can apply changes.
        if let Some(ref tx) = self.result_tx {
            let _ = tx.send(self.config.clone());
        }

        true
    }
}

// ── Folder management (database) ────────────────────────────────────────────

impl SettingsApp {
    fn add_folder(&mut self, path: PathBuf) {
        // Store the canonical form, like everything that reads files (#99).
        let path_str = match crate::watch::paths::normalize_folder(&path.to_string_lossy()) {
            Ok(p) => p,
            Err(e) => {
                rfd::MessageDialog::new()
                    .set_level(rfd::MessageLevel::Error)
                    .set_title("ImmichSync")
                    .set_description(format!("Can't watch this folder: {e}"))
                    .set_buttons(rfd::MessageButtons::Ok)
                    .show();
                return;
            }
        };

        // Check for duplicate.
        if self.folders.iter().any(|f| f.path == path_str) {
            return;
        }

        if let Ok(db) = open_db() {
            match db.add_folder(&path_str, None, false) {
                Ok(id) => {
                    // Re-load from DB to get all fields populated correctly.
                    if let Ok(Some(folder)) = db.get_folder_by_path(&path_str) {
                        self.folders.push(folder);
                    } else {
                        // Fallback: construct manually.
                        self.folders.push(WatchedFolder {
                            id,
                            path: path_str,
                            label: None,
                            enabled: true,
                            watch_mode: WatchMode::Native,
                            poll_interval_secs: 30,
                            album_mode: AlbumMode::None,
                            album_name: None,
                            include_patterns: None,
                            exclude_patterns: None,
                            post_upload: PostUpload::Keep,
                            ignore_online_files: true,
                            auto_added: false,
                            created_at: String::new(),
                            updated_at: String::new(),
                        });
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to add folder");
                }
            }
        }
    }

    fn remove_folder(&mut self, id: i64) {
        if let Ok(db) = open_db() {
            if let Err(e) = db.remove_folder(id) {
                tracing::error!(error = %e, "Failed to remove folder");
                return;
            }
        }
        self.folders.retain(|f| f.id != id);
        self.include_text.remove(&id);
        self.exclude_text.remove(&id);
        self.apply_existing.remove(&id);
    }

    fn save_folders(&self) {
        if let Ok(db) = open_db() {
            for folder in &self.folders {
                // Convert per-folder line-separated text buffers into JSON
                // arrays before persisting. Empty buffers store NULL.
                let include_json = self
                    .include_text
                    .get(&folder.id)
                    .map(|s| patterns_text_to_json(s))
                    .unwrap_or(None);
                let exclude_json = self
                    .exclude_text
                    .get(&folder.id)
                    .map(|s| patterns_text_to_json(s))
                    .unwrap_or(None);

                if let Err(e) = db.update_folder(
                    folder.id,
                    folder.label.as_deref(),
                    folder.enabled,
                    &folder.watch_mode,
                    folder.poll_interval_secs,
                    &folder.album_mode,
                    folder.album_name.as_deref(),
                    include_json.as_deref(),
                    exclude_json.as_deref(),
                    &folder.post_upload,
                    folder.ignore_online_files,
                ) {
                    tracing::warn!(id = folder.id, error = %e, "Failed to update folder");
                }

                // Apply trash/delete to already-uploaded files if the user checked the box.
                if self
                    .apply_existing
                    .get(&folder.id)
                    .copied()
                    .unwrap_or(false)
                {
                    self.apply_post_upload_to_existing(&db, folder);
                }
            }
        }
    }

    /// Trash or delete files in `folder` that have already been uploaded.
    fn apply_post_upload_to_existing(&self, db: &Database, folder: &WatchedFolder) {
        let records = match db.get_uploaded_in_folder(&folder.path) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(folder = %folder.path, error = %e,
                    "Failed to query uploaded files for existing cleanup");
                return;
            }
        };

        info!(folder = %folder.path, count = records.len(),
            "Applying post-upload action to already-uploaded files");

        let mut count = 0u32;
        let mut changed = 0u32;
        let mut skipped_dirs = 0u32;
        for rec in &records {
            let clean = crate::db::strip_extended_prefix(&rec.file_path);
            let path = std::path::Path::new(clean);
            if !path.exists() {
                continue;
            }
            // Never move or delete inside an app library, .Trashes and the
            // like, even for files uploaded before the filter skipped them (#96).
            if crate::watch::filter::in_skipped_dir(path) {
                skipped_dirs += 1;
                continue;
            }
            // Only act on the exact bytes Immich has.
            match crate::upload::worker::still_matches_upload(path, rec) {
                Ok(true) => {}
                Ok(false) => {
                    changed += 1;
                    tracing::warn!(file = %clean,
                        "File changed since upload; leaving it in place");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(file = %clean, error = %e,
                        "Could not re-check file; leaving it in place");
                    continue;
                }
            }

            match folder.post_upload {
                PostUpload::Trash => {
                    if let Err(e) = crate::upload::worker::trash_file(path, &folder.path) {
                        tracing::warn!(file = %clean, error = %e,
                            "Failed to trash existing uploaded file");
                    } else {
                        count += 1;
                    }
                }
                PostUpload::Delete => {
                    if let Err(e) = std::fs::remove_file(path) {
                        tracing::warn!(file = %clean, error = %e,
                            "Failed to delete existing uploaded file");
                    } else {
                        count += 1;
                    }
                }
                PostUpload::Keep => {}
            }
        }

        if skipped_dirs > 0 {
            info!(folder = %folder.path, skipped_dirs,
                "Left files inside app libraries / housekeeping folders alone");
        }
        if changed > 0 {
            info!(folder = %folder.path, changed,
                "Skipped files that changed since upload");
        }
        if count > 0 {
            let action = match folder.post_upload {
                PostUpload::Trash => "trashed",
                PostUpload::Delete => "deleted",
                PostUpload::Keep => return,
            };
            info!(
                folder = %folder.path, count,
                "Applied {action} to already-uploaded files"
            );
        }
    }
}

// ── Database helpers ────────────────────────────────────────────────────────

fn open_db() -> Result<Database, crate::db::DbError> {
    Database::open()
}

fn load_folders() -> Vec<WatchedFolder> {
    match open_db() {
        Ok(db) => db.get_folders().unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %e, "Failed to load folders for settings");
            Vec::new()
        }
    }
}

/// Convert a JSON-encoded array of patterns (from `watched_folders`) into
/// editable one-glob-per-line text for the multiline UI buffer.
fn patterns_text_from_json(raw: Option<&str>) -> String {
    parse_patterns_json(raw).join("\n")
}

/// Convert a one-glob-per-line text buffer into a JSON array string for DB
/// storage. Returns `None` if the buffer is empty (so the column stores NULL).
fn patterns_text_to_json(text: &str) -> Option<String> {
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    patterns_to_json(&lines)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_yields_none_json() {
        assert!(patterns_text_to_json("").is_none());
        assert!(patterns_text_to_json("   \n\n  \n").is_none());
    }

    #[test]
    fn lines_become_json_array() {
        let json = patterns_text_to_json("*.jpg\n**/raw/**").unwrap();
        // Order preserved; whitespace trimmed.
        let parsed: Vec<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, vec!["*.jpg".to_string(), "**/raw/**".to_string()]);
    }

    #[test]
    fn roundtrip_text_to_json_to_text() {
        let original = "*.jpg\n**/raw/**\n  ";
        let json = patterns_text_to_json(original).unwrap();
        let back = patterns_text_from_json(Some(&json));
        // Blank line is dropped; whitespace trimmed.
        assert_eq!(back, "*.jpg\n**/raw/**");
    }

    #[test]
    fn json_to_text_handles_none_and_malformed() {
        assert_eq!(patterns_text_from_json(None), "");
        // Malformed JSON yields empty text rather than panicking.
        assert_eq!(patterns_text_from_json(Some("not json")), "");
    }
}

// ── Photos library tab (macOS; ADR 0002) ────────────────────────────────────

/// What ticking **Sync my Photos library** does, given the current access.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhotosToggle {
    /// Access is already granted: just turn it on.
    Enable,
    /// macOS hasn't asked yet: raise the prompt (this click is the user's
    /// own action, which is the only time we ever prompt).
    Request,
    /// Can't be turned on; the reason is shown under the toggle.
    Refuse(&'static str),
}

#[cfg(target_os = "macos")]
fn photos_toggle(access: crate::platform::photos::Access, can_prompt: bool) -> PhotosToggle {
    use crate::platform::photos::Access;
    match access {
        Access::Authorized | Access::Limited => PhotosToggle::Enable,
        Access::NotDetermined if can_prompt => PhotosToggle::Request,
        Access::NotDetermined => PhotosToggle::Refuse(
            "The Photos library needs ImmichSync.app; this binary can't ask for access.",
        ),
        Access::Denied => PhotosToggle::Refuse(
            "ImmichSync isn't allowed to read Photos. Allow it in System Settings, then tick this again.",
        ),
        Access::Restricted => {
            PhotosToggle::Refuse("Photos access is restricted on this Mac (parental controls or MDM).")
        }
    }
}

/// The Photos tab's album choices (#115): mirror Photos albums, put
/// everything into one album, or neither. The first two combine.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct AlbumChoice {
    sync_albums: bool,
    all_in_on: bool,
    /// The chosen server album, or a new name to create.
    all_in: String,
}

#[cfg(target_os = "macos")]
impl AlbumChoice {
    fn from_config(cfg: &crate::config::PhotosConfig) -> Self {
        let all_in = cfg.all_in_album.trim().to_string();
        Self {
            sync_albums: cfg.sync_albums,
            all_in_on: !all_in.is_empty(),
            all_in,
        }
    }

    fn no_albums(&self) -> bool {
        !self.sync_albums && !self.all_in_on
    }

    /// Ticking "No albums" clears the other two; unticking it turns album
    /// sync back on (the default).
    fn set_no_albums(&mut self, on: bool) {
        if on {
            self.sync_albums = false;
            self.all_in_on = false;
        } else {
            self.sync_albums = true;
        }
    }

    /// `(sync_albums, all_in_album)` to save. A ticked catch-all with a
    /// blank name saves as off.
    fn to_config(&self) -> (bool, String) {
        let name = if self.all_in_on {
            self.all_in.trim().to_string()
        } else {
            String::new()
        };
        (self.sync_albums, name)
    }
}

#[cfg(target_os = "macos")]
struct PhotosTabState {
    enabled: bool,
    /// Download iCloud-only originals (`icloud_originals = "download"`).
    download_icloud: bool,
    /// Entries deferred as iCloud-only, read when the window opens.
    deferred: i64,
    albums: AlbumChoice,
    /// Album names on the server, for the catch-all dropdown.
    server_albums: Vec<String>,
    albums_rx: Option<std::sync::mpsc::Receiver<Result<Vec<String>, String>>>,
    albums_status: String,
    access: crate::platform::photos::Access,
    /// The answer to a prompt raised by the toggle, while it's showing.
    pending: Option<std::sync::mpsc::Receiver<anyhow::Result<crate::platform::photos::Access>>>,
    message: String,
}

#[cfg(target_os = "macos")]
impl PhotosTabState {
    fn new(cfg: &crate::config::PhotosConfig) -> Self {
        let access = crate::platform::photos::access();
        Self {
            // A config that says "on" without access shows as off until fixed.
            enabled: cfg.enabled && access.can_read(),
            download_icloud: cfg.icloud_originals == crate::config::ICloudOriginals::Download,
            deferred: open_db()
                .and_then(|db| db.deferred_photos_count())
                .unwrap_or(0),
            albums: AlbumChoice::from_config(cfg),
            server_albums: Vec::new(),
            albums_rx: None,
            albums_status: String::new(),
            access,
            pending: None,
            message: String::new(),
        }
    }

    /// Albums: mirror Photos albums, one catch-all, or none.
    fn show_albums(&mut self, ui: &mut egui::Ui, server_url: &str, api_key: &str) {
        if let Some(rx) = &self.albums_rx {
            match rx.try_recv() {
                Ok(Ok(names)) => {
                    self.albums_status = format!("{} albums on the server", names.len());
                    self.server_albums = names;
                    self.albums_rx = None;
                }
                Ok(Err(e)) => {
                    self.albums_status = format!("Couldn't load albums: {e}");
                    self.albums_rx = None;
                }
                Err(_) => ui
                    .ctx()
                    .request_repaint_after(std::time::Duration::from_millis(200)),
            }
        }

        ui.label("Albums");
        let c = &mut self.albums;
        ui.checkbox(&mut c.sync_albums, "Sync my Photos albums");
        ui.small(
            "Each Photos album becomes an Immich album; a photo in several albums is in each. \
             Smart albums and shared albums are left out.",
        );

        let was_on = c.all_in_on;
        ui.checkbox(
            &mut c.all_in_on,
            "Also put everything into one Immich album",
        );
        let load = c.all_in_on && !was_on && self.server_albums.is_empty();
        if c.all_in_on {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("photos_all_in")
                    .selected_text(if c.all_in.trim().is_empty() {
                        "Choose an album"
                    } else {
                        c.all_in.as_str()
                    })
                    .show_ui(ui, |ui| {
                        for name in &self.server_albums {
                            ui.selectable_value(&mut c.all_in, name.clone(), name);
                        }
                    });
                ui.label("or a new name:");
                ui.text_edit_singleline(&mut c.all_in);
            });
            ui.horizontal(|ui| {
                if ui.small_button("Refresh albums").clicked() && self.albums_rx.is_none() {
                    self.albums_rx = Some(fetch_album_names(server_url, api_key));
                    self.albums_status = "Loading albums…".to_string();
                }
                if !self.albums_status.is_empty() {
                    ui.small(&self.albums_status);
                }
            });
        }
        if load && self.albums_rx.is_none() {
            self.albums_rx = Some(fetch_album_names(server_url, api_key));
            self.albums_status = "Loading albums…".to_string();
        }

        let mut none = self.albums.no_albums();
        if ui.checkbox(&mut none, "No albums (upload only)").changed() {
            self.albums.set_no_albums(none);
        }
    }

    fn show(&mut self, ui: &mut egui::Ui, server_url: &str, api_key: &str) {
        use crate::platform::photos::{self, Access};

        if let Some(rx) = &self.pending {
            if let Ok(answer) = rx.try_recv() {
                self.pending = None;
                match answer {
                    Ok(access) => {
                        self.access = access;
                        self.enabled = access.can_read();
                        if !self.enabled {
                            if let PhotosToggle::Refuse(why) = photos_toggle(access, false) {
                                self.message = why.to_string();
                            }
                        }
                    }
                    Err(e) => {
                        self.enabled = false;
                        self.message = e.to_string();
                    }
                }
            } else {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(200));
            }
        }

        ui.heading("Photos Library");
        ui.add_space(8.0);
        ui.label(
            "Back up your Apple Photos library to Immich: originals only, never edits. \
             Hidden items and shared-library items are left out. \
             ImmichSync reads the library; it never changes or deletes photos.",
        );
        ui.add_space(8.0);

        let mut wanted = self.enabled;
        let waiting = self.pending.is_some();
        ui.add_enabled_ui(!waiting, |ui| {
            ui.checkbox(&mut wanted, "Sync my Photos library");
        });
        if wanted != self.enabled {
            self.message.clear();
            if !wanted {
                self.enabled = false;
            } else {
                match photos_toggle(self.access, photos::can_prompt()) {
                    PhotosToggle::Enable => self.enabled = true,
                    PhotosToggle::Request => {
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            let _ = tx.send(photos::request_access());
                        });
                        self.pending = Some(rx);
                    }
                    PhotosToggle::Refuse(why) => self.message = why.to_string(),
                }
            }
        }

        ui.add_space(4.0);
        if waiting {
            ui.label("Waiting for your answer to the macOS prompt…");
        }
        match self.access {
            Access::Authorized => {
                ui.label("Access: the whole library.");
            }
            Access::Limited => {
                ui.label("Access: only the items you selected.");
            }
            Access::Denied | Access::Restricted => {
                if ui.button("Open Privacy Settings").clicked() {
                    let _ = open::that(
                        "x-apple.systempreferences:com.apple.preference.security?Privacy_Photos",
                    );
                }
            }
            Access::NotDetermined => {}
        }
        if !self.message.is_empty() {
            ui.colored_label(egui::Color32::from_rgb(200, 120, 0), &self.message);
        }

        ui.add_space(8.0);
        ui.separator();
        ui.checkbox(
            &mut self.download_icloud,
            "Download originals that are only in iCloud",
        );
        ui.small(
            "With Optimize Mac Storage most originals live in iCloud. They are downloaded one at a \
             time and deleted once uploaded; the first sync downloads about the size of your library. \
             Exports pause while less than 2 GB is free.",
        );
        if self.deferred > 0 {
            ui.label(format!(
                "{} originals are only in iCloud and waiting{}.",
                self.deferred,
                if self.download_icloud {
                    " (they'll be downloaded after a restart)"
                } else {
                    ""
                }
            ));
        }

        ui.separator();
        self.show_albums(ui, server_url, api_key);
        ui.add_space(4.0);
        ui.small(
            "Syncs the whole library first, then new photos as you add them. \
             These choices apply after a restart.",
        );
    }
}

/// Load the server's album names on a background thread for the dropdown.
#[cfg(target_os = "macos")]
fn fetch_album_names(
    server_url: &str,
    api_key: &str,
) -> std::sync::mpsc::Receiver<Result<Vec<String>, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (url, key) = (server_url.to_string(), api_key.to_string());
    std::thread::spawn(move || {
        let result = (|| {
            if url.is_empty() || key.is_empty() {
                return Err("set the server URL and API key first".to_string());
            }
            let client = crate::api::ImmichClient::new(&url, &key).map_err(|e| e.to_string())?;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            let mut names: Vec<String> = rt
                .block_on(client.get_albums())
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|a| a.album_name)
                .collect();
            names.sort_by_key(|n| n.to_lowercase());
            names.dedup();
            Ok(names)
        })();
        let _ = tx.send(result);
    });
    rx
}

#[cfg(all(test, target_os = "macos"))]
mod photos_tests {
    use super::*;
    use crate::platform::photos::Access;

    #[test]
    fn album_choices_load_save_and_no_albums_is_exclusive() {
        let mut cfg = crate::config::PhotosConfig::default();
        let c = AlbumChoice::from_config(&cfg);
        assert!(
            c.sync_albums && !c.all_in_on && !c.no_albums(),
            "sync albums by default"
        );
        assert_eq!(c.to_config(), (true, String::new()));

        cfg.all_in_album = "Everything".into();
        let mut c = AlbumChoice::from_config(&cfg);
        assert!(c.sync_albums && c.all_in_on);
        assert_eq!(c.to_config(), (true, "Everything".into()));

        c.set_no_albums(true);
        assert!(c.no_albums());
        assert_eq!(c.to_config(), (false, String::new()));
        c.set_no_albums(false);
        assert_eq!(c.to_config(), (true, String::new()));

        // A ticked catch-all with no name saves as off; names are trimmed.
        let c = AlbumChoice {
            sync_albums: false,
            all_in_on: true,
            all_in: "  ".into(),
        };
        assert_eq!(c.to_config(), (false, String::new()));
        let c = AlbumChoice {
            sync_albums: false,
            all_in_on: true,
            all_in: " Trip ".into(),
        };
        assert_eq!(c.to_config(), (false, "Trip".into()));
    }

    #[test]
    fn the_toggle_prompts_only_when_undetermined_and_promptable() {
        assert_eq!(
            photos_toggle(Access::NotDetermined, true),
            PhotosToggle::Request
        );
        assert!(matches!(
            photos_toggle(Access::NotDetermined, false),
            PhotosToggle::Refuse(_)
        ));
        assert_eq!(
            photos_toggle(Access::Authorized, true),
            PhotosToggle::Enable
        );
        assert_eq!(photos_toggle(Access::Limited, false), PhotosToggle::Enable);
        // Denied never re-prompts (macOS wouldn't show it again anyway).
        assert!(matches!(
            photos_toggle(Access::Denied, true),
            PhotosToggle::Refuse(_)
        ));
        assert!(matches!(
            photos_toggle(Access::Restricted, true),
            PhotosToggle::Refuse(_)
        ));
    }
}

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
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            builder.with_any_thread(true);
        })),
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 420.0])
            .with_title("ImmichSync Settings"),
        ..Default::default()
    };

    // Load watched folders from database for the Watch Folders tab.
    let folders = load_folders();

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

        // Watch Folders tab
        folders,
        folder_to_remove: None,
        apply_existing: HashMap::new(),

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
    Upload,
    Advanced,
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

    // Watch Folders tab
    folders: Vec<WatchedFolder>,
    folder_to_remove: Option<i64>,
    /// Transient per-folder checkbox: apply trash/delete to already-uploaded files on Save.
    apply_existing: HashMap<i64, bool>,

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
                ui.selectable_value(&mut self.active_tab, Tab::Upload, "Upload");
                ui.selectable_value(&mut self.active_tab, Tab::Advanced, "Advanced");
            });
        });

        // Bottom panel: Save/Cancel.
        egui::TopBottomPanel::bottom("button_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    self.save_config();
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
                ui.label("Poll Interval (secs):");
                let mut poll_i32 = self.poll_interval as i32;
                ui.add(egui::DragValue::new(&mut poll_i32).range(5..=300));
                self.poll_interval = poll_i32.max(5) as u64;
                ui.end_row();

                ui.label("Log Level:");
                egui::ComboBox::from_id_salt("log_level")
                    .selected_text(&self.log_level)
                    .show_ui(ui, |ui| {
                        for level in &["trace", "debug", "info", "warn", "error"] {
                            ui.selectable_value(&mut self.log_level, level.to_string(), *level);
                        }
                    });
                ui.end_row();

                ui.label("Write Settle (ms):");
                let mut settle_i32 = self.write_settle_ms as i32;
                ui.add(egui::DragValue::new(&mut settle_i32).range(500..=10_000));
                self.write_settle_ms = settle_i32.max(500) as u64;
                ui.end_row();
            });

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        ui.checkbox(&mut self.autostart, "Start with Windows");
        ui.checkbox(&mut self.minimize_to_tray, "Minimize to system tray");
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

    fn save_config(&mut self) {
        // Apply UI state back to config.
        self.config.server.url = self.server_url.clone();
        self.config.server.api_key = self.api_key.clone();
        self.config.server.device_id = self.device_name.clone();

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

        self.config.ui.start_with_windows = self.autostart;
        self.config.ui.minimize_to_tray = self.minimize_to_tray;
        self.config.ui.show_notifications = self.show_notifications;
        self.config.ui.notification_on_complete = self.notify_on_complete;

        // Save config to disk.
        if let Err(e) = self.config.save() {
            tracing::error!(error = %e, "Failed to save config");
        } else {
            info!("Config saved");
        }

        // Save folder changes to database.
        self.save_folders();

        // Toggle autostart.
        if let Err(e) = crate::platform::set_autostart(self.autostart) {
            tracing::warn!(error = %e, "Failed to set autostart");
        }

        // Notify the running app of the new config so it can apply changes.
        if let Some(ref tx) = self.result_tx {
            let _ = tx.send(self.config.clone());
        }
    }
}

// ── Folder management (database) ────────────────────────────────────────────

impl SettingsApp {
    fn add_folder(&mut self, path: PathBuf) {
        let path_str = path.display().to_string();

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
    }

    fn save_folders(&self) {
        if let Ok(db) = open_db() {
            for folder in &self.folders {
                if let Err(e) = db.update_folder(
                    folder.id,
                    folder.label.as_deref(),
                    folder.enabled,
                    &folder.watch_mode,
                    folder.poll_interval_secs,
                    &folder.album_mode,
                    folder.album_name.as_deref(),
                    folder.include_patterns.as_deref(),
                    folder.exclude_patterns.as_deref(),
                    &folder.post_upload,
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
        let paths = match db.get_uploaded_paths_in_folder(&folder.path) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(folder = %folder.path, error = %e,
                    "Failed to query uploaded files for existing cleanup");
                return;
            }
        };

        info!(folder = %folder.path, count = paths.len(),
            "Applying post-upload action to already-uploaded files");

        let mut count = 0u32;
        for path_str in &paths {
            // Strip the \\?\ extended-path prefix that the file watcher adds.
            let clean = path_str.strip_prefix(r"\\?\").unwrap_or(path_str);
            let path = std::path::Path::new(clean);
            if !path.exists() {
                continue;
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

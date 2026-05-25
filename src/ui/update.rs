// Update dialog — shows available update info, downloads, and applies.
//
// Launched as `--window update --update-info <path-to-json>`.
// The UpdateInfo is serialized to a temp JSON file to avoid CLI escaping issues.
//
// Exit codes:
//   0 = Update applied, caller should relaunch
//   1 = User skipped or closed

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::updater::{self, UpdateInfo};

/// Run the update dialog. Blocks the calling thread.
///
/// Reads `UpdateInfo` from the JSON file at `info_path`, shows the dialog,
/// and exits with code 0 if the update was applied, 1 otherwise.
pub fn run_update_dialog(info_path: &str) {
    let info_json = match std::fs::read_to_string(info_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, path = info_path, "Failed to read update info file");
            std::process::exit(1);
        }
    };

    let update_info: UpdateInfo = match serde_json::from_str(&info_json) {
        Ok(i) => i,
        Err(e) => {
            tracing::error!(error = %e, "Failed to parse update info JSON");
            std::process::exit(1);
        }
    };

    // Clean up the temp file.
    let _ = std::fs::remove_file(info_path);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([460.0, 360.0])
            .with_title("ImmichSync Update")
            .with_resizable(false),
        ..Default::default()
    };

    let app = UpdateApp {
        info: update_info,
        state: DialogState::Prompt,
        progress: Arc::new(Mutex::new((0u64, 0u64))),
        error_msg: String::new(),
        downloaded_path: None,
    };

    let _ = eframe::run_native(
        "ImmichSync Update",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    );

    // If eframe exits without calling process::exit, user closed the window.
    std::process::exit(1);
}

#[derive(Debug, Clone, PartialEq)]
enum DialogState {
    Prompt,
    Downloading,
    ReadyToRestart,
    Error,
}

struct UpdateApp {
    info: UpdateInfo,
    state: DialogState,
    progress: Arc<Mutex<(u64, u64)>>,
    error_msg: String,
    #[allow(dead_code)]
    downloaded_path: Option<PathBuf>,
}

impl eframe::App for UpdateApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Check if the background download thread signalled completion.
        if self.state == DialogState::Downloading {
            self.check_download_state();
        }

        egui::CentralPanel::default().show(ctx, |ui| match self.state {
            DialogState::Prompt => self.show_prompt(ui, ctx),
            DialogState::Downloading => self.show_downloading(ui, ctx),
            DialogState::ReadyToRestart => self.show_ready(ui, ctx),
            DialogState::Error => self.show_error(ui, ctx),
        });
    }
}

impl UpdateApp {
    fn show_prompt(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.heading("Update Available");
        });

        ui.add_space(12.0);
        ui.label(format!(
            "A new version of ImmichSync is available: v{}",
            self.info.new_version
        ));
        ui.label(format!(
            "You are currently running v{}",
            self.info.current_version
        ));

        if self.info.size > 0 {
            ui.add_space(4.0);
            let size_mb = self.info.size as f64 / (1024.0 * 1024.0);
            ui.small(format!("Download size: {size_mb:.1} MB"));
        }

        // Changelog area.
        if !self.info.changelog.is_empty() {
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            ui.label("What's new:");
            egui::ScrollArea::vertical()
                .max_height(150.0)
                .show(ui, |ui| {
                    ui.label(&self.info.changelog);
                });
        }

        ui.add_space(12.0);

        ui.horizontal(|ui| {
            if ui.button("Update Now").clicked() {
                self.start_download(ctx);
            }
            if ui.button("Skip").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                std::process::exit(1);
            }
        });
    }

    fn show_downloading(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.heading("Downloading Update...");
        });

        ui.add_space(16.0);

        let (downloaded, total) = *self.progress.lock().unwrap();

        if total > 0 {
            let fraction = downloaded as f32 / total as f32;
            ui.add(egui::ProgressBar::new(fraction).show_percentage());

            ui.add_space(8.0);
            let dl_mb = downloaded as f64 / (1024.0 * 1024.0);
            let total_mb = total as f64 / (1024.0 * 1024.0);
            ui.label(format!("{dl_mb:.1} MB / {total_mb:.1} MB"));
        } else {
            let dl_mb = downloaded as f64 / (1024.0 * 1024.0);
            ui.add(egui::ProgressBar::new(0.0).text(format!("{dl_mb:.1} MB downloaded")));
        }

        // Keep repainting to show progress.
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    fn show_ready(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.heading("Update Ready");
        });

        ui.add_space(12.0);
        ui.label(format!(
            "ImmichSync v{} has been installed. Restart to use the new version.",
            self.info.new_version
        ));

        ui.add_space(16.0);

        ui.horizontal(|ui| {
            if ui.button("Restart Now").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                std::process::exit(0);
            }
            if ui.button("Later").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                std::process::exit(0);
            }
        });
    }

    fn show_error(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.heading("Update Failed");
        });

        ui.add_space(12.0);
        ui.colored_label(egui::Color32::RED, &self.error_msg);

        ui.add_space(16.0);
        if ui.button("Close").clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            std::process::exit(1);
        }
    }

    fn start_download(&mut self, ctx: &egui::Context) {
        self.state = DialogState::Downloading;

        let url = self.info.download_url.clone();
        let new_version = self.info.new_version.clone();
        let progress = self.progress.clone();
        let ctx = ctx.clone();

        // Spawn a thread for the blocking download.
        std::thread::spawn(move || {
            let progress_fn = {
                let progress = progress.clone();
                let ctx = ctx.clone();
                move |downloaded: u64, total: u64| {
                    *progress.lock().unwrap() = (downloaded, total);
                    ctx.request_repaint();
                }
            };

            match updater::download_update_blocking(&url, progress_fn) {
                Ok(path) => {
                    // Apply the update.
                    match updater::apply_update(&path, &new_version) {
                        Ok(()) => {
                            // Signal success via a special progress value.
                            // The main loop will detect this and switch to ReadyToRestart.
                            *progress.lock().unwrap() = (u64::MAX, u64::MAX);
                            ctx.request_repaint();
                        }
                        Err(e) => {
                            // Signal error.
                            *progress.lock().unwrap() = (u64::MAX - 1, 0);
                            // Store error in a way the UI can read — we'll use
                            // a sentinel and store the message via logging.
                            tracing::error!(error = %e, "Failed to apply update");
                            ctx.request_repaint();
                        }
                    }
                }
                Err(e) => {
                    *progress.lock().unwrap() = (u64::MAX - 1, 0);
                    tracing::error!(error = %e, "Failed to download update");
                    ctx.request_repaint();
                }
            }
        });
    }
}

impl UpdateApp {
    fn check_download_state(&mut self) {
        let (downloaded, total) = *self.progress.lock().unwrap();
        if downloaded == u64::MAX && total == u64::MAX {
            self.state = DialogState::ReadyToRestart;
        } else if downloaded == u64::MAX - 1 && total == 0 {
            self.state = DialogState::Error;
            if self.error_msg.is_empty() {
                self.error_msg =
                    "Download or installation failed. Check logs for details.".to_string();
            }
        }
    }
}

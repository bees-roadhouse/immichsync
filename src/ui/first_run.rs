// First-run setup wizard.
//
// 5-step egui dialog:
//   1. Welcome
//   2. Server Config + Test Connection
//   3. Folder Setup (pick watch folder)
//   4. Autostart (start with Windows?)
//   5. Done
//
// Returns `Some(Config)` if the user completes the wizard, `None` if cancelled.

use eframe::egui;

use crate::config::Config;

/// Run the first-run wizard. Blocks the calling thread.
///
/// Returns `Some(Config)` with user's settings if completed, `None` if cancelled.
pub fn run_first_run_wizard() -> Option<Config> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([480.0, 360.0])
            .with_title("ImmichSync — Setup")
            .with_resizable(false),
        ..Default::default()
    };

    let app = WizardApp {
        step: WizardStep::Welcome,
        config: Config::default(),
        server_url: String::new(),
        api_key: String::new(),
        test_status: String::new(),
        watch_folder: default_pictures_folder(),
        enable_autostart: true,
        result: None,
    };

    // eframe::run_native blocks until the window is closed.
    // We use a shared result to communicate back.
    let result = std::sync::Arc::new(std::sync::Mutex::new(None));
    let result_clone = result.clone();

    let app = WizardAppWrapper {
        inner: app,
        result: result_clone,
    };

    let _ = eframe::run_native(
        "ImmichSync Setup",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    );

    let guard = result.lock().unwrap();
    guard.clone()
}

fn default_pictures_folder() -> String {
    crate::platform::known_folders::get_pictures_folder()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

// ── Wizard steps ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WizardStep {
    Welcome,
    ServerConfig,
    FolderSetup,
    Autostart,
    Done,
}

struct WizardApp {
    step: WizardStep,
    config: Config,
    server_url: String,
    api_key: String,
    test_status: String,
    watch_folder: String,
    enable_autostart: bool,
    result: Option<Config>,
}

struct WizardAppWrapper {
    inner: WizardApp,
    result: std::sync::Arc<std::sync::Mutex<Option<Config>>>,
}

impl eframe::App for WizardAppWrapper {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.inner.update_wizard(ctx, frame);

        // If the wizard completed, store the result.
        if let Some(ref config) = self.inner.result {
            let mut guard = self.result.lock().unwrap();
            *guard = Some(config.clone());
        }
    }
}

impl WizardApp {
    fn update_wizard(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| match self.step {
            WizardStep::Welcome => self.show_welcome(ui, ctx),
            WizardStep::ServerConfig => self.show_server_config(ui, ctx),
            WizardStep::FolderSetup => self.show_folder_setup(ui, ctx),
            WizardStep::Autostart => self.show_autostart(ui, ctx),
            WizardStep::Done => self.show_done(ui, ctx),
        });
    }

    fn show_welcome(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.heading("Welcome to ImmichSync");
            ui.add_space(16.0);
            ui.label("ImmichSync watches your folders and automatically");
            ui.label("uploads photos and videos to your Immich server.");
            ui.add_space(24.0);
            ui.label("Let's get you set up in a few quick steps.");
            ui.add_space(32.0);

            ui.horizontal(|ui| {
                if ui.button("Get Started").clicked() {
                    self.step = WizardStep::ServerConfig;
                }
                if ui.button("Cancel").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }

    fn show_server_config(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Connect to Immich");
        ui.add_space(8.0);
        ui.label("Enter your Immich server URL and API key.");
        ui.add_space(12.0);

        egui::Grid::new("wizard_server_grid")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Server URL:");
                ui.add_sized(
                    [300.0, 20.0],
                    egui::TextEdit::singleline(&mut self.server_url)
                        .hint_text("https://photos.example.com"),
                );
                ui.end_row();

                ui.label("API Key:");
                ui.add_sized(
                    [300.0, 20.0],
                    egui::TextEdit::singleline(&mut self.api_key).password(true),
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

        ui.add_space(16.0);

        ui.horizontal(|ui| {
            if ui.button("Back").clicked() {
                self.step = WizardStep::Welcome;
            }
            let can_proceed = !self.server_url.is_empty() && !self.api_key.is_empty();
            ui.add_enabled_ui(can_proceed, |ui| {
                if ui.button("Next").clicked() {
                    self.config.server.url = self.server_url.clone();
                    self.config.server.api_key = self.api_key.clone();
                    self.step = WizardStep::FolderSetup;
                }
            });
            if ui.button("Cancel").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }

    fn show_folder_setup(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Watch Folder");
        ui.add_space(8.0);
        ui.label("Choose a folder to watch for new photos and videos.");
        ui.add_space(12.0);

        ui.horizontal(|ui| {
            ui.label("Folder:");
            ui.add_sized(
                [280.0, 20.0],
                egui::TextEdit::singleline(&mut self.watch_folder),
            );
            if ui.button("Browse…").clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    self.watch_folder = path.display().to_string();
                }
            }
        });

        ui.add_space(8.0);
        ui.label("You can add more folders later in Settings.");

        ui.add_space(24.0);

        ui.horizontal(|ui| {
            if ui.button("Back").clicked() {
                self.step = WizardStep::ServerConfig;
            }
            if ui.button("Next").clicked() {
                self.step = WizardStep::Autostart;
            }
            if ui.button("Cancel").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }

    fn show_autostart(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Start with Windows");
        ui.add_space(8.0);
        ui.label("ImmichSync can start automatically when you sign in.");
        ui.add_space(16.0);

        ui.checkbox(&mut self.enable_autostart, "Start ImmichSync with Windows");

        ui.add_space(8.0);
        ui.label("You can change this later in Settings.");

        ui.add_space(24.0);

        ui.horizontal(|ui| {
            if ui.button("Back").clicked() {
                self.step = WizardStep::FolderSetup;
            }
            if ui.button("Finish").clicked() {
                self.step = WizardStep::Done;
            }
            if ui.button("Cancel").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }

    fn show_done(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.heading("You're all set!");
            ui.add_space(16.0);
            ui.label("ImmichSync will now run in your system tray and");
            ui.label("automatically upload new photos and videos.");
            ui.add_space(32.0);

            if ui.button("Start ImmichSync").clicked() {
                // Apply autostart preference.
                self.config.ui.start_with_windows = self.enable_autostart;

                // Save config.
                if let Err(e) = self.config.save() {
                    tracing::error!(error = %e, "Failed to save config from wizard");
                }

                // Set or clear the autostart registry key.
                if let Err(e) = crate::platform::set_autostart(self.enable_autostart) {
                    tracing::warn!(error = %e, "Failed to set autostart from wizard");
                }

                // Add the watch folder to the database.
                if !self.watch_folder.is_empty() {
                    if let Ok(db) = crate::db::Database::open() {
                        if let Err(e) = db.add_folder(&self.watch_folder, None, false) {
                            tracing::warn!(error = %e, "Failed to add wizard folder");
                        }
                    }
                }

                self.result = Some(self.config.clone());
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }

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
}

// Install dialog — shown when the app is run from outside its install directory.
//
// Offers three checkboxes (autostart, desktop shortcut, start menu shortcut)
// and two buttons (Install, Run Portable).
//
// In update mode, the heading, button label, and description change to
// reflect that an existing installation is being updated.
//
// Exit codes (subprocess mode):
//   0 = Install/Update chosen — caller should relaunch from installed path
//   1 = Run Portable — caller should continue from current location

use std::sync::{Arc, Mutex};

use eframe::egui;
use tracing::{info, warn};

use crate::config::Config;

/// Result of the install dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallResult {
    /// User clicked Install/Update — exe was copied, shortcuts created.
    Installed,
    /// User clicked Run Portable or closed the window.
    Portable,
}

/// Run the install dialog in subprocess mode (calls `process::exit`).
///
/// Used when invoked via `--window install` or `--window install-update`.
pub fn run_install_dialog_subprocess(is_update: bool, old_version: Option<String>) {
    let install_dir = Config::install_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(unknown)".to_string());

    let title = if is_update {
        "Update ImmichSync"
    } else {
        "Install ImmichSync"
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 300.0])
            .with_title(title)
            .with_resizable(false)
            .with_always_on_top()
            .with_icon(crate::ui::window_icon::brand_icon_data()),
        ..Default::default()
    };

    let result = Arc::new(Mutex::new(InstallResult::Portable));

    let app = InstallApp {
        install_dir,
        is_update,
        old_version,
        new_version: crate::platform::install::running_version().to_string(),
        autostart: true,
        desktop_shortcut: true,
        start_menu_shortcut: true,
        status: String::new(),
        result: result.clone(),
    };

    info!("Opening install dialog (is_update={is_update})");
    match eframe::run_native(title, options, Box::new(|_cc| Ok(Box::new(app)))) {
        Ok(()) => {}
        Err(e) => {
            tracing::error!(error = %e, "eframe::run_native failed for install dialog");
        }
    }

    let r = *result.lock().unwrap();
    match r {
        InstallResult::Installed => std::process::exit(0),
        InstallResult::Portable => std::process::exit(1),
    }
}

struct InstallApp {
    install_dir: String,
    is_update: bool,
    old_version: Option<String>,
    new_version: String,
    autostart: bool,
    desktop_shortcut: bool,
    start_menu_shortcut: bool,
    status: String,
    result: Arc<Mutex<InstallResult>>,
}

impl eframe::App for InstallApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(16.0);

            ui.vertical_centered(|ui| {
                if self.is_update {
                    ui.heading("Update ImmichSync");
                } else {
                    ui.heading("Install ImmichSync");
                }
            });

            ui.add_space(12.0);

            if self.is_update {
                if let Some(ref old) = self.old_version.as_ref().filter(|v| *v != "unknown") {
                    ui.label(format!("Updating from v{old} to v{}", self.new_version));
                } else {
                    ui.label(format!("Updating to v{}", self.new_version));
                }
                ui.add_space(4.0);
            }

            ui.label("ImmichSync will be installed to:");
            ui.monospace(&self.install_dir);

            ui.add_space(16.0);

            if !self.is_update {
                ui.checkbox(&mut self.autostart, "Start with Windows");
            }
            ui.checkbox(&mut self.desktop_shortcut, "Create desktop shortcut");
            ui.checkbox(&mut self.start_menu_shortcut, "Add to Start Menu");

            if !self.status.is_empty() {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::RED, &self.status);
            }

            ui.add_space(16.0);

            let action_label = if self.is_update { "Update" } else { "Install" };

            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Run Portable").clicked() {
                        self.do_portable(ctx);
                    }

                    if ui.button(action_label).clicked() {
                        self.do_install(ctx);
                    }
                });
            });
        });
    }
}

impl InstallApp {
    fn do_install(&mut self, ctx: &egui::Context) {
        // For updates, kill any running instances first so we can overwrite the exe.
        if self.is_update {
            info!("Killing running ImmichSync instances before update");
            kill_running_instances();
            // Brief wait for process to fully exit and release file handles.
            std::thread::sleep(std::time::Duration::from_millis(500));
        }

        // Try to copy the exe. If it fails (file still locked), try the rename dance.
        if let Err(e) = crate::platform::install_exe() {
            warn!(error = %e, "install_exe failed, trying rename dance");

            if let Err(e2) = install_exe_rename_dance() {
                self.status = format!("Install failed: {e2}");
                return;
            }
        }

        let installed_path = match crate::platform::installed_exe_path() {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("Could not determine install path: {e}");
                return;
            }
        };

        // Create shortcuts.
        if self.desktop_shortcut {
            if let Err(e) = crate::platform::create_desktop_shortcut(
                &installed_path,
                "ImmichSync",
                "Sync photos and videos to Immich",
            ) {
                warn!(error = %e, "Failed to create desktop shortcut");
            }
        }

        if self.start_menu_shortcut {
            if let Err(e) = crate::platform::create_start_menu_shortcut(
                &installed_path,
                "ImmichSync",
                "Sync photos and videos to Immich",
            ) {
                warn!(error = %e, "Failed to create Start Menu shortcut");
            }
        }

        // Set autostart (preserve existing setting for updates).
        if !self.is_update {
            if let Err(e) = crate::platform::set_autostart(self.autostart) {
                warn!(error = %e, "Failed to set autostart");
            }
        }

        // Save config with portable_mode = false.
        if let Ok(mut config) = Config::load() {
            if !self.is_update {
                config.ui.start_with_windows = self.autostart;
            }
            config.ui.portable_mode = false;
            if let Err(e) = config.save() {
                warn!(error = %e, "Failed to save config from install dialog");
            }
        }

        // Register in Apps & Features (HKCU Uninstall block) so the user
        // can remove via Settings → Apps and `winget uninstall` works.
        // Idempotent ... updates rewrite the values with the new version.
        if let Err(e) = crate::platform::install::write_uninstall_registry() {
            warn!(error = %e, "Failed to write Uninstall registry block");
        }

        // Relaunch from installed path directly (the parent process was
        // killed by taskkill, so we can't rely on it to relaunch).
        info!(installed = %installed_path.display(), "Install complete, relaunching from installed path");
        let _ = std::process::Command::new(&installed_path).spawn();

        *self.result.lock().unwrap() = InstallResult::Installed;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn do_portable(&mut self, ctx: &egui::Context) {
        if let Ok(mut config) = Config::load() {
            config.ui.portable_mode = true;
            if let Err(e) = config.save() {
                warn!(error = %e, "Failed to save config from install dialog");
            }
        }

        *self.result.lock().unwrap() = InstallResult::Portable;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

/// Kill any running ImmichSync processes (except ourselves).
fn kill_running_instances() {
    let our_pid = std::process::id();

    // Use taskkill to terminate all immichsync.exe processes except ours.
    // /F = force, /FI = filter by PID not equal to ours, /IM = image name.
    let pid_filter = format!("PID ne {our_pid}");
    let output = std::process::Command::new("taskkill")
        .args(["/F", "/FI", &pid_filter, "/IM", "immichsync.exe"])
        .output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            info!(
                our_pid,
                stdout = %stdout.trim(),
                stderr = %stderr.trim(),
                "taskkill result"
            );
        }
        Err(e) => {
            warn!(error = %e, "Failed to run taskkill");
        }
    }
}

/// Fallback install: rename the locked installed exe to .old, then copy the new one in.
///
/// This works because Windows allows renaming a running exe (just not overwriting).
fn install_exe_rename_dance() -> Result<(), String> {
    let current = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dest =
        crate::platform::installed_exe_path().map_err(|e| format!("installed_exe_path: {e}"))?;
    let old = dest.with_extension("exe.old");

    // Remove leftover .old if it exists.
    if old.exists() {
        let _ = std::fs::remove_file(&old);
    }

    // Rename running exe → .old (works on Windows even while running).
    if dest.exists() {
        info!(
            from = %dest.display(),
            to = %old.display(),
            "Renaming installed exe to .old"
        );
        std::fs::rename(&dest, &old).map_err(|e| format!("rename to .old: {e}"))?;
    }

    // Copy new exe into place.
    info!(
        from = %current.display(),
        to = %dest.display(),
        "Copying new exe to install dir"
    );
    std::fs::copy(&current, &dest).map_err(|e| format!("copy: {e}"))?;

    // Write version.txt.
    if let Some(parent) = dest.parent() {
        let version_file = parent.join("version.txt");
        let _ = std::fs::write(&version_file, crate::platform::install::running_version());
    }

    info!("Install via rename dance succeeded");
    Ok(())
}

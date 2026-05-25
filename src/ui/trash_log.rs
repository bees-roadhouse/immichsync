// Trash log viewer — shows files currently in `.immichsync-trash/` directories
// across all watched folders.

use std::sync::Arc;

use eframe::egui;

use crate::db::DbStore;

/// Open the trash log viewer (blocking the calling thread).
pub fn show_trash_log(db: Arc<DbStore>) {
    let options = eframe::NativeOptions {
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            builder.with_any_thread(true);
        })),
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([700.0, 420.0])
            .with_title("ImmichSync — Trash"),
        ..Default::default()
    };

    let entries = load_trash_entries(&db);

    let app = TrashLogApp { entries, db };

    if let Err(e) = eframe::run_native(
        "ImmichSync Trash",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    ) {
        tracing::error!(error = %e, "Failed to open Trash Log window");
    }
}

// ── Trash entry ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct TrashEntry {
    /// Display name: relative path within the trash dir.
    relative_path: String,
    /// Which watched folder this trash belongs to.
    folder: String,
    /// File size in bytes.
    size: u64,
    /// When the file was moved to trash (from file modified time).
    trashed_at: String,
}

// ── App state ───────────────────────────────────────────────────────────────

struct TrashLogApp {
    entries: Vec<TrashEntry>,
    db: Arc<DbStore>,
}

impl eframe::App for TrashLogApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("trash_top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let count = self.entries.len();
                let total_bytes: u64 = self.entries.iter().map(|e| e.size).sum();
                ui.label(format!(
                    "{} file{} — {}",
                    count,
                    if count == 1 { "" } else { "s" },
                    format_size(total_bytes),
                ));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Refresh").clicked() {
                        self.entries = load_trash_entries(&self.db);
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.entries.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("Trash is empty.");
                });
                return;
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("trash_grid")
                    .num_columns(4)
                    .striped(true)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        // Header
                        ui.strong("File");
                        ui.strong("Folder");
                        ui.strong("Size");
                        ui.strong("Trashed");
                        ui.end_row();

                        for entry in &self.entries {
                            ui.label(&entry.relative_path);
                            ui.label(&entry.folder);
                            ui.label(format_size(entry.size));
                            ui.label(&entry.trashed_at);
                            ui.end_row();
                        }
                    });
            });
        });
    }
}

// ── Data loading ────────────────────────────────────────────────────────────

fn load_trash_entries(db: &Arc<DbStore>) -> Vec<TrashEntry> {
    let guard = db.inner().lock().unwrap();
    let folders = match guard.get_folders() {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to load folders for trash log");
            return Vec::new();
        }
    };
    drop(guard);

    let mut entries = Vec::new();

    for folder in &folders {
        let root = std::path::Path::new(&folder.path);
        let trash_root = root.join(crate::upload::worker::TRASH_DIR_NAME);
        if !trash_root.exists() {
            continue;
        }

        // Recursively walk the trash directory.
        walk_trash(&trash_root, &trash_root, &folder.path, &mut entries);
    }

    // Sort by trashed_at descending (most recent first).
    entries.sort_by(|a, b| b.trashed_at.cmp(&a.trashed_at));
    entries
}

fn walk_trash(
    dir: &std::path::Path,
    trash_root: &std::path::Path,
    folder_path: &str,
    entries: &mut Vec<TrashEntry>,
) {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_trash(&path, trash_root, folder_path, entries);
        } else if path.is_file() {
            let relative = path
                .strip_prefix(trash_root)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| {
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });

            let (size, trashed_at) = match path.metadata() {
                Ok(meta) => {
                    let size = meta.len();
                    let modified = meta
                        .modified()
                        .ok()
                        .map(|t| {
                            let dt: chrono::DateTime<chrono::Local> = t.into();
                            dt.format("%Y-%m-%d %H:%M").to_string()
                        })
                        .unwrap_or_default();
                    (size, modified)
                }
                Err(_) => (0, String::new()),
            };

            // Shorten folder path for display.
            let folder_display = std::path::Path::new(folder_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| folder_path.to_string());

            entries.push(TrashEntry {
                relative_path: relative,
                folder: folder_display,
                size,
                trashed_at,
            });
        }
    }
}

// ── Formatting ──────────────────────────────────────────────────────────────

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

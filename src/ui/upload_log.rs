// Upload log viewer — shows recent upload history in a scrollable table.

use std::sync::Arc;

use eframe::egui;

use crate::db::DbStore;

/// Open the upload log viewer (blocking the calling thread).
///
/// Safe to call from any thread — uses `with_any_thread(true)` so eframe can
/// create its event loop off the main thread.
pub fn show_upload_log(db: Arc<DbStore>) {
    let options = eframe::NativeOptions {
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            builder.with_any_thread(true);
        })),
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([620.0, 420.0])
            .with_title("ImmichSync — Upload Log")
            .with_icon(crate::ui::window_icon::brand_icon_data()),
        ..Default::default()
    };

    let entries = load_entries(&db);

    let app = UploadLogApp {
        entries,
        filter: LogFilter::All,
        db,
    };

    if let Err(e) = eframe::run_native(
        "ImmichSync Upload Log",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    ) {
        tracing::error!(error = %e, "Failed to open Upload Log window");
    }
}

// ── Filter ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogFilter {
    All,
    Completed,
    Failed,
}

// ── Log entry (unified view of queue + uploaded) ────────────────────────────

#[derive(Debug, Clone)]
struct LogEntry {
    filename: String,
    timestamp: String,
    status: String,
    error: Option<String>,
}

// ── App state ───────────────────────────────────────────────────────────────

struct UploadLogApp {
    entries: Vec<LogEntry>,
    filter: LogFilter,
    db: Arc<DbStore>,
}

impl eframe::App for UploadLogApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("log_filter_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(self.filter == LogFilter::All, "All")
                    .clicked()
                {
                    self.filter = LogFilter::All;
                }
                if ui
                    .selectable_label(self.filter == LogFilter::Completed, "Completed")
                    .clicked()
                {
                    self.filter = LogFilter::Completed;
                }
                if ui
                    .selectable_label(self.filter == LogFilter::Failed, "Failed")
                    .clicked()
                {
                    self.filter = LogFilter::Failed;
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Refresh").clicked() {
                        self.entries = load_entries(&self.db);
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let filtered: Vec<&LogEntry> = self
                .entries
                .iter()
                .filter(|e| match self.filter {
                    LogFilter::All => true,
                    LogFilter::Completed => e.status == "completed",
                    LogFilter::Failed => e.status == "failed",
                })
                .collect();

            if filtered.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No entries to display.");
                });
                return;
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("log_grid")
                    .num_columns(4)
                    .striped(true)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        // Header
                        ui.strong("File");
                        ui.strong("Time");
                        ui.strong("Status");
                        ui.strong("Error");
                        ui.end_row();

                        for entry in &filtered {
                            ui.label(&entry.filename);
                            ui.label(&entry.timestamp);
                            ui.label(&entry.status);
                            ui.label(entry.error.as_deref().unwrap_or(""));
                            ui.end_row();
                        }
                    });
            });
        });
    }
}

// ── Data loading ────────────────────────────────────────────────────────────

fn load_entries(db: &Arc<DbStore>) -> Vec<LogEntry> {
    let guard = db.inner().lock().unwrap();

    let mut entries = Vec::new();

    // Load recent queue entries (all statuses).
    if let Ok(queue_entries) = guard.get_recent_queue_entries(200) {
        for e in queue_entries {
            let filename = std::path::Path::new(&e.file_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| e.file_path.clone());

            let timestamp = e
                .completed_at
                .as_deref()
                .or(Some(e.queued_at.as_str()))
                .unwrap_or("")
                .to_string();

            entries.push(LogEntry {
                filename,
                timestamp,
                status: e.status.as_str().to_string(),
                error: e.error_message,
            });
        }
    }

    entries
}

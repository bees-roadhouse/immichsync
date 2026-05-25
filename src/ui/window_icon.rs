// Shared helper for stamping the brand shutter icon onto every eframe window
// (settings, install dialog, about, log viewers, update dialog, first-run).
//
// The same 32x32 RGBA8 blob the tray uses for the static idle frame doubles as
// the window icon. eframe `ViewportBuilder::with_icon` takes an `egui::IconData`
// containing width/height + RGBA bytes ... no separate decode needed.

use eframe::egui;

use crate::ui::tray::{brand_icon_dimensions, brand_icon_rgba};

/// Build an `egui::IconData` from the embedded brand-icon bytes.
///
/// Always succeeds (the data is compiled in and validated at compile time
/// via the `_` const assertion in `tray.rs`).
pub fn brand_icon_data() -> egui::IconData {
    let (w, h) = brand_icon_dimensions();
    egui::IconData {
        rgba: brand_icon_rgba().to_vec(),
        width: w,
        height: h,
    }
}

/// Apply the brand icon to a `ViewportBuilder`. Use it at the end of any
/// `ViewportBuilder::default().with_*()` chain inside `eframe::NativeOptions`.
pub fn with_brand_icon(vb: egui::ViewportBuilder) -> egui::ViewportBuilder {
    vb.with_icon(brand_icon_data())
}

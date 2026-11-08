//! Starts the msQuant window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
use eframe::egui;
use msquant::{app::App, icon};

/// Opens the main window, with the application icon if it can be decoded.
fn main() -> eframe::Result<()> {
    let mut vp = egui::ViewportBuilder::default().with_inner_size([1500.0, 900.0]).with_title("msQuant");
    if let Some((rgba, width, height)) = icon::decode_ico(include_bytes!("../img/msQuant-ico.ico")) {
        vp = vp.with_icon(egui::IconData { rgba, width, height });
    }
    let opts = eframe::NativeOptions { viewport: vp, ..Default::default() };
    eframe::run_native("msQuant", opts, Box::new(|_cc| Ok(Box::new(App::new()))))
}

mod audio;
mod codec;
mod config;
mod crypto;
mod rtp;
mod sip;
mod theme;
mod ui;

use eframe::egui;
use ui::RustSipApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RustSIP — Softphone Nativo Linux")
            .with_app_id("rustsip")
            .with_inner_size([380.0, 640.0])
            .with_min_inner_size([360.0, 560.0]),
        ..Default::default()
    };

    eframe::run_native(
        "RustSIP",
        options,
        Box::new(|cc| Ok(Box::new(RustSipApp::new(cc)))),
    )
}

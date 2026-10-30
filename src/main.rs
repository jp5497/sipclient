mod audio;
mod codec;
mod config;
mod crypto;
mod daemon;
mod rtp;
mod sip;
mod theme;
mod ui;

use eframe::egui;
use ui::RustSipApp;

fn main() -> eframe::Result<()> {
    // Dispatch por argumento (sem clap): `rustsip daemon` roda o modo daemon
    // headless com bandeja; qualquer outro caso (default) roda EXATAMENTE a
    // janela de hoje, inalterada.
    if std::env::args().nth(1).as_deref() == Some("daemon") {
        if let Err(e) = daemon::run() {
            eprintln!("rustsip daemon: erro fatal: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

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

mod audio;
mod codec;
mod config;
mod crypto;
mod daemon;
mod ipc;
mod remote;
mod rtp;
mod sip;
mod theme;
mod ui;

use eframe::egui;
use ui::RustSipApp;

fn main() -> eframe::Result<()> {
    // Dispatch por argumento (sem clap): `rustsip daemon` roda o modo daemon
    // headless com bandeja; `rustsip --remote` (ou `remote`) abre a janela em
    // modo CLIENTE lendo o estado do daemon por socket (F1, read-only); qualquer
    // outro caso (default) roda EXATAMENTE a janela de hoje, inalterada.
    // args_os (não args): std::env::args() faz PANIC se algum argumento não for
    // UTF-8. O dispatch só compara o 1º arg com literais ASCII, então comparar
    // o OsStr cru evita o panic de borda sem mudar o comportamento.
    let arg1 = std::env::args_os().nth(1);
    if arg1.as_deref() == Some(std::ffi::OsStr::new("daemon")) {
        if let Err(e) = daemon::run() {
            eprintln!("rustsip daemon: erro fatal: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    if matches!(
        arg1.as_deref(),
        Some(a) if a == std::ffi::OsStr::new("--remote") || a == std::ffi::OsStr::new("remote")
    ) {
        return remote::run_remote_window();
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

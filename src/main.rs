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

fn main() -> eframe::Result<()> {
    // Dispatch por argumento (sem clap):
    //   `rustsip daemon`           -> modo daemon headless com bandeja (F0);
    //   `rustsip` (sem args)       -> F2: garante o daemon vivo e abre a JANELA
    //                                 CLIENTE que fala com ele por IPC. A janela
    //                                 não roda mais SIP/áudio in-process (fecha o
    //                                 achado A1);
    //   `rustsip --remote`/`remote`-> alias explícito do MESMO caminho da janela
    //                                 cliente (o default o absorveu; mantido por
    //                                 conveniência/scripts).
    //
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

    // Default E alias --remote/remote: a janela cliente do daemon. O
    // RustSipApp::new in-process foi REMOVIDO (código morto) — a UI é sempre
    // cliente IPC agora.
    remote::run_remote_window()
}

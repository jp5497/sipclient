//! Modo `--remote` (F1 da refatoração daemon+IPC, ADR-0001): janela CLIENTE
//! que LÊ o estado do daemon por socket Unix, sem núcleo SIP/áudio local.
//!
//! É um 3º caminho TEMPORÁRIO de validação do IPC read-only. A refatoração
//! completa da UI para falar IPC (comandos inclusive) é a F2; aqui o objetivo é
//! só PROVAR a leitura: a janela mostra status de registro, logs, VU e uma
//! chamada entrando, tudo vindo do socket.
//!
//! Estratégia de MENOR mudança no ui.rs: em vez de refatorar cada leitura de
//! estado, alimentamos os MESMOS tipos de estado compartilhado que a janela já
//! lê hoje (`SipSharedState`, `AudioSharedState`) a partir das `ServerMsg`. O
//! `RustSipApp` é construído com um `SipController` INERTE (sem supervisor, sem
//! socket, sem REGISTER) e um `AudioEngine` que só ENVOLVE o estado espelho
//! (sem abrir dispositivo). Assim nenhum SipController/AudioEngine "dono" é
//! criado localmente — o daemon é o dono — e a UI renderiza sem alteração.

use crate::audio::AudioSharedState;
use crate::config::AppConfig;
use crate::ipc::{self, ClientMsg, ServerMsg, PROTO_VERSION};
use crate::rtp::RtpStats;
use crate::sip::SipSharedState;
use crate::ui::RustSipApp;
use eframe::egui;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// Caminho do socket do daemon (espelha `daemon::runtime_dir`).
fn socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("rustsip"))
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config").join("rustsip")
        });
    base.join("daemon.sock")
}

/// mtime (epoch secs) do próprio exe, enviado no Hello para o daemon detectar
/// binário obsoleto.
fn exe_mtime() -> u64 {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Config exibido pela janela remota: na F1 é preenchido pelo Welcome. A UI já
/// lê `self.config` para contatos/histórico/conta; o espelho vive num
/// Arc<Mutex<>> que a thread de IPC atualiza e a janela copia por frame.
pub type RemoteConfig = Arc<Mutex<AppConfig>>;

/// Handle de ESCRITA do cliente para o daemon. A janela (F2) envia `ClientMsg`
/// de comando por aqui; a leitura dos `ServerMsg` fica na thread de IPC. O
/// `UnixStream` é full-duplex, então um `try_clone` dá o half de escrita para a
/// UI enquanto a thread lê do outro clone.
///
/// Todas as ações da UI passam por `send()`. Uma falha de escrita (daemon caiu)
/// é guardada em `broken` para a UI poder refletir "desconectado" sem travar.
/// NUNCA logar o frame (SaveAccount carrega a senha) — só a causa, sem corpo.
#[derive(Clone)]
pub struct DaemonClient {
    writer: Arc<Mutex<UnixStream>>,
    broken: Arc<std::sync::atomic::AtomicBool>,
}

impl DaemonClient {
    fn new(writer: UnixStream) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
            broken: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Envia um comando ao daemon. Best-effort: um erro de escrita marca o
    /// cliente como quebrado e é silenciado (a thread de leitura também detecta
    /// o EOF e para de atualizar o espelho). Nunca propaga/loga o conteúdo.
    pub fn send(&self, msg: ClientMsg) {
        if self.broken.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut w) = self.writer.lock() {
            if ipc::write_frame(&mut *w, &msg).is_err() {
                self.broken.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Garante um daemon vivo e devolve um `UnixStream` conectado ao socket (ADR
/// §1 "garante o daemon"). Tenta conectar; se não houver ninguém ouvindo,
/// lança `rustsip daemon` (processo separado, dono do núcleo SIP/áudio) e faz
/// retry de connect a cada 100ms por até 3s.
fn ensure_daemon() -> std::io::Result<UnixStream> {
    let path = socket_path();
    // Caminho feliz: daemon já de pé.
    if let Ok(s) = UnixStream::connect(&path) {
        return Ok(s);
    }
    // Lança o daemon como processo separado (mesmo executável, arg "daemon").
    // O daemon redireciona o próprio stdio para ~/.config/rustsip/daemon.log;
    // o lock de instância única garante que um 2º spawn simultâneo sai sozinho.
    let exe = std::env::current_exe()?;
    let _ = std::process::Command::new(exe).arg("daemon").spawn()?;
    // Retry de connect: 100ms * 30 = 3s.
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(s);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "daemon não respondeu no socket {} após 3s",
            path.display()
        ),
    ))
}

/// Ponto de entrada do MODO DEFAULT da F2 (`rustsip` sem args) e do alias
/// `--remote`: garante o daemon vivo e sobe a janela JÁ como CLIENTE IPC. A
/// janela não roda mais SIP/áudio in-process — lê o estado espelho e envia
/// comandos ao daemon (fecha o achado A1: a UI não dispara áudio/save local).
pub fn run_remote_window() -> eframe::Result<()> {
    let path = socket_path();
    let stream = match ensure_daemon() {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "rustsip: não foi possível garantir o daemon em {} ({e}).\n\
                 Tente iniciar manualmente com `rustsip daemon` e verifique \
                 ~/.config/rustsip/daemon.log.",
                path.display()
            );
            std::process::exit(1);
        }
    };

    // Estado ESPELHO que a UI lê (os mesmos tipos de hoje).
    let sip_state = SipSharedState::new(RtpStats::new());
    let audio_state = AudioSharedState::new();
    let config: RemoteConfig = Arc::new(Mutex::new(AppConfig::default()));

    // Half de ESCRITA para a UI (comandos) e half de LEITURA para a thread de
    // IPC (frames). try_clone dá dois handles do MESMO socket full-duplex.
    let write_half = stream
        .try_clone()
        .expect("clonar UnixStream para escrita do cliente");
    let client = DaemonClient::new(write_half);

    // Thread dedicada BLOQUEANTE para o socket (std, fora do tokio): envia o
    // Hello e aplica cada ServerMsg no espelho. A janela (egui) roda na main.
    {
        let sip_state = sip_state.clone();
        let audio_state = audio_state.clone();
        let config = config.clone();
        std::thread::Builder::new()
            .name("rustsip-ipc-client".to_string())
            .spawn(move || {
                if let Err(e) = ipc_client_loop(stream, sip_state, audio_state, config) {
                    // NUNCA logar o frame (pode conter a senha no Welcome); só a causa.
                    eprintln!("rustsip: conexão IPC encerrada: {e}");
                }
            })
            .expect("criar thread cliente IPC");
    }

    // Sobe a janela em modo cliente (SipController inerte + AudioEngine espelho
    // + DaemonClient para enviar comandos).
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
        Box::new(move |cc| {
            Ok(Box::new(RustSipApp::new_remote(
                cc,
                sip_state,
                audio_state,
                config,
                client,
            )))
        }),
    )
}

/// Loop de leitura do socket: Hello -> consome ServerMsg até EOF/erro.
fn ipc_client_loop(
    stream: UnixStream,
    sip_state: Arc<SipSharedState>,
    audio_state: Arc<AudioSharedState>,
    config: RemoteConfig,
) -> anyhow::Result<()> {
    // Metade de escrita (Hello) e leitura (frames). try_clone dá dois handles do
    // mesmo socket.
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    // 1) Handshake.
    ipc::write_frame(
        &mut writer,
        &ClientMsg::Hello {
            proto: PROTO_VERSION,
            exe_mtime: exe_mtime(),
        },
    )?;

    // 2) Consome frames e aplica no espelho.
    loop {
        let msg: Option<ServerMsg> = ipc::read_frame(&mut reader)?;
        let Some(msg) = msg else {
            return Ok(()); // EOF limpo (daemon encerrou)
        };
        apply_server_msg(msg, &sip_state, &audio_state, &config);
    }
}

/// Aplica uma ServerMsg no estado espelho que a UI lê.
fn apply_server_msg(
    msg: ServerMsg,
    sip_state: &Arc<SipSharedState>,
    audio_state: &Arc<AudioSharedState>,
    config: &RemoteConfig,
) {
    match msg {
        ServerMsg::Welcome {
            account,
            audio,
            contacts,
            history,
            password_needs_reentry,
            reg,
            call,
            public_addr,
            flags,
            log_seq,
            ..
        } => {
            if let Ok(mut cfg) = config.lock() {
                cfg.account = account;
                cfg.audio = audio;
                cfg.contacts = contacts;
                cfg.history = history;
                cfg.password_needs_reentry = password_needs_reentry;
            }
            sip_state.set_reg_status(reg);
            sip_state.set_call_phase(call.into_call_phase());
            if let Ok(mut pa) = sip_state.detected_public_addr.lock() {
                *pa = public_addr;
            }
            apply_flags(audio_state, flags);
            // Alinha o seq de log do espelho ao snapshot do daemon.
            sip_state.log_seq.store(log_seq, Ordering::Relaxed);
        }
        ServerMsg::LogAppend { entry, .. } => {
            // add_log do espelho usa um &'static str; reconstruímos a entrada.
            let e = entry.into_log_entry();
            if let Ok(mut logs) = sip_state.logs.lock() {
                if logs.len() >= 250 {
                    logs.pop_front();
                }
                logs.push_back(e);
            }
            sip_state.log_seq.fetch_add(1, Ordering::Relaxed);
        }
        ServerMsg::Reg(reg) => sip_state.set_reg_status(reg),
        ServerMsg::Call(call) => sip_state.set_call_phase(call.into_call_phase()),
        ServerMsg::Flags(flags) => apply_flags(audio_state, flags),
        ServerMsg::PublicAddr(pa) => {
            if let Ok(mut slot) = sip_state.detected_public_addr.lock() {
                *slot = pa;
            }
        }
        ServerMsg::Hot { mic, spk, tx, rx } => {
            audio_state.set_mic_level(mic);
            audio_state.set_speaker_level(spk);
            sip_state.rtp_stats.tx_packets.store(tx, Ordering::Relaxed);
            sip_state.rtp_stats.rx_packets.store(rx, Ordering::Relaxed);
        }
        // --- Frames de SEÇÃO (F2): atualizam o espelho por seção ---
        ServerMsg::Contacts(contacts) => {
            if let Ok(mut cfg) = config.lock() {
                cfg.contacts = contacts;
            }
        }
        ServerMsg::History(history) => {
            if let Ok(mut cfg) = config.lock() {
                cfg.history = history;
            }
        }
        ServerMsg::AudioCfg(audio) => {
            // Espelha gain/volume nos átomos (o footer/VU/sliders leem daqui) e
            // atualiza a seção de áudio do config (devices/valores dos sliders).
            audio_state.set_mic_gain(audio.mic_gain);
            audio_state.set_speaker_volume(audio.speaker_volume);
            if let Ok(mut cfg) = config.lock() {
                cfg.audio = audio;
            }
        }
        ServerMsg::Account(account) => {
            // L6: aplica o formulário de conta SÓ quando este frame chega
            // (resposta a SaveAccount) — nunca a cada tick, para não sobrescrever
            // o que o usuário está digitando.
            if let Ok(mut cfg) = config.lock() {
                cfg.account = account;
            }
        }
        // F3: tray "Mostrar" / shutdown — tratados numa fase futura.
        ServerMsg::Raise | ServerMsg::Shutdown => {}
    }
}

/// Aplica FlagsWire nos átomos do estado de áudio espelho.
fn apply_flags(audio: &Arc<AudioSharedState>, flags: crate::ipc::FlagsWire) {
    audio.muted.store(flags.muted, Ordering::Relaxed);
    audio.on_hold.store(flags.on_hold, Ordering::Relaxed);
    audio.mode.store(flags.mode, Ordering::Relaxed);
}

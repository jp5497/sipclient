//! Modo DAEMON headless do RustSIP (`rustsip daemon`).
//!
//! Papel introduzido na F0 da refatoração daemon+IPC (ADR-0001). O daemon roda
//! o núcleo SIP (registro no PBX, chamadas) SEM janela, expondo um ícone na
//! bandeja do sistema via `ksni` (StatusNotifierItem puro sobre D-Bus/zbus —
//! não puxa GTK). É o DONO do config: é o único processo que escreve em
//! `config.save()` (drena o histórico de chamadas periodicamente).
//!
//! F0 NÃO implementa IPC/socket (isso é a F1) nem muda o comportamento do
//! `rustsip` default (janela eframe em processo único). O menu "Mostrar" por
//! ora só loga a intenção; o encerramento ("Sair", SIGTERM/SIGINT) é gracioso:
//! derruba a chamada ativa, faz un-REGISTER com timeout e para o áudio.

use crate::audio::{AudioEngine, AudioSharedState, MODE_LOOPBACK_TEST};
use crate::config::AppConfig;
use crate::ipc::{
    self, CallPhaseWire, ClientMsg, FlagsWire, ServerMsg, SipLogEntryWire, MAX_FRAME_BYTES,
    PROTO_VERSION,
};
use crate::sip::{CallPhase, RegistrationStatus, SipCommand, SipController, SipSharedState};
use anyhow::{Context, Result};
use chrono::Local;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

/// ARGB32 (network byte order) 64x64 do ícone de bandeja — glifo de telefone
/// branco com contorno preto (contraste dual claro/escuro). Gerado por
/// `assets/gen_tray_argb.rs` a partir da lógica de `build_icon_rgba` da branch
/// feat/system-tray. Embutido no binário para o daemon não depender de arquivo.
const TRAY_ICON_ARGB: &[u8] = include_bytes!("../assets/tray-64.argb");
const TRAY_ICON_SIZE: i32 = 64;

/// Evento da bandeja para o núcleo do daemon. O clique esquerdo (`activate`) e
/// o item "Mostrar" NÃO chamam `process::exit` direto — mandam `Show` por este
/// canal; só o "Sair" sinaliza `Quit`, que dispara o encerramento gracioso no
/// loop principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonEvt {
    Show,
    Quit,
}

/// Implementação do StatusNotifierItem. É `Send + 'static` (exigido por
/// `ksni::Tray`): carrega só o sender do canal e flags copiáveis.
struct RustSipTray {
    /// true enquanto houver chamada ativa — reservado para F2+ (variar título/
    /// ícone por estado). Em F0 só alimenta o `title()`.
    in_call: bool,
    evt_tx: tokio::sync::mpsc::UnboundedSender<DaemonEvt>,
}

impl ksni::Tray for RustSipTray {
    fn id(&self) -> String {
        "rustsip".into()
    }

    fn title(&self) -> String {
        if self.in_call {
            "RustSIP — em chamada".into()
        } else {
            "RustSIP".into()
        }
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Communications
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![ksni::Icon {
            width: TRAY_ICON_SIZE,
            height: TRAY_ICON_SIZE,
            data: TRAY_ICON_ARGB.to_vec(),
        }]
    }

    /// Clique esquerdo: pede para mostrar a janela (F1 abre o socket; em F0 só
    /// loga). Nunca encerra o processo aqui.
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.evt_tx.send(DaemonEvt::Show);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{StandardItem, MenuItem};
        vec![
            StandardItem {
                label: "Mostrar RustSIP".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.evt_tx.send(DaemonEvt::Show);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Sair".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.evt_tx.send(DaemonEvt::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Diretório de runtime do daemon: `$XDG_RUNTIME_DIR/rustsip` quando disponível
/// (tmpfs por-sessão, ideal para lock/socket), senão `~/.config/rustsip`. Cria
/// o dir 0700. Retorna o caminho garantido-existente.
fn runtime_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("rustsip"))
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config").join("rustsip")
        });

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        use std::os::unix::fs::PermissionsExt;
        if !base.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&base)
                .with_context(|| format!("criar runtime dir {}", base.display()))?;
        } else {
            // N3: não engole a falha de chmod — um runtime dir que ficou com
            // permissão frouxa é relevante (o socket IPC é defendido por uid +
            // chmod 0600, mas o dir 0700 é defesa em profundidade).
            if let Err(e) =
                std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            {
                eprintln!(
                    "[daemon] aviso: chmod 0700 em {} falhou: {e}",
                    base.display()
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&base)
            .with_context(|| format!("criar runtime dir {}", base.display()))?;
    }
    Ok(base)
}

/// Caminho do log do daemon (sempre em ~/.config/rustsip, persistente entre
/// sessões, ao contrário do runtime dir que é tmpfs).
fn daemon_log_path() -> PathBuf {
    AppConfig::config_path()
        .parent()
        .map(|p| p.join("daemon.log"))
        .unwrap_or_else(|| PathBuf::from("daemon.log"))
}

/// Redireciona stdout/stderr do processo para `~/.config/rustsip/daemon.log`
/// via dup2, de modo que `println!`/`eprintln!` (e panics) caiam no arquivo.
#[cfg(unix)]
fn redirect_stdio_to_log(path: &std::path::Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("abrir log {}", path.display()))?;
    let fd = f.as_raw_fd();
    // SAFETY: fd é válido (acabou de ser aberto); dup2 em 1/2 é a forma padrão
    // de redirecionar os descritores do processo. `f` é esquecido para o fd
    // ficar vivo pelo resto do processo.
    unsafe {
        if libc_dup2(fd, 1) < 0 || libc_dup2(fd, 2) < 0 {
            anyhow::bail!("dup2 falhou ao redirecionar stdio");
        }
    }
    std::mem::forget(f);
    Ok(())
}

// dup2 sem crate libc: syscall direta não é portável, mas o projeto é
// Linux-only (PipeWire/pactl). Usamos a libc do sistema via extern "C" — já
// linkada pelo std, sem adicionar dependência de crate.
#[cfg(unix)]
extern "C" {
    #[link_name = "dup2"]
    fn libc_dup2(oldfd: i32, newfd: i32) -> i32;
}

/// Carimbo de tempo para as linhas de log do daemon.
fn ts() -> String {
    Local::now().format("%d/%m/%Y %H:%M:%S%.3f").to_string()
}

/// Janela mínima entre dois relances de janela disparados pelo daemon. Protege
/// contra duplo-clique no tray e contra re-relance em loop no auto-show de
/// chamada entrando (o tray pode reemitir `activate`; o `Incoming` persiste por
/// vários ticks). Um relance dentro desta janela é IGNORADO.
const RELAUNCH_GUARD: Duration = Duration::from_secs(5);

/// F3 (L4): decisão de o que fazer quando o daemon quer mostrar a janela
/// (tray "Mostrar" ou auto-show de chamada entrando). Extraída numa função PURA
/// para ter um teste que quebra se a lógica de relance quebrar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelaunchAction {
    /// Há janela viva: só pede Raise (traz à frente) por broadcast.
    Raise,
    /// Sem janela e fora da guarda anti-loop: relança a janela.
    Spawn,
    /// Sem janela mas um relance recente ainda está dentro da guarda: ignora
    /// (evita fork-bomb no `Incoming` que persiste por vários ticks e no
    /// duplo-clique do tray).
    Ignore,
}

/// Regra PURA de relance: `count>0` → Raise; `count==0` e sem relance recente →
/// Spawn; `count==0` mas o último relance foi há menos que `guard` → Ignore.
fn relaunch_decision(
    client_count: usize,
    last_relaunch: Option<Instant>,
    now: Instant,
    guard: Duration,
) -> RelaunchAction {
    if client_count > 0 {
        RelaunchAction::Raise
    } else if last_relaunch.is_none_or(|t| now.duration_since(t) >= guard) {
        RelaunchAction::Spawn
    } else {
        RelaunchAction::Ignore
    }
}

/// Relança a JANELA CLIENTE como processo FILHO do daemon. É o mesmo binário
/// SEM argumento (o default de `main` = "garante daemon + abre janela cliente";
/// o daemon já está de pé, então só a janela sobe e conecta ao socket).
///
/// Hardening (espelha o spawn do daemon na F2): `stdin(null)` solta a entrada
/// controladora e o `wait()` numa thread detached faz o reap para a janela não
/// virar zumbi se morrer antes do daemon. A janela PODE herdar o grupo de
/// processos do daemon (é filha dele, não o contrário), então NÃO forçamos
/// `process_group(0)` aqui — o que importa é não virar zumbi e não prender stdin.
///
/// `current_exe()` pode falhar (ex. o binário foi removido/substituído); nesse
/// caso NÃO fazemos unwrap — logamos e desistimos do relance (o usuário ainda
/// tem o tray). Retorna `true` se o spawn foi disparado.
fn spawn_window() -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            println!("[{}] [daemon] relance da janela abortado: current_exe falhou ({e})", ts());
            return false;
        }
    };
    match std::process::Command::new(exe)
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => {
            std::thread::Builder::new()
                .name("rustsip-window-reaper".to_string())
                .spawn(move || {
                    let mut child = child;
                    let _ = child.wait();
                })
                .ok();
            println!("[{}] [daemon] janela cliente relançada", ts());
            true
        }
        Err(e) => {
            println!("[{}] [daemon] relance da janela falhou ({e})", ts());
            false
        }
    }
}

/// Ponto de entrada do modo daemon, chamado pelo `main` quando o 1º arg é
/// "daemon". Síncrono: monta o runtime tokio próprio e bloqueia nele.
pub fn run() -> Result<()> {
    let log_path = daemon_log_path();
    #[cfg(unix)]
    {
        if let Err(e) = redirect_stdio_to_log(&log_path) {
            eprintln!("[daemon] aviso: não foi possível redirecionar stdio: {e}");
        }
    }
    println!("[{}] [daemon] iniciando (log em {})", ts(), log_path.display());

    // INSTÂNCIA ÚNICA: segura um lock exclusivo no lockfile. Quem perde sai 0.
    let rt_dir = runtime_dir()?;
    let lock_path = rt_dir.join("daemon.lock");
    let lock_file = File::create(&lock_path)
        .with_context(|| format!("abrir lockfile {}", lock_path.display()))?;
    match lock_file.try_lock() {
        Ok(()) => {
            println!("[{}] [daemon] lock adquirido ({})", ts(), lock_path.display());
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            println!(
                "[{}] [daemon] outra instância já roda (lock ocupado); saindo",
                ts()
            );
            return Ok(());
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(anyhow::anyhow!("falha ao travar lockfile: {e}"));
        }
    }
    // `lock_file` DEVE viver até o fim do processo — o lock solta no drop.

    // Runtime tokio próprio do daemon (o SipController cria o DELE numa thread
    // à parte; dois runtimes no mesmo processo são ok).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("criar runtime tokio do daemon")?;

    let result = rt.block_on(run_inner());

    // NÃO apagar o lockfile. O flock é solto automaticamente quando o fd
    // (`lock_file`) fecha no drop ao fim do processo; o arquivo vazio em tmpfs
    // não custa nada. Apagar o caminho abriria a corrida clássica do flock: A
    // solta o lock e remove o path, B já travou o inode antigo, e C cria um
    // inode novo no mesmo caminho e trava sem conflito -> B e C rodam juntas
    // (duplo REGISTER + dois writers do config). Mantendo o arquivo, o lock
    // vive porque o fd vive, e uma 2ª instância sempre colide no mesmo inode.
    drop(lock_file);
    result
}

/// Loop assíncrono principal do daemon.
async fn run_inner() -> Result<()> {
    // O daemon é o dono do config. Envolvido em Arc<Mutex<>> porque o servidor
    // IPC (F1) lê o snapshot (conta/áudio/contatos/histórico) concorrentemente
    // com o drain de histórico do loop principal.
    let config = Arc::new(Mutex::new(AppConfig::load()));

    // Núcleo de áudio + SIP, reutilizados COMO ESTÃO (sem alterar audio.rs/sip.rs).
    let audio = AudioEngine::new();
    {
        let cfg = config.lock().unwrap();
        audio.state.set_mic_gain(cfg.audio.mic_gain);
        audio.state.set_speaker_volume(cfg.audio.speaker_volume);
    }

    // M1: limpa devices salvos que não existem mais ANTES de conectar. A F2
    // removeu do ui.rs::RustSipApp::new a lógica que zerava input_device/
    // output_device quando o id salvo não está mais na lista atual (ex. headset
    // USB salvo e depois desconectado). Sem isso, o auto_connect abaixo passaria
    // um id fantasma ao SipController e o pa_simple_new falharia no daemon,
    // deixando a chamada MUDA. As funções list_*_devices são read-only (pactl),
    // seguras de rodar no boot do daemon. Grava no máximo uma vez.
    {
        let input_devices = AudioEngine::list_input_devices();
        let output_devices = AudioEngine::list_output_devices();
        let mut cfg = config.lock().unwrap();
        let mut dirty = false;
        if let Some(ref saved_in) = cfg.audio.input_device {
            if !input_devices.iter().any(|d| &d.id == saved_in) {
                println!(
                    "[{}] [daemon] M1: input_device salvo '{}' não existe mais; zerando",
                    ts(),
                    saved_in
                );
                cfg.audio.input_device = None;
                dirty = true;
            }
        }
        if let Some(ref saved_out) = cfg.audio.output_device {
            if !output_devices.iter().any(|d| &d.id == saved_out) {
                println!(
                    "[{}] [daemon] M1: output_device salvo '{}' não existe mais; zerando",
                    ts(),
                    saved_out
                );
                cfg.audio.output_device = None;
                dirty = true;
            }
        }
        if dirty {
            cfg.save();
        }
    }

    let sip = SipController::new(audio.clone());
    let state = sip.state.clone();

    // Sinais instalados ANTES de qualquer REGISTER (auto_connect abaixo):
    // SIGTERM/SIGINT => Sair gracioso; SIGHUP ignorado. Se instalados DEPOIS do
    // Connect, um SIGTERM na janela entre o REGISTER e a instalação teria ação
    // padrão e mataria o processo SEM un-REGISTER (caso real: systemd/script
    // dando stop logo após start; a janela podia chegar a ~25s se o watcher
    // D-Bus travasse no register do ksni). tokio permite recv() mais tarde, no
    // loop principal — só a INSTALAÇÃO precisa vir cedo.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("instalar handler SIGTERM")?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("instalar handler SIGINT")?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .context("instalar handler SIGHUP")?;

    // auto_connect: dispara REGISTER no boot, igual o ui.rs faz no new.
    {
        let cfg = config.lock().unwrap();
        if cfg.account.is_configured() && cfg.account.auto_connect {
            println!("[{}] [daemon] auto_connect: enviando Connect", ts());
            sip.send(SipCommand::Connect {
                account: cfg.account.clone(),
                input_dev: cfg.audio.input_device.clone(),
                output_dev: cfg.audio.output_device.clone(),
            });
        } else {
            println!(
                "[{}] [daemon] conta não configurada ou auto_connect off; sem REGISTER",
                ts()
            );
        }
    }

    // Canal bandeja -> núcleo.
    let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel::<DaemonEvt>();

    // Sobe a bandeja. ksni::spawn() FALHA honestamente se não há host SNI
    // (org.kde.StatusNotifierWatcher) — guardamos tray_available e, se falhar,
    // seguimos (o daemon ainda serve o SIP).
    let tray = RustSipTray {
        in_call: false,
        evt_tx: evt_tx.clone(),
    };
    let tray_available;
    let _tray_handle = {
        use ksni::TrayMethods;
        match tray.spawn().await {
            Ok(handle) => {
                tray_available = true;
                println!("[{}] [daemon] bandeja ksni ativa", ts());
                Some(handle)
            }
            Err(e) => {
                tray_available = false;
                // GOTCHA F3: o tray-icon antigo tinha assume_sni_available que
                // MASCARAVA a ausência de watcher (build Ok sem host). Com ksni
                // DIRETO o spawn falha de verdade aqui — comportamento desejado.
                // Se um ambiente futuro não falhar honestamente, checar
                // NameHasOwner("org.kde.StatusNotifierWatcher") via zbus antes
                // de confiar (anotado para F3).
                println!(
                    "[{}] [daemon] AVISO: bandeja indisponível ({e}); seguindo sem tray",
                    ts()
                );
                None
            }
        }
    };

    // --- Servidor IPC: expõe o estado por socket Unix e, na F2, APLICA os
    // comandos da janela (Connect/Dial/.../SaveAccount). O daemon é o dono do
    // núcleo SIP/áudio e o ÚNICO writer do config. ---
    // Remove socket órfão antes do bind; o diretório 0700 já existe (runtime_dir
    // foi criado em run()). O servidor roda como task tokio neste mesmo runtime.
    let rt_dir = runtime_dir()?;
    let sock_path = rt_dir.join("daemon.sock");
    // Canal de BROADCAST para frames de SEÇÃO (AudioCfg/Contacts/History/
    // Account) quando um comando muda o config: todos os clientes conectados
    // recebem. Capacidade modesta — um cliente muito atrasado perde frames de
    // config (Lagged), que o Welcome/próxima mudança reconcilia. NÃO vai o
    // AppConfig inteiro, só a seção afetada (senão sobrescreve formulário aberto).
    let (cfg_tx, _cfg_rx0) = tokio::sync::broadcast::channel::<ServerMsg>(64);
    // F3: contador de clientes-janela vivos. O servidor IPC incrementa após o
    // Hello e decrementa quando a conexão cai; o loop principal lê para escolher
    // entre Raise (broadcast) e relançar a janela.
    let client_count = Arc::new(AtomicUsize::new(0));
    let ipc_server = IpcServer {
        sip: sip.clone(),
        audio: audio.clone(),
        sip_state: state.clone(),
        audio_state: audio.state.clone(),
        config: config.clone(),
        cfg_tx: cfg_tx.clone(),
        tray_available,
        daemon_pid: std::process::id(),
        client_count: client_count.clone(),
        evt_tx: evt_tx.clone(),
        save_gate: Arc::new(tokio::sync::Mutex::new(())),
    };
    match ipc_server.bind(&sock_path).await {
        Ok(listener) => {
            println!("[{}] [daemon] IPC ouvindo em {}", ts(), sock_path.display());
            tokio::spawn(ipc_server.serve(listener));
        }
        Err(e) => {
            // Falha no socket NÃO derruba o daemon: o núcleo SIP segue; só a
            // janela remota fica indisponível.
            println!(
                "[{}] [daemon] AVISO: IPC indisponível ({e:#}); seguindo sem socket",
                ts()
            );
        }
    }

    // Sinais já foram instalados no início de run_inner (antes do auto_connect),
    // para fechar a janela em que um SIGTERM mataria o processo sem un-REGISTER.

    // Drena o histórico periodicamente (o daemon é o único writer do config).
    let mut drain_tick = tokio::time::interval(Duration::from_secs(2));
    // F3: tick rápido para detectar a BORDA de subida para CallPhase::Incoming e
    // auto-mostrar a janela (relançar se não houver nenhuma). 250ms: rápido o
    // bastante para a janela surgir junto do ringtone, barato o bastante (só lê
    // o Mutex da fase).
    let mut incoming_tick = tokio::time::interval(Duration::from_millis(250));
    let mut prev_incoming = false;
    // F3: instante do último relance de janela disparado pelo daemon (guarda
    // anti duplo-clique / anti-loop do auto-show). `None` = nunca relançou.
    let mut last_relaunch: Option<Instant> = None;

    println!("[{}] [daemon] loop principal pronto", ts());
    loop {
        tokio::select! {
            _ = drain_tick.tick() => {
                drain_completed_calls(&state, &config, &cfg_tx);
            }
            _ = incoming_tick.tick() => {
                // Auto-show em chamada entrando (item 6). Dispara só na borda de
                // subida Idle/outro -> Incoming, não a cada tick enquanto toca.
                let is_incoming = matches!(
                    state.call_phase.lock().map(|p| p.clone()),
                    Ok(CallPhase::Incoming { .. })
                );
                if is_incoming && !prev_incoming {
                    match relaunch_decision(
                        client_count.load(Ordering::Relaxed),
                        last_relaunch,
                        Instant::now(),
                        RELAUNCH_GUARD,
                    ) {
                        RelaunchAction::Raise => {
                            // Há janela: só traz à frente.
                            println!("[{}] [daemon] chamada entrando: Raise à janela", ts());
                            let _ = cfg_tx.send(ServerMsg::Raise);
                        }
                        RelaunchAction::Spawn => {
                            println!("[{}] [daemon] chamada entrando: relançando janela", ts());
                            if spawn_window() {
                                last_relaunch = Some(Instant::now());
                            }
                        }
                        RelaunchAction::Ignore => {}
                    }
                }
                prev_incoming = is_incoming;
            }
            Some(evt) = evt_rx.recv() => {
                match evt {
                    DaemonEvt::Show => {
                        // Item 1: se há janela cliente conectada, pede Raise por
                        // broadcast; senão, relança a janela (default = cliente).
                        match relaunch_decision(
                            client_count.load(Ordering::Relaxed),
                            last_relaunch,
                            Instant::now(),
                            RELAUNCH_GUARD,
                        ) {
                            RelaunchAction::Raise => {
                                println!("[{}] [daemon] 'Mostrar': Raise à janela existente", ts());
                                let _ = cfg_tx.send(ServerMsg::Raise);
                            }
                            RelaunchAction::Spawn => {
                                println!("[{}] [daemon] 'Mostrar': relançando janela", ts());
                                if spawn_window() {
                                    last_relaunch = Some(Instant::now());
                                }
                            }
                            RelaunchAction::Ignore => {
                                println!("[{}] [daemon] 'Mostrar' ignorado (relance recente)", ts());
                            }
                        }
                    }
                    DaemonEvt::Quit => {
                        println!("[{}] [daemon] 'Sair' solicitado", ts());
                        break;
                    }
                }
            }
            _ = sigterm.recv() => {
                println!("[{}] [daemon] SIGTERM recebido", ts());
                break;
            }
            _ = sigint.recv() => {
                println!("[{}] [daemon] SIGINT recebido", ts());
                break;
            }
            _ = sighup.recv() => {
                println!("[{}] [daemon] SIGHUP ignorado", ts());
            }
        }
    }

    // F3 (item 4): antes de derrubar o SIP, avisa as janelas que o daemon vai
    // encerrar — elas fecham de verdade ao receber Shutdown. Broadcast é no-op
    // se não há cliente. Feito ANTES do graceful_shutdown para a janela sair
    // junto (e não ficar pendurada tentando reconectar a um socket que já foi).
    let _ = cfg_tx.send(ServerMsg::Shutdown);

    // F3 (M1): o `send` acima só ENFILEIRA o frame no broadcast; quem o escreve
    // no socket é a task de cada conexão. Se derrubarmos o runtime agora, essa
    // task pode nunca escrever — a janela vê EOF com shutdown=false, acha que o
    // daemon CAIU e respawna um daemon novo (ressuscitando o que o "Sair" pediu
    // para encerrar). Aguardamos `client_count` chegar a 0 (teto ~1s): isso dá
    // tempo das tasks entregarem o Shutdown e CONFIRMA que a janela fechou.
    // Teto para não travar o encerramento se uma janela estiver presa.
    let drain_deadline = Instant::now() + Duration::from_millis(1000);
    while client_count.load(Ordering::Relaxed) > 0 && Instant::now() < drain_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    graceful_shutdown(&sip, &state, &audio, &config, &cfg_tx).await;
    // Remove o socket IPC (best-effort) para não deixar um arquivo órfão.
    let _ = std::fs::remove_file(&sock_path);
    // Drop do _tray_handle aqui remove o ícone da bandeja.
    println!("[{}] [daemon] encerrado", ts());
    Ok(())
}

/// Porta da lógica de `ui.rs::drain_completed_calls` para o daemon: move os
/// registros de chamada concluída para `config.history` e persiste. É o único
/// ponto que escreve o config no modo daemon.
///
/// H1: quando HOUVE chamada nova (`new_entries` não vazio), após gravar faz
/// broadcast de `ServerMsg::History` por `cfg_tx` a todos os clientes — assim a
/// aba Histórico da janela atualiza na hora (antes da F2 isso vinha só no
/// Welcome, então a aba ficava parada até reabrir a janela). O snapshot do
/// histórico é clonado SOB o lock e o lock é SOLTO antes do send.
fn drain_completed_calls(
    state: &Arc<SipSharedState>,
    config: &Arc<Mutex<AppConfig>>,
    cfg_tx: &tokio::sync::broadcast::Sender<ServerMsg>,
) {
    let mut new_entries = Vec::new();
    if let Ok(mut q) = state.completed_calls.lock() {
        while let Some(rec) = q.pop_front() {
            new_entries.push(rec);
        }
    }
    if new_entries.is_empty() {
        return;
    }
    let history_snapshot = {
        let mut config = match config.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        for rec in new_entries {
            let contact_name = config
                .contacts
                .iter()
                .find(|c| c.number == rec.number)
                .map(|c| c.name.clone())
                .unwrap_or(rec.name);
            config.history.insert(
                0,
                crate::config::CallHistoryEntry {
                    timestamp: Local::now().format("%d/%m/%Y %H:%M").to_string(),
                    number: rec.number,
                    name: contact_name,
                    direction: rec.direction,
                    duration_secs: rec.duration_secs,
                },
            );
        }
        if config.history.len() > 100 {
            config.history.truncate(100);
        }
        config.save();
        config.history.clone()
    };
    println!("[{}] [daemon] histórico atualizado e salvo", ts());
    // H1: emite a lista nova a todos os clientes (lock já solto). Se não há
    // cliente conectado o broadcast é no-op (nenhum receiver) — inofensivo.
    let _ = cfg_tx.send(ServerMsg::History(history_snapshot));
}

/// Encerramento gracioso: Hangup se em chamada + Disconnect (un-REGISTER) com
/// timeout de 2s aguardando o estado virar Disconnected, depois para o áudio e
/// faz um drain final do histórico.
async fn graceful_shutdown(
    sip: &SipController,
    state: &Arc<SipSharedState>,
    audio: &AudioEngine,
    config: &Arc<Mutex<AppConfig>>,
    cfg_tx: &tokio::sync::broadcast::Sender<ServerMsg>,
) {
    let in_call = matches!(
        state.call_phase.lock().map(|p| p.clone()),
        Ok(CallPhase::InCall { .. })
            | Ok(CallPhase::Dialing { .. })
            | Ok(CallPhase::Incoming { .. })
    );
    if in_call {
        println!("[{}] [daemon] encerrando chamada ativa (Hangup)", ts());
        sip.send(SipCommand::Hangup);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Manda Disconnect SEMPRE que o estado NÃO for Disconnected — não só em
    // Registered|Connecting. Um re-REGISTER sem resposta (sip.rs:1034) ou um
    // 401/403/4xx (sip.rs ~1197-1210) seta Failed(..) SEM sair do loop da
    // sessão, então o binding segue válido no PBX e precisa do un-REGISTER.
    // É seguro mesmo sem sessão viva: o run_sip_supervisor (sip.rs:778)
    // descarta comandos != Connect, então o Disconnect vira no-op (pior caso:
    // espera os 2s do timeout abaixo).
    let needs_unregister = !matches!(
        state.reg_status.lock().map(|s| s.clone()),
        Ok(RegistrationStatus::Disconnected)
    );
    if needs_unregister {
        println!("[{}] [daemon] un-REGISTER (Disconnect), timeout 2s", ts());
        sip.send(SipCommand::Disconnect);
        // Aguarda até 2s o estado virar Disconnected — e SÓ Disconnected. O
        // handler de Disconnect (sip.rs ~1660) só seta Disconnected DEPOIS de
        // mandar o REGISTER Expires:0 e esperar 120ms, então esperar por
        // Disconnected garante que o un-REGISTER saiu. Aceitar Failed(_) aqui
        // encerraria a espera ANTES do Expires:0 sair (um Failed que chegue
        // durante a janela), deixando o binding pendurado.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let done = matches!(
                state.reg_status.lock().map(|s| s.clone()),
                Ok(RegistrationStatus::Disconnected)
            );
            if done {
                println!("[{}] [daemon] un-REGISTER concluído", ts());
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                println!("[{}] [daemon] timeout de un-REGISTER; seguindo", ts());
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    audio.stop_audio();
    // Drain final para não perder a última chamada no histórico.
    drain_completed_calls(state, config, cfg_tx);

    // O unlink do socket IPC é feito pelo chamador (run_inner) após este retorno.
    let _ = std::io::stdout().flush();
}

// =============================================================================
// Servidor IPC (F1) — expõe o estado do daemon por socket Unix, READ-ONLY.
// =============================================================================

/// Capacidade do canal por-cliente (backpressure). As mensagens de baixa-
/// frequência (Reg/Call/Flags/PublicAddr/LogAppend) usam `try_send`: se o buffer
/// encher, o cliente é lento demais e é DESCONECTADO (não há espera que pudesse
/// travar a task de escrita). A telemetria `Hot` também usa `try_send`, mas é
/// apenas DESCARTADA se cheio (a próxima amostra basta).
const CLIENT_CHAN_CAP: usize = 256;

/// Máximo de entradas de log reenviadas como backlog logo após o Welcome.
const LOG_BACKLOG_MAX: usize = 250;

/// Período do tick do Publisher (diff de estado).
const PUBLISH_TICK: Duration = Duration::from_millis(50);

/// Teto de tempo para o handshake inicial (ler o Hello e escrever o Welcome).
/// Uma conexão que abre o socket mas não fala (ou não lê) não pode prender a
/// task de conexão para sempre — passado o teto, a task encerra.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Intervalo mínimo entre `Hot` por cliente (<= 20/s => 50ms). Como o tick já é
/// 50ms, cada tick pode emitir no máximo um Hot — mas mantemos a checagem
/// explícita para robustez se o tick mudar.
const HOT_MIN_INTERVAL: Duration = Duration::from_millis(50);

/// Estado compartilhado que o servidor IPC lê para montar snapshots e deltas,
/// e os handles que ele usa para APLICAR os comandos da F2 (SipController para
/// Connect/Dial/..., AudioEngine para gain/volume/device/loopback, config para
/// persistir). `cfg_tx` faz broadcast dos frames de seção (AudioCfg/Contacts/
/// History/Account) a TODOS os clientes quando um comando muda o config.
struct IpcServer {
    sip: SipController,
    audio: AudioEngine,
    sip_state: Arc<SipSharedState>,
    audio_state: Arc<AudioSharedState>,
    config: Arc<Mutex<AppConfig>>,
    cfg_tx: tokio::sync::broadcast::Sender<ServerMsg>,
    tray_available: bool,
    daemon_pid: u32,
    /// Nº de clientes-janela conectados AGORA (incrementado após o Hello, zerado
    /// quando a conexão cai, via guarda RAII). O loop principal lê este contador
    /// para decidir, no "Mostrar"/auto-show, entre BROADCAST de `Raise` (há
    /// janela viva) e RELANÇAR a janela (não há nenhuma).
    client_count: Arc<AtomicUsize>,
    /// Canal bandeja/comando -> núcleo. `apply_command(Quit)` manda `Quit` por
    /// aqui para disparar o encerramento gracioso no loop principal (fallback
    /// sem tray: a janela fecha e pede ao daemon que também encerre).
    evt_tx: tokio::sync::mpsc::UnboundedSender<DaemonEvt>,
    /// B2 (gate F2): serializa os `config.save()` da via quente (`mutate_config`).
    /// O save é I/O + cifra + fsync + rename; roda em `spawn_blocking` para não
    /// bloquear um worker do runtime a cada evento (ex. cada 'changed' do slider
    /// de volume). Este mutex ASSÍNCRONO ordena os saves entre si — necessário
    /// porque `write_atomic` usa um tmp por-PID (`.config.json.tmp.{pid}`): dois
    /// saves concorrentes do MESMO processo colidiriam no `create_new`.
    save_gate: Arc<tokio::sync::Mutex<()>>,
}

impl IpcServer {
    /// Faz bind do UnixListener no `path`, removendo um socket órfão antes, e
    /// aplica chmod 0600 no socket (só o dono conecta — defesa em profundidade
    /// além do check de uid por conexão).
    async fn bind(&self, path: &std::path::Path) -> Result<UnixListener> {
        // Remove socket velho (um bind em path existente falha com EADDRINUSE).
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)
            .with_context(|| format!("bind do socket IPC {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 0600 {}", path.display()))?;
        }
        Ok(listener)
    }

    /// Loop de accept: cada conexão vira uma task independente. O servidor é
    /// `Arc`-compartilhado entre as conexões.
    async fn serve(self, listener: UnixListener) {
        let server = Arc::new(self);
        let my_uid = current_uid();
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    // Defesa: só aceita conexão do MESMO uid do daemon.
                    match peer_uid(&stream) {
                        Some(uid) if uid == my_uid => {}
                        other => {
                            println!(
                                "[{}] [daemon] IPC: conexão recusada (peer uid {:?} != {})",
                                ts(),
                                other,
                                my_uid
                            );
                            continue; // stream dropado => fecha
                        }
                    }
                    let server = server.clone();
                    tokio::spawn(async move {
                        if let Err(e) = server.handle_conn(stream).await {
                            // NUNCA logar o conteúdo do frame; só a causa.
                            println!("[{}] [daemon] IPC: conexão encerrada ({e})", ts());
                        }
                    });
                }
                Err(e) => {
                    println!("[{}] [daemon] IPC: accept falhou ({e}); parando listener", ts());
                    break;
                }
            }
        }
    }

    /// Atende uma conexão: lê o Hello, envia Welcome + backlog de logs, depois
    /// entrega os deltas do Publisher até o cliente desconectar.
    async fn handle_conn(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        // 1) Lê o Hello (com teto de linha e de TEMPO), senão desconecta. Sem
        //    o timeout, uma conexão que abre o socket e cala prenderia esta task
        //    indefinidamente.
        let hello = match tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            read_client_frame(&mut reader),
        )
        .await
        {
            Ok(res) => res?,
            Err(_) => {
                println!("[{}] [daemon] IPC: timeout aguardando Hello; desconectando", ts());
                return Ok(());
            }
        };
        let (client_mtime,) = match hello {
            Some(ClientMsg::Hello { proto, exe_mtime }) => {
                if proto != PROTO_VERSION {
                    // Versão incompatível: avisa (stale) mas segue — o cliente
                    // decide. Não há negociação na F1.
                    println!(
                        "[{}] [daemon] IPC: cliente proto {} != {} (stale)",
                        ts(),
                        proto,
                        PROTO_VERSION
                    );
                }
                (exe_mtime,)
            }
            None => return Ok(()), // EOF antes do Hello
            // Um comando antes do Hello viola a ordem do protocolo: o daemon
            // ainda não enviou o Welcome. Encerra a conexão sem aplicar nada.
            Some(_) => {
                println!(
                    "[{}] [daemon] IPC: comando antes do Hello; desconectando",
                    ts()
                );
                return Ok(());
            }
        };

        // 2) Monta e envia o Welcome (snapshot). stale_exe: compara o mtime do
        //    exe do cliente com o do daemon.
        let stale_exe = daemon_exe_mtime()
            .map(|d| d != client_mtime && client_mtime != 0)
            .unwrap_or(false);

        // F3: a partir daqui há um cliente-janela VIVO. Conta-o com uma guarda
        // RAII: incrementa agora e decrementa quando `handle_conn` retornar (por
        // qualquer caminho — EOF, erro de escrita, abort). O loop principal lê
        // este contador para escolher Raise vs. relançar.
        struct ClientGuard(Arc<AtomicUsize>);
        impl Drop for ClientGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let _client_guard = ClientGuard(self.client_count.clone());
        self.client_count.fetch_add(1, Ordering::Relaxed);

        let (log_seq_at_snapshot, backlog) = self.log_snapshot();
        let (welcome, contacts, history) = self.build_welcome(stale_exe, log_seq_at_snapshot);
        // Timeout no write do Welcome: um cliente que conecta e não lê não pode
        // prender a task de escrita aqui.
        match tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            ipc::write_frame_async(&mut write_half, &welcome),
        )
        .await
        {
            Ok(res) => res?,
            Err(_) => {
                println!("[{}] [daemon] IPC: timeout escrevendo Welcome; desconectando", ts());
                return Ok(());
            }
        }

        // L1: contacts e history NÃO vão no Welcome (estourariam o teto de 256
        // KiB com muitos itens). Emite cada lista em seu próprio frame logo após
        // o Welcome — variantes já existentes do protocolo. (A ligação disso no
        // cliente é da F2; remote.rs é andaime nesta fase.)
        ipc::write_frame_async(&mut write_half, &ServerMsg::Contacts(contacts)).await?;
        ipc::write_frame_async(&mut write_half, &ServerMsg::History(history)).await?;

        // 3) Backlog de logs (até LOG_BACKLOG_MAX) como LogAppend. O seq de cada
        //    entrada é aproximado: o snapshot nos dá o seq FINAL; reconstruímos
        //    os seq para trás a partir dele. (F1: suficiente para a janela
        //    alinhar; os seq exatos por-entrada entram se a API de log expuser.)
        let n = backlog.len() as u64;
        for (i, entry) in backlog.into_iter().enumerate() {
            let seq = log_seq_at_snapshot.saturating_sub(n - 1 - i as u64);
            let msg = ServerMsg::LogAppend {
                seq,
                entry: SipLogEntryWire::from(&entry),
            };
            ipc::write_frame_async(&mut write_half, &msg).await?;
        }

        // 4) Publisher: canal por-cliente + task que faz diff e empurra deltas.
        let (tx, mut rx) = mpsc::channel::<ServerMsg>(CLIENT_CHAN_CAP);
        let publisher = Publisher {
            sip_state: self.sip_state.clone(),
            audio_state: self.audio_state.clone(),
            tx,
            last_log_seq: log_seq_at_snapshot,
        };
        let pub_handle = tokio::spawn(publisher.run());

        // 5) Duas tarefas concorrentes nesta conexão:
        //    (a) READER: possui `reader` e faz loop de `read_client_frame` para
        //        aplicar os comandos ClientMsg (F2). Fica em TASK PRÓPRIA porque
        //        `read_until` NÃO é cancel-safe: num `select!` que o cancelasse
        //        no meio de uma linha, o buffer parcial se perderia e o próximo
        //        frame viria corrompido. Isolado numa task, o read nunca é
        //        cancelado. Sinaliza o fim (EOF/erro) por um oneshot.
        //    (b) WRITER (este loop): multiplexa os deltas do Publisher (`rx`) e
        //        os frames de seção por broadcast (`cfg_rx`), e encerra quando o
        //        reader sinaliza que o cliente fechou.
        let (eof_tx, mut eof_rx) = tokio::sync::oneshot::channel::<()>();
        let reader_server = self.clone();
        let reader_handle = tokio::spawn(async move {
            let mut reader = reader;
            // Ok(Some)=comando; Ok(None)=EOF limpo; Err=frame inválido/grande.
            // Os dois últimos encerram o loop (e a conexão).
            while let Ok(Some(msg)) = read_client_frame(&mut reader).await {
                // NUNCA logar o frame (SaveAccount carrega senha).
                reader_server.apply_command(msg);
            }
            let _ = eof_tx.send(());
        });

        let mut cfg_rx = self.cfg_tx.subscribe();
        let res: Result<()> = loop {
            tokio::select! {
                maybe = rx.recv() => {
                    match maybe {
                        Some(msg) => {
                            if let Err(e) = ipc::write_frame_async(&mut write_half, &msg).await {
                                break Err(e);
                            }
                        }
                        None => break Ok(()), // Publisher encerrou
                    }
                }
                bcast = cfg_rx.recv() => {
                    match bcast {
                        Ok(msg) => {
                            if let Err(e) = ipc::write_frame_async(&mut write_half, &msg).await {
                                break Err(e);
                            }
                        }
                        // Cliente atrasado perdeu N frames de config; o próximo
                        // frame de seção (ou um novo Welcome ao reconectar)
                        // reconcilia. Não derruba a conexão por isto.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // O daemon está encerrando (cfg_tx caiu). Segue
                            // servindo os deltas até o cliente/Publisher fechar.
                        }
                    }
                }
                _ = &mut eof_rx => {
                    // O reader detectou EOF/erro do cliente: encerra a conexão.
                    break Ok(());
                }
            }
        };
        pub_handle.abort();
        reader_handle.abort();
        res
    }

    /// Aplica um ClientMsg de COMANDO (F2) no núcleo do daemon. O `Hello` tardio
    /// é ignorado (o handshake já ocorreu). Cada comando que muda o config grava
    /// (o daemon é o único writer) e faz broadcast do frame de SEÇÃO afetado via
    /// `cfg_tx` — não o AppConfig inteiro, para não sobrescrever um formulário
    /// aberto em outro cliente.
    fn apply_command(&self, msg: ClientMsg) {
        match msg {
            // Handshake repetido: no-op (não há renegociação).
            ClientMsg::Hello { .. } => {}

            // --- Sessão ---
            ClientMsg::Connect => {
                let (account, input_dev, output_dev) = {
                    let cfg = self.config.lock().unwrap_or_else(|p| p.into_inner());
                    (
                        cfg.account.clone(),
                        cfg.audio.input_device.clone(),
                        cfg.audio.output_device.clone(),
                    )
                };
                if account.is_configured() {
                    self.sip.send(SipCommand::Connect {
                        account,
                        input_dev,
                        output_dev,
                    });
                }
            }
            ClientMsg::Disconnect => self.sip.send(SipCommand::Disconnect),
            ClientMsg::Dial { target } => {
                let target = target.trim().to_string();
                if target.is_empty() {
                    return;
                }
                let (input_dev, output_dev) = self.audio_devices();
                // Se um teste de loopback estava rodando, encerra antes de discar
                // (mesma política da UI local antiga em trigger_dial).
                if self.audio.state.mode.load(Ordering::Relaxed) == MODE_LOOPBACK_TEST {
                    self.audio.stop_audio();
                }
                self.sip.send(SipCommand::Dial {
                    target,
                    input_dev,
                    output_dev,
                });
            }
            ClientMsg::Answer => {
                let (input_dev, output_dev) = self.audio_devices();
                self.sip.send(SipCommand::Answer { input_dev, output_dev });
            }
            ClientMsg::Hangup => self.sip.send(SipCommand::Hangup),
            ClientMsg::KeypadDigit(d) => {
                // Tom DTMF local no daemon + DTMF SIP se em chamada.
                let (_in, out) = self.audio_devices();
                self.audio.play_keypad_dtmf(d, out);
                let in_call = matches!(
                    self.sip_state.call_phase.lock().map(|p| p.clone()),
                    Ok(CallPhase::InCall { .. })
                );
                if in_call {
                    self.sip.send(SipCommand::SendDtmf(d));
                }
            }

            // --- Flags de áudio (refletem via Publisher::Flags) ---
            ClientMsg::SetMuted(v) => self.audio.state.muted.store(v, Ordering::Relaxed),
            ClientMsg::SetHold(v) => self.audio.state.on_hold.store(v, Ordering::Relaxed),

            // --- Ajustes de áudio: aplica + grava + emite AudioCfg ---
            ClientMsg::SetMicGain(g) => {
                self.audio.state.set_mic_gain(g);
                self.mutate_config(|c| c.audio.mic_gain = g);
                self.broadcast_audio_cfg();
            }
            ClientMsg::SetSpeakerVolume(v) => {
                self.audio.state.set_speaker_volume(v);
                self.mutate_config(|c| c.audio.speaker_volume = v);
                self.broadcast_audio_cfg();
            }
            ClientMsg::SetInputDevice(dev) => {
                self.mutate_config(|c| c.audio.input_device = dev.clone());
                let out = self.audio_devices().1;
                self.audio.reload_devices_if_active(dev, out);
                self.broadcast_audio_cfg();
            }
            ClientMsg::SetOutputDevice(dev) => {
                self.mutate_config(|c| c.audio.output_device = dev.clone());
                let inp = self.audio_devices().0;
                self.audio.reload_devices_if_active(inp, dev);
                self.broadcast_audio_cfg();
            }

            // --- Teste de áudio (real, só no daemon) ---
            ClientMsg::StartLoopbackTest => {
                let (inp, out) = self.audio_devices();
                self.audio.start_loopback_test(inp, out);
            }
            ClientMsg::StopLoopbackTest => self.audio.stop_audio(),

            // --- Config persistido ---
            ClientMsg::SaveAccount { account, reconnect } => {
                let do_connect = reconnect && account.is_configured();
                let (input_dev, output_dev) = self.audio_devices();
                self.mutate_config(|c| c.account = account.clone());
                // Emite a conta de volta (o cliente aplica o formulário só aqui).
                let _ = self.cfg_tx.send(ServerMsg::Account(account.clone()));
                if do_connect {
                    self.sip.send(SipCommand::Connect {
                        account,
                        input_dev,
                        output_dev,
                    });
                }
            }
            ClientMsg::AddContact { name, number, notes } => {
                let id = format!("{}", chrono::Local::now().timestamp_millis());
                let Some(contact) = crate::config::build_contact(&name, &number, &notes, id) else {
                    return;
                };
                self.mutate_config(|c| c.contacts.push(contact.clone()));
                self.broadcast_contacts();
            }
            ClientMsg::DeleteContact(id) => {
                let mut removed = false;
                self.mutate_config(|c| {
                    removed = crate::config::remove_contact(&mut c.contacts, &id);
                });
                if removed {
                    self.broadcast_contacts();
                }
            }
            ClientMsg::ClearHistory => {
                self.mutate_config(|c| c.history.clear());
                self.broadcast_history();
            }
            ClientMsg::Quit => {
                // Fallback sem tray (item 5): a janela fechou e pediu que o
                // daemon também encerre. Sinaliza o loop principal, que faz o
                // shutdown gracioso (Hangup + un-REGISTER + stop_audio) e o
                // broadcast de Shutdown. NÃO encerramos aqui (este é só o
                // handler de comando de uma conexão) para passar pelo MESMO
                // caminho de "Sair"/SIGTERM. Se o canal já caiu (daemon
                // encerrando), o send falha silenciosamente — já está indo.
                println!("[{}] [daemon] IPC: Quit recebido da janela; encerrando", ts());
                let _ = self.evt_tx.send(DaemonEvt::Quit);
            }
            ClientMsg::RequestShow => {
                // Item 3: 2ª instância pediu "Mostrar". Reusa o caminho do tray
                // (DaemonEvt::Show) — com janela(s) viva(s) vira broadcast de
                // Raise à(s) existente(s). A 2ª instância sai logo após enviar.
                println!("[{}] [daemon] IPC: RequestShow recebido", ts());
                let _ = self.evt_tx.send(DaemonEvt::Show);
            }
        }
    }

    /// Dispositivos de áudio atuais do config (input, output).
    fn audio_devices(&self) -> (Option<String>, Option<String>) {
        let cfg = self.config.lock().unwrap_or_else(|p| p.into_inner());
        (cfg.audio.input_device.clone(), cfg.audio.output_device.clone())
    }

    /// Muta o config sob lock E persiste (o daemon é o único writer). B2: o
    /// save (I/O + cifra + fsync + rename) NÃO roda mais no worker async — sob o
    /// lock só mutamos e clonamos o snapshot; o lock é solto e o save vai para
    /// `spawn_blocking`, serializado pelo `save_gate` (ver o campo). Assim um
    /// stream de eventos rápidos (slider de volume) não prende um worker do
    /// runtime a cada 'changed'. A ordem dos saves é preservada pelo gate; a
    /// cópia em memória do config (a fonte de verdade in-process) já reflete a
    /// mutação assim que esta função retorna.
    fn mutate_config(&self, f: impl FnOnce(&mut AppConfig)) {
        let snapshot = {
            let mut cfg = self.config.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut cfg);
            cfg.clone()
        };
        let gate = self.save_gate.clone();
        tokio::spawn(async move {
            // Serializa contra outros saves (tmp por-PID em write_atomic) e
            // move o I/O bloqueante para fora do pool async.
            let _guard = gate.lock().await;
            let _ = tokio::task::spawn_blocking(move || snapshot.save()).await;
        });
    }

    /// Broadcast do frame de seção de ÁUDIO (gain/volume/device) a todos os
    /// clientes. Snapshot do config sob lock, solto antes do send.
    fn broadcast_audio_cfg(&self) {
        let audio = {
            let cfg = self.config.lock().unwrap_or_else(|p| p.into_inner());
            cfg.audio.clone()
        };
        let _ = self.cfg_tx.send(ServerMsg::AudioCfg(audio));
    }

    /// Broadcast da lista de contatos a todos os clientes.
    fn broadcast_contacts(&self) {
        let contacts = {
            let cfg = self.config.lock().unwrap_or_else(|p| p.into_inner());
            cfg.contacts.clone()
        };
        let _ = self.cfg_tx.send(ServerMsg::Contacts(contacts));
    }

    /// Broadcast do histórico a todos os clientes.
    fn broadcast_history(&self) {
        let history = {
            let cfg = self.config.lock().unwrap_or_else(|p| p.into_inner());
            cfg.history.clone()
        };
        let _ = self.cfg_tx.send(ServerMsg::History(history));
    }

    /// Lê o seq atual e um backlog das últimas entradas de log (clonadas),
    /// SOB O MESMO LOCK (`log_seq_and_entries`) para que seq e conteúdo sejam
    /// coerentes. Depois recorta às últimas `LOG_BACKLOG_MAX`.
    fn log_snapshot(&self) -> (u64, Vec<crate::sip::SipLogEntry>) {
        let (seq, mut backlog) = self.sip_state.log_seq_and_entries();
        if backlog.len() > LOG_BACKLOG_MAX {
            backlog.drain(..backlog.len() - LOG_BACKLOG_MAX);
        }
        (seq, backlog)
    }

    /// Monta o Welcome lendo config + estado atuais.
    ///
    /// L1: `contacts` e `history` NÃO vão dentro do Welcome — com muitos
    /// contatos/histórico o frame estouraria o teto de 256 KiB e o cliente nunca
    /// receberia o snapshot. O Welcome sai com as duas listas VAZIAS; as listas
    /// reais são devolvidas aqui para o chamador emitir como frames `Contacts` e
    /// `History` separados (variantes já existentes do protocolo), cada um
    /// pequeno. Retorna `(welcome, contacts, history)`.
    fn build_welcome(
        &self,
        stale_exe: bool,
        log_seq: u64,
    ) -> (ServerMsg, Vec<crate::config::Contact>, Vec<crate::config::CallHistoryEntry>) {
        let (account, audio_cfg, contacts, history, pw_reentry) = {
            let cfg = self.config.lock().unwrap_or_else(|p| p.into_inner());
            (
                cfg.account.clone(),
                cfg.audio.clone(),
                cfg.contacts.clone(),
                cfg.history.clone(),
                cfg.password_needs_reentry,
            )
        };
        let reg = self
            .sip_state
            .reg_status
            .lock()
            .map(|s| s.clone())
            .unwrap_or(RegistrationStatus::Disconnected);
        let call = self
            .sip_state
            .call_phase
            .lock()
            .map(|p| CallPhaseWire::from(&*p))
            .unwrap_or(CallPhaseWire::Idle);
        let public_addr = self
            .sip_state
            .detected_public_addr
            .lock()
            .map(|a| a.clone())
            .unwrap_or(None);
        let flags = read_flags(&self.audio_state);

        let welcome = ServerMsg::Welcome {
            proto: PROTO_VERSION,
            daemon_pid: self.daemon_pid,
            stale_exe,
            tray_available: self.tray_available,
            account,
            audio: audio_cfg,
            // L1: listas potencialmente grandes saem em frames próprios.
            contacts: Vec::new(),
            history: Vec::new(),
            password_needs_reentry: pw_reentry,
            reg,
            call,
            public_addr,
            flags,
            log_seq,
        };
        (welcome, contacts, history)
    }
}

/// Task que observa o estado e empurra APENAS deltas para um cliente.
struct Publisher {
    sip_state: Arc<SipSharedState>,
    audio_state: Arc<AudioSharedState>,
    tx: mpsc::Sender<ServerMsg>,
    last_log_seq: u64,
}

impl Publisher {
    async fn run(mut self) {
        let mut tick = tokio::time::interval(PUBLISH_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Últimos valores enviados (para o diff).
        let mut last_reg = self
            .sip_state
            .reg_status
            .lock()
            .map(|s| s.clone())
            .unwrap_or(RegistrationStatus::Disconnected);
        let mut last_call = self
            .sip_state
            .call_phase
            .lock()
            .map(|p| CallPhaseWire::from(&*p))
            .unwrap_or(CallPhaseWire::Idle);
        let mut last_flags = read_flags(&self.audio_state);
        let mut last_public = self
            .sip_state
            .detected_public_addr
            .lock()
            .map(|a| a.clone())
            .unwrap_or(None);
        let mut last_hot: Option<(f32, f32, u64, u64)> = None;
        let mut last_hot_sent = tokio::time::Instant::now() - HOT_MIN_INTERVAL;

        loop {
            tick.tick().await;
            // Sem cliente (canal fechado), encerra.
            if self.tx.is_closed() {
                return;
            }

            // --- Reg ---
            // Extrai o valor e SOLTA o guard antes de qualquer await (o
            // MutexGuard std não é Send e não pode cruzar o .await).
            let reg_now = self
                .sip_state
                .reg_status
                .lock()
                .map(|s| s.clone())
                .ok();
            if let Some(reg) = reg_now {
                if reg != last_reg {
                    last_reg = reg.clone();
                    if self.send_lowfreq_now(ServerMsg::Reg(reg)).is_err() {
                        return;
                    }
                }
            }

            // --- Call ---
            let call_now = self
                .sip_state
                .call_phase
                .lock()
                .map(|p| CallPhaseWire::from(&*p))
                .ok();
            if let Some(call) = call_now {
                // Compara ignorando o elapsed_ms de InCall (que muda sempre); só
                // emite quando a FASE/identidade muda, não a cada tick em chamada.
                if !same_call_identity(&call, &last_call) {
                    last_call = call.clone();
                    if self.send_lowfreq_now(ServerMsg::Call(call)).is_err() {
                        return;
                    }
                }
            }

            // --- Flags ---
            let flags = read_flags(&self.audio_state);
            if flags != last_flags {
                last_flags = flags;
                if self.send_lowfreq_now(ServerMsg::Flags(flags)).is_err() {
                    return;
                }
            }

            // --- PublicAddr ---
            let pa_now = self
                .sip_state
                .detected_public_addr
                .lock()
                .map(|a| a.clone())
                .ok();
            if let Some(pa) = pa_now {
                if pa != last_public {
                    last_public = pa.clone();
                    if self.send_lowfreq_now(ServerMsg::PublicAddr(pa)).is_err() {
                        return;
                    }
                }
            }

            // --- LogAppend (novos desde last_log_seq) ---
            // Lê seq E conteúdo sob o MESMO lock (coerência com add_log). Só
            // então decide se há algo novo e recorta a cauda. Travar o lock a
            // cada tick (50ms) é barato; a correção vale mais que o antigo
            // short-circuit sem lock (que lia o seq fora do lock e abria a
            // corrida M1).
            let (cur_seq, all_entries) = self.sip_state.log_seq_and_entries();
            if cur_seq > self.last_log_seq {
                let new_count = (cur_seq - self.last_log_seq) as usize;
                // Recorta às últimas `new_count` entradas (o ring pode ter
                // descartado entradas antigas; nunca mais do que há).
                let take = new_count.min(all_entries.len());
                let entries = &all_entries[all_entries.len() - take..];
                let base = cur_seq.saturating_sub(entries.len() as u64) + 1;
                for (i, entry) in entries.iter().enumerate() {
                    let msg = ServerMsg::LogAppend {
                        seq: base + i as u64,
                        entry: SipLogEntryWire::from(entry),
                    };
                    if self.send_lowfreq_now(msg).is_err() {
                        return;
                    }
                }
                self.last_log_seq = cur_seq;
            }

            // --- Hot (telemetria, <= 20/s, try_send, só se mudou) ---
            if last_hot_sent.elapsed() >= HOT_MIN_INTERVAL {
                let mic = self.audio_state.mic_level();
                let spk = self.audio_state.speaker_level();
                let tx_p = self.sip_state.rtp_stats.tx_packets.load(Ordering::Relaxed);
                let rx_p = self.sip_state.rtp_stats.rx_packets.load(Ordering::Relaxed);
                let cur = (mic, spk, tx_p, rx_p);
                let changed = match last_hot {
                    Some((m, s, t, r)) => {
                        (m - mic).abs() > 0.001
                            || (s - spk).abs() > 0.001
                            || t != tx_p
                            || r != rx_p
                    }
                    None => true,
                };
                if changed {
                    // try_send: descarta silenciosamente se o buffer encheu (o
                    // cliente está atrasado; a próxima amostra basta).
                    match self.tx.try_send(ServerMsg::Hot {
                        mic,
                        spk,
                        tx: tx_p,
                        rx: rx_p,
                    }) {
                        Ok(()) => {
                            last_hot = Some(cur);
                            last_hot_sent = tokio::time::Instant::now();
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => { /* descarta */ }
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            }
        }
    }

    /// Envia uma mensagem de baixa-frequência SEM bloquear o Publisher.
    ///
    /// Usa `try_send`: se o canal encheu (`Full`), o cliente está lento demais
    /// e é DESCONECTADO (retorna `Err`) — como o comentário de `CLIENT_CHAN_CAP`
    /// promete. O antigo `send().await` apenas ESPERAVA, e um cliente que parou
    /// de ler travava a task de escrita para sempre (o braço de leitura do
    /// `select!` nunca mais rodava). `Closed` (cliente já foi) também vira `Err`.
    fn send_lowfreq_now(&self, msg: ServerMsg) -> Result<(), ()> {
        match self.tx.try_send(msg) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(()),
        }
    }
}

/// Duas CallPhaseWire têm a MESMA identidade se diferem no máximo no elapsed_ms
/// de InCall (que avança a cada tick e não deve gerar broadcast).
fn same_call_identity(a: &CallPhaseWire, b: &CallPhaseWire) -> bool {
    use CallPhaseWire::*;
    match (a, b) {
        (Idle, Idle) => true,
        (
            Dialing { remote_number: n1, status_text: s1 },
            Dialing { remote_number: n2, status_text: s2 },
        ) => n1 == n2 && s1 == s2,
        (
            Incoming { remote_number: n1, remote_name: m1 },
            Incoming { remote_number: n2, remote_name: m2 },
        ) => n1 == n2 && m1 == m2,
        (
            InCall { remote_number: n1, remote_name: m1, codec_name: c1, .. },
            InCall { remote_number: n2, remote_name: m2, codec_name: c2, .. },
        ) => n1 == n2 && m1 == m2 && c1 == c2,
        _ => false,
    }
}

/// Lê as flags atômicas do estado de áudio.
fn read_flags(audio: &AudioSharedState) -> FlagsWire {
    FlagsWire {
        muted: audio.muted.load(Ordering::Relaxed),
        on_hold: audio.on_hold.load(Ordering::Relaxed),
        mode: audio.mode.load(Ordering::Relaxed),
    }
}

/// Lê um ClientMsg do socket com teto de MAX_FRAME_BYTES (framing NDJSON).
/// Retorna Ok(None) em EOF. Reusa a lógica do ipc.rs mas em cima do async
/// BufReader do tokio.
async fn read_client_frame<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> Result<Option<ClientMsg>> {
    let mut buf = Vec::with_capacity(256);
    let n = reader
        .take((MAX_FRAME_BYTES as u64) + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    let had_newline = buf.last() == Some(&b'\n');
    if had_newline {
        buf.pop();
    }
    if !had_newline && buf.len() > MAX_FRAME_BYTES {
        anyhow::bail!("Hello IPC excede o teto de {} bytes", MAX_FRAME_BYTES);
    }
    if buf.len() > MAX_FRAME_BYTES {
        anyhow::bail!("Hello IPC excede o teto de {} bytes", MAX_FRAME_BYTES);
    }
    let msg = serde_json::from_slice::<ClientMsg>(&buf)?;
    Ok(Some(msg))
}

/// uid efetivo do processo (sem crate libc; via metadata de /proc/self).
fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(md) = std::fs::metadata("/proc/self") {
            return md.uid();
        }
    }
    // Fallback improvável: 0 só casaria root; melhor um sentinela que não casa
    // ninguém a toa — usamos u32::MAX para recusar por segurança se /proc falhar.
    u32::MAX
}

/// uid do peer de um UnixStream, via SO_PEERCRED exposto pelo tokio.
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    stream.peer_cred().ok().map(|c| c.uid())
}

/// mtime (epoch secs) do executável do daemon, para detecção de stale_exe.
fn daemon_exe_mtime() -> Option<u64> {
    let exe = std::env::current_exe().ok()?;
    let md = std::fs::metadata(exe).ok()?;
    let mtime = md.modified().ok()?;
    mtime
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F3 (L4): a regra pura de relance. count>0 → Raise; count==0 sem relance
    /// recente → Spawn; count==0 com relance dentro da guarda → Ignore.
    #[test]
    fn test_relaunch_decision() {
        let guard = Duration::from_secs(5);
        let now = Instant::now();

        // Há janela viva: sempre Raise, mesmo que nunca tenha relançado.
        assert_eq!(
            relaunch_decision(1, None, now, guard),
            RelaunchAction::Raise
        );
        // Há janela viva ignora a guarda (um relance recente não impede o Raise).
        assert_eq!(
            relaunch_decision(2, Some(now), now, guard),
            RelaunchAction::Raise
        );

        // Sem janela e nunca relançou: Spawn.
        assert_eq!(
            relaunch_decision(0, None, now, guard),
            RelaunchAction::Spawn
        );
        // Sem janela e último relance já saiu da guarda (>= guard atrás): Spawn.
        let old = now.checked_sub(guard).unwrap_or(now);
        assert_eq!(
            relaunch_decision(0, Some(old), now, guard),
            RelaunchAction::Spawn
        );

        // Sem janela mas relance recente (< guarda atrás): Ignore.
        let recent = now
            .checked_sub(Duration::from_secs(1))
            .unwrap_or(now);
        assert_eq!(
            relaunch_decision(0, Some(recent), now, guard),
            RelaunchAction::Ignore
        );
    }

    /// Instância única: o 1º File::try_lock() no mesmo path adquire (Ok(true)),
    /// o 2º com o 1º ainda vivo é recusado (Ok(false)). Reproduz a garantia de
    /// que uma 2ª `rustsip daemon` sai sozinha.
    #[test]
    fn test_single_instance_lock() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("rustsip-test-{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let f1 = File::create(&path).expect("criar lockfile 1");
        assert!(
            f1.try_lock().is_ok(),
            "1ª instância deve adquirir o lock"
        );

        let f2 = File::create(&path).expect("criar lockfile 2");
        assert!(
            matches!(f2.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "2ª instância deve ser recusada (WouldBlock) enquanto a 1ª segura o lock"
        );

        // Solta a 1ª e confirma que agora o lock é adquirível.
        drop(f1);
        let f3 = File::create(&path).expect("criar lockfile 3");
        assert!(
            f3.try_lock().is_ok(),
            "após soltar, o lock deve ser adquirível de novo"
        );

        drop(f3);
        let _ = std::fs::remove_file(&path);
    }

    /// O ícone embutido tem o tamanho ARGB32 esperado (64*64*4) e não é todo
    /// transparente (o glifo foi preservado na geração).
    #[test]
    fn test_tray_icon_bytes() {
        assert_eq!(
            TRAY_ICON_ARGB.len(),
            (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize,
            "tamanho ARGB != 64*64*4"
        );
        let opaque = TRAY_ICON_ARGB.chunks(4).filter(|p| p[0] == 255).count();
        assert!(opaque > 0, "ícone inteiramente transparente");
    }
}

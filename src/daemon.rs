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

use crate::audio::{AudioEngine, AudioSharedState};
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
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
            let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700));
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

    // --- Servidor IPC (F1): expõe o estado por socket Unix para a janela ---
    // Remove socket órfão antes do bind; o diretório 0700 já existe (runtime_dir
    // foi criado em run()). O servidor roda como task tokio neste mesmo runtime.
    let rt_dir = runtime_dir()?;
    let sock_path = rt_dir.join("daemon.sock");
    let ipc_server = IpcServer {
        sip_state: state.clone(),
        audio_state: audio.state.clone(),
        config: config.clone(),
        tray_available,
        daemon_pid: std::process::id(),
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

    println!("[{}] [daemon] loop principal pronto", ts());
    loop {
        tokio::select! {
            _ = drain_tick.tick() => {
                drain_completed_calls(&state, &config);
            }
            Some(evt) = evt_rx.recv() => {
                match evt {
                    DaemonEvt::Show => {
                        // F1 abrirá a janela via socket; F0 só registra a intenção.
                        println!("[{}] [daemon] show requested", ts());
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

    graceful_shutdown(&sip, &state, &audio, &config).await;
    // Remove o socket IPC (best-effort) para não deixar um arquivo órfão.
    let _ = std::fs::remove_file(&sock_path);
    // Drop do _tray_handle aqui remove o ícone da bandeja.
    println!("[{}] [daemon] encerrado", ts());
    Ok(())
}

/// Porta da lógica de `ui.rs::drain_completed_calls` para o daemon: move os
/// registros de chamada concluída para `config.history` e persiste. É o único
/// ponto que escreve o config no modo daemon.
fn drain_completed_calls(state: &Arc<SipSharedState>, config: &Arc<Mutex<AppConfig>>) {
    let mut new_entries = Vec::new();
    if let Ok(mut q) = state.completed_calls.lock() {
        while let Some(rec) = q.pop_front() {
            new_entries.push(rec);
        }
    }
    if new_entries.is_empty() {
        return;
    }
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
    println!("[{}] [daemon] histórico atualizado e salvo", ts());
}

/// Encerramento gracioso: Hangup se em chamada + Disconnect (un-REGISTER) com
/// timeout de 2s aguardando o estado virar Disconnected, depois para o áudio e
/// faz um drain final do histórico.
async fn graceful_shutdown(
    sip: &SipController,
    state: &Arc<SipSharedState>,
    audio: &AudioEngine,
    config: &Arc<Mutex<AppConfig>>,
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
    drain_completed_calls(state, config);

    // O unlink do socket IPC é feito pelo chamador (run_inner) após este retorno.
    let _ = std::io::stdout().flush();
}

// =============================================================================
// Servidor IPC (F1) — expõe o estado do daemon por socket Unix, READ-ONLY.
// =============================================================================

/// Capacidade do canal por-cliente (backpressure). Mensagens de baixa-frequência
/// (Reg/Call/Flags/PublicAddr/LogAppend/Welcome) usam `send().await` que só
/// bloqueia o Publisher brevemente; se o buffer encher, o cliente é lento demais
/// e é desconectado. A telemetria `Hot` usa `try_send` e é DESCARTADA se cheio.
const CLIENT_CHAN_CAP: usize = 256;

/// Máximo de entradas de log reenviadas como backlog logo após o Welcome.
const LOG_BACKLOG_MAX: usize = 250;

/// Período do tick do Publisher (diff de estado).
const PUBLISH_TICK: Duration = Duration::from_millis(50);

/// Intervalo mínimo entre `Hot` por cliente (<= 20/s => 50ms). Como o tick já é
/// 50ms, cada tick pode emitir no máximo um Hot — mas mantemos a checagem
/// explícita para robustez se o tick mudar.
const HOT_MIN_INTERVAL: Duration = Duration::from_millis(50);

/// Estado compartilhado que o servidor IPC lê para montar snapshots e deltas.
struct IpcServer {
    sip_state: Arc<SipSharedState>,
    audio_state: Arc<AudioSharedState>,
    config: Arc<Mutex<AppConfig>>,
    tray_available: bool,
    daemon_pid: u32,
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

        // 1) Lê o Hello (com teto de linha), senão desconecta.
        let hello = read_client_frame(&mut reader).await?;
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
        };

        // 2) Monta e envia o Welcome (snapshot). stale_exe: compara o mtime do
        //    exe do cliente com o do daemon.
        let stale_exe = daemon_exe_mtime()
            .map(|d| d != client_mtime && client_mtime != 0)
            .unwrap_or(false);

        let (log_seq_at_snapshot, backlog) = self.log_snapshot();
        let welcome = self.build_welcome(stale_exe, log_seq_at_snapshot);
        ipc::write_frame_async(&mut write_half, &welcome).await?;

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

        // 5) Entrega os deltas ao socket. Também drena o read half para detectar
        //    EOF/erros do cliente (na F1 não há mais ClientMsg, mas um frame
        //    inesperado > teto desconecta).
        let mut drain_buf = [0u8; 256];
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
                r = reader.read(&mut drain_buf) => {
                    match r {
                        Ok(0) => break Ok(()),       // cliente fechou
                        Ok(_) => { /* F1: ignora bytes do cliente */ }
                        Err(e) => break Err(e.into()),
                    }
                }
            }
        };
        pub_handle.abort();
        res
    }

    /// Lê o seq atual e um backlog das últimas entradas de log (clonadas).
    fn log_snapshot(&self) -> (u64, Vec<crate::sip::SipLogEntry>) {
        let seq = self.sip_state.log_seq.load(Ordering::Relaxed);
        let backlog = if let Ok(logs) = self.sip_state.logs.lock() {
            let take = logs.len().min(LOG_BACKLOG_MAX);
            logs.iter().skip(logs.len() - take).cloned().collect()
        } else {
            Vec::new()
        };
        (seq, backlog)
    }

    /// Monta o Welcome lendo config + estado atuais.
    fn build_welcome(&self, stale_exe: bool, log_seq: u64) -> ServerMsg {
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

        ServerMsg::Welcome {
            proto: PROTO_VERSION,
            daemon_pid: self.daemon_pid,
            stale_exe,
            tray_available: self.tray_available,
            account,
            audio: audio_cfg,
            contacts,
            history,
            password_needs_reentry: pw_reentry,
            reg,
            call,
            public_addr,
            flags,
            log_seq,
        }
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
                    if self.send_lowfreq(ServerMsg::Reg(reg)).await.is_err() {
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
                    if self.send_lowfreq(ServerMsg::Call(call)).await.is_err() {
                        return;
                    }
                }
            }

            // --- Flags ---
            let flags = read_flags(&self.audio_state);
            if flags != last_flags {
                last_flags = flags;
                if self.send_lowfreq(ServerMsg::Flags(flags)).await.is_err() {
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
                    if self.send_lowfreq(ServerMsg::PublicAddr(pa)).await.is_err() {
                        return;
                    }
                }
            }

            // --- LogAppend (novos desde last_log_seq) ---
            let cur_seq = self.sip_state.log_seq.load(Ordering::Relaxed);
            if cur_seq > self.last_log_seq {
                let new_count = (cur_seq - self.last_log_seq) as usize;
                // Clona as entradas e SOLTA o guard antes do loop de await.
                let entries: Vec<crate::sip::SipLogEntry> = {
                    if let Ok(logs) = self.sip_state.logs.lock() {
                        let take = new_count.min(logs.len());
                        logs.iter().skip(logs.len() - take).cloned().collect()
                    } else {
                        Vec::new()
                    }
                };
                let base = cur_seq.saturating_sub(entries.len() as u64) + 1;
                for (i, entry) in entries.into_iter().enumerate() {
                    let msg = ServerMsg::LogAppend {
                        seq: base + i as u64,
                        entry: SipLogEntryWire::from(&entry),
                    };
                    if self.send_lowfreq(msg).await.is_err() {
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

    /// Envia uma mensagem de baixa-frequência; o `send().await` aplica
    /// backpressure e, se o canal fechou (cliente foi), retorna Err para o run
    /// encerrar.
    async fn send_lowfreq(&self, msg: ServerMsg) -> Result<(), ()> {
        self.tx.send(msg).await.map_err(|_| ())
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

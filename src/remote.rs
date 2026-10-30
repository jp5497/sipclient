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
use std::os::unix::process::CommandExt;
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

/// Caminho do lockfile de instância única da JANELA (espelha o `runtime_dir` do
/// daemon). Distinto do `daemon.lock` — um daemon e uma janela coexistem.
fn gui_lock_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("rustsip"))
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config").join("rustsip")
        });
    base.join("gui.lock")
}

/// F3 (item 3 / L3): resultado de `acquire_gui_lock`. Três estados, porque
/// "não consegui o lock" tem DUAS causas que o chamador deve tratar de forma
/// OPOSTA:
///   - `Owner(File)`: somos a 1ª janela. Mantenha o `File` vivo até o fim do
///     processo (o lock é solto no drop).
///   - `Busy`: já há OUTRA janela (lock ocupado). A 2ª instância pede Raise e
///     sai com exit 0 (não abre janela).
///   - `NoLock`: NÃO deu para criar/travar o lock por erro de I/O (FS sem
///     flock, /dev/null indisponível, etc.). Degradar ABERTO: seguir como 1ª
///     instância e ABRIR a janela — um erro de lock não deve impedir o app de
///     abrir. O custo é uma possível 2ª janela nesse caso raro.
enum GuiLock {
    Owner(std::fs::File),
    Busy,
    NoLock,
}

/// F3 (item 3 / L3): adquire o lock exclusivo da janela. Mesmo padrão do
/// daemon.lock (File::try_lock). Distingue "já há janela" (`Busy`) de "falha de
/// I/O ao obter o lock" (`NoLock`) — antes ambos devolviam `Err(())` e o
/// chamador tratava os dois como "já há janela", saindo em SILÊNCIO sem abrir
/// janela mesmo quando só o lock falhou.
fn acquire_gui_lock() -> GuiLock {
    let path = gui_lock_path();
    // Garante o diretório (o daemon normalmente já criou, mas a janela pode
    // subir primeiro num ambiente sem auto_connect).
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            // Erro de I/O ao criar o lockfile: NÃO sabemos se há outra janela;
            // degradar aberto (seguir como 1ª instância e abrir a janela).
            eprintln!("rustsip: aviso: gui.lock indisponível ({e}); seguindo sem instância única");
            return GuiLock::NoLock;
        }
    };
    match file.try_lock() {
        Ok(()) => GuiLock::Owner(file),
        // Lock ocupado: há OUTRA janela viva.
        Err(std::fs::TryLockError::WouldBlock) => GuiLock::Busy,
        // Erro de I/O ao travar: degradar aberto (1ª instância).
        Err(std::fs::TryLockError::Error(e)) => {
            eprintln!("rustsip: aviso: travar gui.lock falhou ({e}); seguindo como 1ª instância");
            GuiLock::NoLock
        }
    }
}

/// F3 (item 3): a 2ª instância conecta ao daemon e manda Hello + RequestShow,
/// fazendo o daemon trazer a janela JÁ ABERTA à frente. Best-effort: qualquer
/// falha é silenciada (a 2ª instância sai de qualquer forma). NÃO abre janela.
fn request_show_existing() {
    let Ok(mut stream) = ensure_daemon() else {
        return;
    };
    let _ = ipc::write_frame(
        &mut stream,
        &ClientMsg::Hello {
            proto: PROTO_VERSION,
            exe_mtime: exe_mtime(),
        },
    );
    let _ = ipc::write_frame(&mut stream, &ClientMsg::RequestShow);
    // Pequena folga para o frame sair antes do processo terminar (o write já
    // deu flush, mas garante o envio em sockets lentos).
    std::thread::sleep(std::time::Duration::from_millis(50));
}

/// Handle de ESCRITA do cliente para o daemon. A janela envia `ClientMsg` de
/// comando por aqui; a leitura dos `ServerMsg` fica na thread de IPC. O
/// `UnixStream` é full-duplex, então um `try_clone` dá o half de escrita para a
/// UI enquanto a thread lê do outro clone.
///
/// Todas as ações da UI passam por `send()`. Uma falha de escrita (daemon caiu)
/// é guardada em `broken` para a UI poder refletir "desconectado" sem travar.
/// NUNCA logar o frame (SaveAccount carrega a senha) — só a causa, sem corpo.
///
/// F3: o `writer` é um `Option` SUBSTITUÍVEL — quando o socket cai ele vira
/// `None` e `broken` fica true; o supervisor de reconexão (ver `run_remote_window`)
/// instala um novo half de escrita via `reconnect_with` quando o daemon volta.
#[derive(Clone)]
pub struct DaemonClient {
    writer: Arc<Mutex<Option<UnixStream>>>,
    broken: Arc<std::sync::atomic::AtomicBool>,
}

impl DaemonClient {
    fn new(writer: UnixStream) -> Self {
        Self {
            writer: Arc::new(Mutex::new(Some(writer))),
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
        if let Ok(mut guard) = self.writer.lock() {
            match guard.as_mut() {
                Some(w) => {
                    if ipc::write_frame(w, &msg).is_err() {
                        self.broken.store(true, Ordering::Relaxed);
                        *guard = None;
                    }
                }
                None => self.broken.store(true, Ordering::Relaxed),
            }
        }
    }

    /// true se o socket caiu (EOF/erro). A UI mostra o banner "reconectando" e
    /// o supervisor tenta reabrir. Público para a UI consultar por frame.
    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }

    /// F3: marca o cliente como quebrado (a thread de leitura chama isto ao ver
    /// EOF/erro, para o `send()` da UI parar imediatamente e o banner subir).
    pub fn mark_broken(&self) {
        self.broken.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.writer.lock() {
            *guard = None;
        }
    }

    /// F3: instala um novo half de escrita após uma reconexão bem-sucedida e
    /// limpa o flag `broken`. A UI volta a enviar comandos pelo socket novo.
    fn reconnect_with(&self, writer: UnixStream) {
        if let Ok(mut guard) = self.writer.lock() {
            *guard = Some(writer);
        }
        self.broken.store(false, Ordering::Relaxed);
    }
}

/// F3: sinais de CONTROLE do daemon para a janela que NÃO são estado espelhado
/// e sim ações pontuais da UI thread (trazer à frente, fechar) mais flags de
/// política. Compartilhado entre a thread de IPC (que SETA) e o `update()` da
/// janela (que POLLA por frame e age). A UI thread é a única que pode falar com
/// o viewport do egui; por isso a thread de IPC só levanta o flag e pede um
/// repaint, nunca chama o viewport direto.
pub struct WindowControl {
    /// O daemon pediu `Raise` (tray "Mostrar" com janela viva, ou auto-show).
    pub raise: std::sync::atomic::AtomicBool,
    /// O daemon vai encerrar (`Shutdown`): a janela fecha de verdade e NÃO
    /// mostra o banner de reconexão (o EOF que vem a seguir é esperado).
    pub shutdown: std::sync::atomic::AtomicBool,
    /// Há bandeja (SNI) no ambiente (vem do `Welcome.tray_available`). Decide o
    /// comportamento de FECHAR a janela: com tray, só encerra a janela; sem
    /// tray, encerra o daemon também (ClientMsg::Quit).
    ///
    /// L2 (gate F3): o default é `true` ANTES do Welcome chegar (ver `new()`).
    /// Trade-off: existe uma janela de milissegundos entre a janela subir e o
    /// Welcome informar o valor real em que, se houver bandeja, um fechar
    /// nesse instante assumiria `true` corretamente, mas se NÃO houver bandeja
    /// o fechar não mandaria `Quit` e deixaria o daemon órfão. Aceitável porque
    /// (a) o Welcome chega logo após o connect (handshake local no socket), e
    /// (b) o default errar para `true` é o lado seguro: nunca derruba o daemon
    /// por engano num fechar prematuro — no máximo adia o `Quit` por um frame.
    pub tray_available: std::sync::atomic::AtomicBool,
    /// F3 (M1): o usuário pediu para encerrar (fechou a janela sem tray → Quit,
    /// ou está fechando de propósito). Um EOF no socket DEPOIS disto é um
    /// "fechamento esperado" — o supervisor NÃO deve respawnar o daemon (senão
    /// o fechar-sem-tray ressuscita o daemon que o Quit acabou de derrubar).
    pub expect_quit: std::sync::atomic::AtomicBool,
    /// F3 (M2/L1): o supervisor já GASTOU o respawn automático desta vida e
    /// desistiu de ressuscitar o daemon (a 1ª tentativa de respawn não colou).
    /// A UI troca o banner de "reconectando…" para "reabra o RustSIP". É só
    /// leitura para a UI; quem seta é o `reconnect_loop`.
    pub gave_up: std::sync::atomic::AtomicBool,
    /// Clone do `egui::Context` para a thread de IPC pedir repaint quando um
    /// sinal chega com a janela ociosa em background.
    pub ctx: Mutex<Option<egui::Context>>,
}

impl WindowControl {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            raise: std::sync::atomic::AtomicBool::new(false),
            shutdown: std::sync::atomic::AtomicBool::new(false),
            // Assume true até o Welcome dizer o contrário — evita que um fechar
            // antes do Welcome derrube o daemon por engano.
            tray_available: std::sync::atomic::AtomicBool::new(true),
            expect_quit: std::sync::atomic::AtomicBool::new(false),
            gave_up: std::sync::atomic::AtomicBool::new(false),
            ctx: Mutex::new(None),
        })
    }

    /// Acorda a UI thread (repaint) para ela poller os flags já no próximo frame
    /// mesmo se a janela estava ociosa em background.
    fn wake(&self) {
        if let Ok(guard) = self.ctx.lock() {
            if let Some(ctx) = guard.as_ref() {
                ctx.request_repaint();
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
    //
    // M2: o daemon tem que SOBREVIVER ao fim da janela e ao Ctrl-C no terminal
    // de onde o `rustsip` foi lançado (objetivo central do ADR: registro SIP
    // contínuo com a janela fechada). Sem grupo próprio, o SIGINT do Ctrl-C vai
    // ao grupo inteiro do terminal e derruba o daemon num shutdown gracioso
    // (un-REGISTER), matando o binding. `.process_group(0)` coloca o daemon em
    // seu próprio grupo (desacoplado do SIGINT do terminal) e `.stdin(null)`
    // solta a entrada controladora. Ambos são std estável, sem dep nova.
    let exe = std::env::current_exe()?;
    let child = std::process::Command::new(exe)
        .arg("daemon")
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .spawn()?;

    // BÔNUS M2: o Child era descartado sem wait — se o daemon morresse antes da
    // janela viraria zumbi (entrada na tabela de processos até o pai sair).
    // Uma thread detached faz o reap. A F3 (relaunch da janela) vai reencostar
    // neste spawn; por ora isto basta e é trivial.
    std::thread::Builder::new()
        .name("rustsip-daemon-reaper".to_string())
        .spawn(move || {
            let mut child = child;
            let _ = child.wait();
        })
        .ok();
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

    // F3 (item 3): INSTÂNCIA ÚNICA da janela via `gui.lock` (mesmo padrão do
    // daemon.lock da F0: File::try_lock). `Busy` → já há janela: esta 2ª
    // instância NÃO abre janela, só dispara RequestShow (o daemon faz broadcast
    // de Raise à janela existente) e sai 0. `Owner` → somos a 1ª: o File DEVE
    // viver até o fim do processo (mantido no Option abaixo). `NoLock` → erro
    // de I/O no lock: degradar ABERTO (seguir como 1ª instância, abrir a janela
    // sem garantia de instância única — não fechar o app por falha de lock).
    let gui_lock: Option<std::fs::File> = match acquire_gui_lock() {
        GuiLock::Owner(f) => Some(f),
        GuiLock::Busy => {
            // Lock ocupado: já há uma janela. Pede Raise e sai.
            request_show_existing();
            std::process::exit(0);
        }
        GuiLock::NoLock => None,
    };

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

    // F3: sinais de controle daemon->janela (Raise/Shutdown/tray_available) e o
    // ctx para acordar a UI thread quando um sinal chega ociosa.
    let control = WindowControl::new();

    // Half de ESCRITA para a UI (comandos) e half de LEITURA para a thread de
    // IPC (frames). try_clone dá dois handles do MESMO socket full-duplex.
    let write_half = stream
        .try_clone()
        .expect("clonar UnixStream para escrita do cliente");
    let client = DaemonClient::new(write_half);

    // F3: SUPERVISOR de IPC. Em vez de uma thread que morre no 1º EOF, um loop
    // que: roda o ipc_client_loop até EOF/erro; se foi um Shutdown anunciado,
    // encerra quieto; senão marca `broken` (a UI sobe o banner) e tenta
    // reconectar — ensure_daemon() respawna o daemon UMA vez por tentativa e dá
    // retry de connect a cada 1s. Ao reconectar, instala o novo writer no
    // DaemonClient e volta a ler.
    {
        let sip_state = sip_state.clone();
        let audio_state = audio_state.clone();
        let config = config.clone();
        let control = control.clone();
        let client = client.clone();
        let mut stream = Some(stream);
        std::thread::Builder::new()
            .name("rustsip-ipc-client".to_string())
            .spawn(move || {
            // F3 (M2): o respawn automático é 1x POR VIDA desta janela, não por
            // queda. Depois do 1º respawn, quedas seguintes só tentam CONNECT
            // (não spawnam de novo) — um daemon que sobe, aceita e crasha não
            // pode gerar loop respawn→connect→crash→respawn.
            let mut respawned = false;
            loop {
                // Pega o stream atual (o 1º já conectado; os seguintes vêm do
                // reconnect abaixo).
                let s = match stream.take() {
                    Some(s) => s,
                    None => break,
                };
                let res = ipc_client_loop(
                    s,
                    sip_state.clone(),
                    audio_state.clone(),
                    config.clone(),
                    control.clone(),
                );
                if let Err(e) = res {
                    // NUNCA logar o frame (pode conter a senha no Welcome); só a causa.
                    eprintln!("rustsip: conexão IPC encerrada: {e}");
                }

                // FECHAMENTO ESPERADO (M1): não respawnar.
                //   - Shutdown anunciado pelo daemon: EOF esperado, a UI fecha
                //     pelo handler de Shutdown.
                //   - expect_quit: o usuário fechou a janela sem tray (mandou
                //     Quit) ou está fechando de propósito — o EOF que vem a
                //     seguir é o daemon encerrando a pedido, NÃO uma queda. Sem
                //     este guard, o fechar-sem-tray ressuscitaria o daemon que o
                //     Quit acabou de derrubar.
                if control.shutdown.load(Ordering::Relaxed)
                    || control.expect_quit.load(Ordering::Relaxed)
                {
                    break;
                }

                // Queda INESPERADA: sobe o banner e tenta reconectar.
                client.mark_broken();
                control.wake();
                match reconnect_loop(&control, &mut respawned) {
                    Some(new_stream) => {
                        let wh = match new_stream.try_clone() {
                            Ok(w) => w,
                            Err(_) => {
                                // Falha ao clonar o half de escrita: trata como
                                // nova queda e segue tentando.
                                continue;
                            }
                        };
                        client.reconnect_with(wh);
                        control.wake();
                        stream = Some(new_stream);
                    }
                    // None: ou um Shutdown/Quit chegou no meio, ou o supervisor
                    // DESISTIU de respawnar (gave_up setado, banner já trocado).
                    // Em ambos encerra o loop de reconexão.
                    None => break,
                }
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
    let result = eframe::run_native(
        "RustSIP",
        options,
        Box::new(move |cc| {
            // Instala o ctx no controle para a thread de IPC pedir repaint.
            if let Ok(mut slot) = control.ctx.lock() {
                *slot = Some(cc.egui_ctx.clone());
            }
            Ok(Box::new(RustSipApp::new_remote(
                cc,
                sip_state,
                audio_state,
                config,
                client,
                control,
            )))
        }),
    );
    // O gui.lock só é solto agora, após a janela fechar — mantê-lo vivo durante
    // todo o run_native é o que garante a instância única.
    drop(gui_lock);
    result
}

/// F3 (item 7 / M2): tenta reconectar ao daemon após uma queda, SEM entrar em
/// loop de respawn. O respawn automático é 1x POR VIDA da janela (`respawned`),
/// não por queda:
///   1. Se AINDA não respawnamos nesta vida: `ensure_daemon()` — respawna o
///      daemon UMA vez (ele próprio faz no máx 1 spawn + retry de connect por
///      3s). Marca `respawned=true`. Se COLAR, devolve o stream.
///      Se a 1ª tentativa de respawn NÃO colar, DESISTIMOS: seta `gave_up` (a
///      UI troca o banner para "reabra o RustSIP") e devolve None. A spec é
///      "1x; se falhar, para de respawnar".
///   2. Se JÁ respawnamos nesta vida (queda posterior de um daemon que subiu e
///      caiu): NÃO respawna de novo — só tenta `connect` a cada 1s. Se o daemon
///      voltar por conta própria (tray/systemd/usuário), reconectamos; senão o
///      banner fica visível sem fork-bomb. Isto fecha o loop infinito
///      respawn→connect→crash→respawn.
///
/// Para quando conectar (Some) ou quando desistir / um Shutdown-Quit for
/// anunciado (None).
fn reconnect_loop(control: &Arc<WindowControl>, respawned: &mut bool) -> Option<UnixStream> {
    if control.shutdown.load(Ordering::Relaxed) || control.expect_quit.load(Ordering::Relaxed) {
        return None;
    }

    if !*respawned {
        // (1) Primeira e ÚNICA tentativa de respawn nesta vida.
        *respawned = true;
        if let Ok(s) = ensure_daemon() {
            return Some(s);
        }
        // O respawn não colou: para de respawnar e avisa a UI.
        control.gave_up.store(true, Ordering::Relaxed);
        control.wake();
        return None;
    }

    // (2) Já gastamos o respawn: só reconecta ao socket existente (sem spawn).
    let path = socket_path();
    loop {
        if control.shutdown.load(Ordering::Relaxed) || control.expect_quit.load(Ordering::Relaxed) {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        if let Ok(s) = UnixStream::connect(&path) {
            return Some(s);
        }
    }
}

/// Loop de leitura do socket: Hello -> consome ServerMsg até EOF/erro.
fn ipc_client_loop(
    stream: UnixStream,
    sip_state: Arc<SipSharedState>,
    audio_state: Arc<AudioSharedState>,
    config: RemoteConfig,
    control: Arc<WindowControl>,
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
        apply_server_msg(msg, &sip_state, &audio_state, &config, &control);
    }
}

/// Aplica uma ServerMsg no estado espelho que a UI lê.
fn apply_server_msg(
    msg: ServerMsg,
    sip_state: &Arc<SipSharedState>,
    audio_state: &Arc<AudioSharedState>,
    config: &RemoteConfig,
    control: &Arc<WindowControl>,
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
            tray_available,
            ..
        } => {
            // F3 (item 5): guarda tray_available para o handler de close decidir
            // se fechar a janela também encerra o daemon.
            control
                .tray_available
                .store(tray_available, Ordering::Relaxed);
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
        // F3: tray "Mostrar"/auto-show e encerramento do daemon.
        ServerMsg::Raise => {
            control.raise.store(true, Ordering::Relaxed);
            control.wake();
        }
        ServerMsg::Shutdown => {
            control.shutdown.store(true, Ordering::Relaxed);
            control.wake();
        }
    }
}

/// Aplica FlagsWire nos átomos do estado de áudio espelho.
fn apply_flags(audio: &Arc<AudioSharedState>, flags: crate::ipc::FlagsWire) {
    audio.muted.store(flags.muted, Ordering::Relaxed);
    audio.on_hold.store(flags.on_hold, Ordering::Relaxed);
    audio.mode.store(flags.mode, Ordering::Relaxed);
}

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

use crate::audio::AudioEngine;
use crate::config::AppConfig;
use crate::sip::{CallPhase, RegistrationStatus, SipCommand, SipController, SipSharedState};
use anyhow::{Context, Result};
use chrono::Local;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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

    // Mantém o lock vivo explicitamente até aqui.
    drop(lock_file);
    let _ = std::fs::remove_file(&lock_path);
    result
}

/// Loop assíncrono principal do daemon.
async fn run_inner() -> Result<()> {
    // O daemon é o dono do config.
    let mut config = AppConfig::load();

    // Núcleo de áudio + SIP, reutilizados COMO ESTÃO (sem alterar audio.rs/sip.rs).
    let audio = AudioEngine::new();
    audio.state.set_mic_gain(config.audio.mic_gain);
    audio.state.set_speaker_volume(config.audio.speaker_volume);
    let sip = SipController::new(audio.clone());
    let state = sip.state.clone();

    // auto_connect: dispara REGISTER no boot, igual o ui.rs faz no new.
    if config.account.is_configured() && config.account.auto_connect {
        println!("[{}] [daemon] auto_connect: enviando Connect", ts());
        sip.send(SipCommand::Connect {
            account: config.account.clone(),
            input_dev: config.audio.input_device.clone(),
            output_dev: config.audio.output_device.clone(),
        });
    } else {
        println!(
            "[{}] [daemon] conta não configurada ou auto_connect off; sem REGISTER",
            ts()
        );
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
    let _ = tray_available;

    // Sinais: SIGTERM/SIGINT => Sair gracioso; SIGHUP ignorado.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("instalar handler SIGTERM")?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("instalar handler SIGINT")?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .context("instalar handler SIGHUP")?;

    // Drena o histórico periodicamente (o daemon é o único writer do config).
    let mut drain_tick = tokio::time::interval(Duration::from_secs(2));

    println!("[{}] [daemon] loop principal pronto", ts());
    loop {
        tokio::select! {
            _ = drain_tick.tick() => {
                drain_completed_calls(&state, &mut config);
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

    graceful_shutdown(&sip, &state, &audio, &mut config).await;
    // Drop do _tray_handle aqui remove o ícone da bandeja.
    println!("[{}] [daemon] encerrado", ts());
    Ok(())
}

/// Porta da lógica de `ui.rs::drain_completed_calls` para o daemon: move os
/// registros de chamada concluída para `config.history` e persiste. É o único
/// ponto que escreve o config no modo daemon.
fn drain_completed_calls(state: &Arc<SipSharedState>, config: &mut AppConfig) {
    let mut new_entries = Vec::new();
    if let Ok(mut q) = state.completed_calls.lock() {
        while let Some(rec) = q.pop_front() {
            new_entries.push(rec);
        }
    }
    if new_entries.is_empty() {
        return;
    }
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
    config: &mut AppConfig,
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

    let was_registered = matches!(
        state.reg_status.lock().map(|s| s.clone()),
        Ok(RegistrationStatus::Registered) | Ok(RegistrationStatus::Connecting)
    );
    if was_registered {
        println!("[{}] [daemon] un-REGISTER (Disconnect), timeout 2s", ts());
        sip.send(SipCommand::Disconnect);
        // Aguarda até 2s o estado virar Disconnected.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let done = matches!(
                state.reg_status.lock().map(|s| s.clone()),
                Ok(RegistrationStatus::Disconnected) | Ok(RegistrationStatus::Failed(_))
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

    // NOTA F1: aqui também será feito o unlink do socket IPC, quando existir.
    let _ = std::io::stdout().flush();
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

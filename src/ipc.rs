//! Protocolo IPC daemon <-> janela (F1 da refatoração daemon+IPC, ADR-0001).
//!
//! Transporte: socket Unix em `$XDG_RUNTIME_DIR/rustsip/daemon.sock`. Framing
//! NDJSON (um objeto JSON por linha, terminado em `\n`). O daemon é o SERVIDOR
//! e o dono do estado; a janela é um CLIENTE que apenas LÊ o estado nesta fase
//! (os comandos chegam na F2). As variantes de comando/estado que só são
//! EMITIDAS na F2/F3 já podem ser declaradas aqui (marcadas `#[allow(dead_code)]`)
//! para o contrato ficar estável, mas NÃO são produzidas/consumidas ainda.
//!
//! SEGURANÇA: um frame `Welcome` carrega a `SipAccountConfig` inteira, que
//! inclui a SENHA. NUNCA logue um frame serializado. O `Debug` da
//! `SipAccountConfig` já redige a senha, mas o JSON cru NÃO — por isso
//! `write_frame`/`read_frame` jamais imprimem o conteúdo.

use crate::config::{AudioConfig, Contact, CallHistoryEntry, SipAccountConfig};
use crate::sip::{CallPhase, RegistrationStatus, SipLogEntry};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::time::Instant;

/// Versão do protocolo. Bump a cada mudança incompatível no wire.
pub const PROTO_VERSION: u32 = 1;

/// Teto de tamanho de uma linha/frame (256 KiB). Uma linha maior é rejeitada e
/// a conexão deve ser derrubada — evita que um peer (ou corrupção) force a
/// leitura de um frame arbitrariamente grande para a memória.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

/// Mensagens CLIENTE -> SERVIDOR. Na F1 o cliente só manda `Hello`; os comandos
/// (`Connect`, `Dial`, ...) entram na F2. Mantidos fora deste enum por ora.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t")]
pub enum ClientMsg {
    /// Handshake inicial. `exe_mtime` é o mtime do executável do cliente, para
    /// o daemon detectar binário obsoleto (`stale_exe`) e avisar a janela.
    Hello { proto: u32, exe_mtime: u64 },
}

/// Mensagens SERVIDOR -> CLIENTE.
///
/// `Welcome` é o snapshot completo enviado uma vez logo após o `Hello`; as
/// demais variantes são deltas emitidos pelo Publisher quando algo muda.
///
/// TAG ADJACENTE (`tag="t", content="c"`) e não interna: várias variantes são
/// newtype carregando `Option`/`Vec`/enum (`PublicAddr(Option<String>)`,
/// `Contacts(Vec<..>)`, `Reg(RegistrationStatus)`). A tag INTERNA do serde_json
/// recusa um newtype cujo conteúdo não serializa como mapa (Option/Vec/
/// primitivo) — a adjacente põe o payload sob `c` e aceita qualquer tipo.
///
/// As variantes `AudioCfg`/`Contacts`/`History`/`Account`/`Raise`/`Shutdown`
/// fazem parte do contrato mas só são EMITIDAS a partir da F2/F3 — declaradas
/// aqui para estabilidade do protocolo, com `#[allow(dead_code)]` onde o
/// construtor ainda não é exercido.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", content = "c")]
// Welcome é grande (snapshot completo) e as demais variantes pequenas; boxear
// o payload não vale a complexidade pois Welcome é enviado uma vez por conexão.
#[allow(clippy::large_enum_variant)]
pub enum ServerMsg {
    /// Snapshot inicial completo do estado do daemon.
    Welcome {
        proto: u32,
        daemon_pid: u32,
        /// true se o mtime do exe do cliente diverge do exe do daemon.
        stale_exe: bool,
        /// true se o daemon tem bandeja (SNI) ativa.
        tray_available: bool,
        account: SipAccountConfig,
        audio: AudioConfig,
        contacts: Vec<Contact>,
        history: Vec<CallHistoryEntry>,
        password_needs_reentry: bool,
        reg: RegistrationStatus,
        call: CallPhaseWire,
        public_addr: Option<String>,
        flags: FlagsWire,
        /// Sequência de log no instante do snapshot — o cliente usa para alinhar
        /// os `LogAppend` subsequentes.
        log_seq: u64,
    },
    /// Uma nova entrada de log SIP foi anexada (seq monotônico).
    LogAppend { seq: u64, entry: SipLogEntryWire },
    /// Mudança de status de registro.
    Reg(RegistrationStatus),
    /// Mudança de fase de chamada.
    Call(CallPhaseWire),
    /// Mudança de flags (mute/hold/mode).
    Flags(FlagsWire),
    /// Endereço público detectado (STUN) mudou.
    PublicAddr(Option<String>),
    /// Telemetria de alta frequência: níveis de VU e contadores RTP. Limitada a
    /// ~20/s pelo Publisher; é a única variante que pode ser descartada sob
    /// backpressure (try_send).
    Hot { mic: f32, spk: f32, tx: u64, rx: u64 },

    // --- Declaradas para o contrato; EMITIDAS só na F2/F3 ---
    /// Config de áudio mudou (F2+).
    #[allow(dead_code)]
    AudioCfg(AudioConfig),
    /// Lista de contatos mudou (F2+).
    #[allow(dead_code)]
    Contacts(Vec<Contact>),
    /// Histórico mudou (F2+).
    #[allow(dead_code)]
    History(Vec<CallHistoryEntry>),
    /// Conta/credenciais mudaram (F2+).
    #[allow(dead_code)]
    Account(SipAccountConfig),
    /// Pedido do daemon para a janela se trazer à frente (F3, tray "Mostrar").
    #[allow(dead_code)]
    Raise,
    /// Daemon vai encerrar; a janela deve fechar (F3).
    #[allow(dead_code)]
    Shutdown,
}

/// Espelho serializável de `CallPhase`. A única diferença estrutural é o
/// `InCall`: o original carrega `connected_at: Instant` (não serializável);
/// o wire carrega `elapsed_ms: u64` (milissegundos desde o início da chamada
/// no instante do envio). O cliente reconstrói um `Instant` aproximado
/// subtraindo `elapsed_ms` de `Instant::now()` no momento da recepção.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "p")]
pub enum CallPhaseWire {
    Idle,
    Dialing {
        remote_number: String,
        status_text: String,
    },
    Incoming {
        remote_number: String,
        remote_name: String,
    },
    InCall {
        remote_number: String,
        remote_name: String,
        codec_name: String,
        elapsed_ms: u64,
    },
}

impl From<&CallPhase> for CallPhaseWire {
    fn from(p: &CallPhase) -> Self {
        match p {
            CallPhase::Idle => CallPhaseWire::Idle,
            CallPhase::Dialing {
                remote_number,
                status_text,
            } => CallPhaseWire::Dialing {
                remote_number: remote_number.clone(),
                status_text: status_text.clone(),
            },
            CallPhase::Incoming {
                remote_number,
                remote_name,
            } => CallPhaseWire::Incoming {
                remote_number: remote_number.clone(),
                remote_name: remote_name.clone(),
            },
            CallPhase::InCall {
                remote_number,
                remote_name,
                codec_name,
                connected_at,
            } => CallPhaseWire::InCall {
                remote_number: remote_number.clone(),
                remote_name: remote_name.clone(),
                codec_name: codec_name.clone(),
                // Saturating: um relógio que recuou não vira duração negativa.
                elapsed_ms: connected_at.elapsed().as_millis() as u64,
            },
        }
    }
}

impl CallPhaseWire {
    /// Reconstrói um `CallPhase` local. Para `InCall`, ancora o `connected_at`
    /// em `Instant::now() - elapsed_ms` — o relógio de parede do cliente, não o
    /// do daemon, mas a DURAÇÃO exibida fica correta (±latência do IPC).
    pub fn into_call_phase(self) -> CallPhase {
        match self {
            CallPhaseWire::Idle => CallPhase::Idle,
            CallPhaseWire::Dialing {
                remote_number,
                status_text,
            } => CallPhase::Dialing {
                remote_number,
                status_text,
            },
            CallPhaseWire::Incoming {
                remote_number,
                remote_name,
            } => CallPhase::Incoming {
                remote_number,
                remote_name,
            },
            CallPhaseWire::InCall {
                remote_number,
                remote_name,
                codec_name,
                elapsed_ms,
            } => CallPhase::InCall {
                remote_number,
                remote_name,
                codec_name,
                connected_at: Instant::now()
                    - std::time::Duration::from_millis(elapsed_ms),
            },
        }
    }
}

/// Espelho serializável de `SipLogEntry`. O `direction` do original é
/// `&'static str` ("TX"/"RX"/"INFO"/"ERR"); no wire vira `String`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SipLogEntryWire {
    pub timestamp: String,
    pub direction: String,
    pub summary: String,
    pub raw: String,
}

impl From<&SipLogEntry> for SipLogEntryWire {
    fn from(e: &SipLogEntry) -> Self {
        SipLogEntryWire {
            timestamp: e.timestamp.clone(),
            direction: e.direction.to_string(),
            summary: e.summary.clone(),
            raw: e.raw.clone(),
        }
    }
}

impl SipLogEntryWire {
    /// Converte de volta para `SipLogEntry`. Como o original exige
    /// `&'static str` em `direction`, mapeamos os rótulos conhecidos para
    /// literais estáticos; um rótulo desconhecido cai em "INFO".
    pub fn into_log_entry(self) -> SipLogEntry {
        let direction: &'static str = match self.direction.as_str() {
            "TX" => "TX",
            "RX" => "RX",
            "ERR" => "ERR",
            _ => "INFO",
        };
        SipLogEntry {
            timestamp: self.timestamp,
            direction,
            summary: self.summary,
            raw: self.raw,
        }
    }
}

/// Flags de áudio/sessão espelhadas (mute, hold, modo). `mode` é o `AtomicU8`
/// do `AudioSharedState` (MODE_IDLE / MODE_CALL / MODE_LOOPBACK_TEST).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlagsWire {
    pub muted: bool,
    pub on_hold: bool,
    pub mode: u8,
}

/// Serializa `msg` como uma linha NDJSON (JSON + `\n`) em `w`.
///
/// NUNCA logar `line` — pode conter a senha (Welcome).
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    if line.len() > MAX_FRAME_BYTES {
        bail!("frame IPC excede {} bytes ({} bytes)", MAX_FRAME_BYTES, line.len());
    }
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()?;
    Ok(())
}

/// Variante ASSÍNCRONA (tokio) de `write_frame`, para o servidor no daemon
/// escrever no half de escrita do UnixStream. Mesmo contrato: serializa + `\n`,
/// aplica o teto de `MAX_FRAME_BYTES`, e NUNCA loga o conteúdo.
pub async fn write_frame_async<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: tokio::io::AsyncWriteExt + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_vec(msg)?;
    if line.len() > MAX_FRAME_BYTES {
        bail!("frame IPC excede {} bytes ({} bytes)", MAX_FRAME_BYTES, line.len());
    }
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

/// Lê uma linha NDJSON de `r` com teto de `MAX_FRAME_BYTES`, desserializando em
/// `T`. Retorna `Ok(None)` em EOF limpo; `Err` se a linha exceder o teto (o
/// chamador deve desconectar) ou o JSON for inválido.
///
/// Usa `take(MAX_FRAME_BYTES + 1)` para limitar a leitura: se encher o teto sem
/// achar `\n`, a linha é grande demais e viramos erro em vez de crescer a
/// memória indefinidamente.
pub fn read_frame<R: BufRead, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<Option<T>> {
    let mut buf = Vec::with_capacity(1024);
    // .take() limita quanto lemos até o \n (inclusive). Lemos no máx teto+1
    // bytes: se o \n não apareceu nesse limite, a linha estourou o teto.
    let n = {
        let by_ref: &mut R = r;
        let mut limited = std::io::Read::take(by_ref, (MAX_FRAME_BYTES as u64) + 1);
        limited.read_until(b'\n', &mut buf)?
    };
    if n == 0 {
        return Ok(None); // EOF limpo
    }
    let had_newline = buf.last() == Some(&b'\n');
    if had_newline {
        buf.pop();
    }
    // Sem newline após teto+1 bytes => linha grande demais (ou EOF no meio de
    // uma linha gigante). Rejeita.
    if !had_newline && buf.len() > MAX_FRAME_BYTES {
        bail!("linha IPC excede o teto de {} bytes", MAX_FRAME_BYTES);
    }
    if buf.len() > MAX_FRAME_BYTES {
        bail!("linha IPC excede o teto de {} bytes", MAX_FRAME_BYTES);
    }
    let msg = serde_json::from_slice::<T>(&buf)?;
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SipAccountConfig, TransportProtocol};
    use std::io::BufReader;

    fn sample_account() -> SipAccountConfig {
        SipAccountConfig {
            display_name: "Teste".into(),
            username: "100".into(),
            auth_id: "100".into(),
            password: "segredo".into(),
            server: "pbx.example.com:5060".into(),
            domain: "example.com".into(),
            transport: TransportProtocol::Udp,
            register_expires: 300,
            use_stun: true,
            stun_server: "stun.l.google.com:19302".into(),
            auto_connect: true,
        }
    }

    /// Round-trip de cada variante de ClientMsg via framing NDJSON.
    #[test]
    fn test_clientmsg_roundtrip() {
        let msgs = vec![ClientMsg::Hello {
            proto: PROTO_VERSION,
            exe_mtime: 1_700_000_000,
        }];
        for m in msgs {
            let mut buf = Vec::new();
            write_frame(&mut buf, &m).expect("write");
            let mut r = BufReader::new(&buf[..]);
            let got: ClientMsg = read_frame(&mut r).expect("read").expect("some");
            assert_eq!(got, m);
        }
    }

    /// Round-trip de cada variante de ServerMsg, incluindo as declaradas-mas-
    /// não-emitidas-ainda (AudioCfg/Contacts/History/Account/Raise/Shutdown).
    #[test]
    fn test_servermsg_roundtrip_all_variants() {
        let variants = vec![
            ServerMsg::Welcome {
                proto: PROTO_VERSION,
                daemon_pid: 4242,
                stale_exe: false,
                tray_available: true,
                account: sample_account(),
                audio: AudioConfig::default(),
                contacts: vec![Contact {
                    id: "c1".into(),
                    name: "Alice".into(),
                    number: "200".into(),
                    notes: "".into(),
                }],
                history: vec![],
                password_needs_reentry: false,
                reg: RegistrationStatus::Registered,
                call: CallPhaseWire::Idle,
                public_addr: Some("1.2.3.4:5060".into()),
                flags: FlagsWire { muted: false, on_hold: false, mode: 0 },
                log_seq: 7,
            },
            ServerMsg::LogAppend {
                seq: 8,
                entry: SipLogEntryWire {
                    timestamp: "10:00:00.000".into(),
                    direction: "TX".into(),
                    summary: "REGISTER".into(),
                    raw: "REGISTER sip:...".into(),
                },
            },
            ServerMsg::Reg(RegistrationStatus::Failed("403 Forbidden".into())),
            ServerMsg::Call(CallPhaseWire::Dialing {
                remote_number: "300".into(),
                status_text: "chamando".into(),
            }),
            ServerMsg::Flags(FlagsWire { muted: true, on_hold: false, mode: 1 }),
            ServerMsg::PublicAddr(None),
            ServerMsg::Hot { mic: 0.5, spk: 0.25, tx: 100, rx: 98 },
            ServerMsg::AudioCfg(AudioConfig::default()),
            ServerMsg::Contacts(vec![]),
            ServerMsg::History(vec![]),
            ServerMsg::Account(sample_account()),
            ServerMsg::Raise,
            ServerMsg::Shutdown,
        ];
        for m in variants {
            let mut buf = Vec::new();
            write_frame(&mut buf, &m).expect("write");
            let mut r = BufReader::new(&buf[..]);
            let got: ServerMsg = read_frame(&mut r).expect("read").expect("some");
            assert_eq!(got, m, "variante não sobreviveu ao round-trip");
        }
    }

    /// Uma linha maior que o teto de 256 KiB é rejeitada na leitura.
    #[test]
    fn test_oversized_line_rejected() {
        // Monta uma linha de > MAX_FRAME_BYTES que NÃO contém '\n' até o teto.
        let mut big = vec![b'x'; MAX_FRAME_BYTES + 10];
        big.push(b'\n');
        let mut r = BufReader::new(&big[..]);
        let res: Result<Option<ServerMsg>> = read_frame(&mut r);
        assert!(res.is_err(), "linha > teto deveria ser rejeitada");
    }

    /// Um frame no limite exato ainda é lido (teto é inclusivo para o conteúdo,
    /// o estouro é a partir de MAX_FRAME_BYTES+1 bytes de conteúdo).
    #[test]
    fn test_write_frame_rejects_oversized_payload() {
        // Payload que serializa acima do teto deve falhar no write.
        let huge = "y".repeat(MAX_FRAME_BYTES + 1);
        let msg = ServerMsg::PublicAddr(Some(huge));
        let mut buf = Vec::new();
        let res = write_frame(&mut buf, &msg);
        assert!(res.is_err(), "payload acima do teto deveria falhar no write");
    }

    /// InCall -> elapsed_ms -> Instant reconstruído, com tolerância de ±50ms.
    #[test]
    fn test_callphase_incall_elapsed_roundtrip() {
        let original_connected = Instant::now() - std::time::Duration::from_secs(12);
        let phase = CallPhase::InCall {
            remote_number: "300".into(),
            remote_name: "Bob".into(),
            codec_name: "PCMU".into(),
            connected_at: original_connected,
        };
        let wire: CallPhaseWire = (&phase).into();
        // Serializa e desserializa (exercita o wire de verdade).
        let mut buf = Vec::new();
        write_frame(&mut buf, &ServerMsg::Call(wire.clone())).expect("write");
        let mut r = BufReader::new(&buf[..]);
        let got: ServerMsg = read_frame(&mut r).expect("read").expect("some");
        let ServerMsg::Call(got_wire) = got else {
            panic!("esperava Call");
        };
        let reconstructed = got_wire.into_call_phase();
        let CallPhase::InCall { connected_at, .. } = reconstructed else {
            panic!("esperava InCall");
        };
        // A duração reconstruída deve bater com a original dentro de ±50ms.
        let orig_elapsed = original_connected.elapsed().as_millis() as i64;
        let new_elapsed = connected_at.elapsed().as_millis() as i64;
        let drift = (orig_elapsed - new_elapsed).abs();
        assert!(
            drift <= 50,
            "drift de reconstrução {drift}ms excede ±50ms (orig={orig_elapsed} new={new_elapsed})"
        );
    }
}

use crate::audio::AudioEngine;
use crate::codec::AudioCodec;
use crate::config::{SipAccountConfig, TransportProtocol};
use crate::rtp::{allocate_rtp_socket, spawn_rtp_session, RtpSessionHandle, RtpSocketAllocation, RtpStats};
use chrono::Local;
use md5::{Digest, Md5};
use rand::Rng;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationStatus {
    Disconnected,
    Connecting,
    Registered,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallPhase {
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
        connected_at: Instant,
    },
}

#[derive(Debug, Clone)]
pub struct SipLogEntry {
    pub timestamp: String,
    pub direction: &'static str, // "TX", "RX", "INFO", "ERR"
    pub summary: String,
    pub raw: String,
}

#[derive(Debug, Clone)]
pub struct CompletedCallRecord {
    pub number: String,
    pub name: String,
    pub direction: crate::config::CallDirection,
    pub duration_secs: u64,
}

pub struct SipSharedState {
    pub reg_status: Mutex<RegistrationStatus>,
    pub call_phase: Mutex<CallPhase>,
    pub logs: Mutex<VecDeque<SipLogEntry>>,
    pub completed_calls: Mutex<VecDeque<CompletedCallRecord>>,
    pub rtp_stats: Arc<RtpStats>,
    pub detected_public_addr: Mutex<Option<String>>,
}

impl SipSharedState {
    pub fn new(rtp_stats: Arc<RtpStats>) -> Arc<Self> {
        Arc::new(Self {
            reg_status: Mutex::new(RegistrationStatus::Disconnected),
            call_phase: Mutex::new(CallPhase::Idle),
            logs: Mutex::new(VecDeque::with_capacity(200)),
            completed_calls: Mutex::new(VecDeque::new()),
            rtp_stats,
            detected_public_addr: Mutex::new(None),
        })
    }

    pub fn add_log(&self, direction: &'static str, summary: impl Into<String>, raw: impl Into<String>) {
        let entry = SipLogEntry {
            timestamp: Local::now().format("%H:%M:%S%.3f").to_string(),
            direction,
            summary: summary.into(),
            raw: raw.into(),
        };
        if let Ok(mut logs) = self.logs.lock() {
            if logs.len() >= 250 {
                logs.pop_front();
            }
            logs.push_back(entry);
        }
    }

    pub fn set_reg_status(&self, status: RegistrationStatus) {
        if let Ok(mut s) = self.reg_status.lock() {
            *s = status;
        }
    }

    pub fn set_call_phase(&self, phase: CallPhase) {
        if let Ok(mut p) = self.call_phase.lock() {
            *p = phase;
        }
    }
}

#[derive(Debug, Clone)]
pub enum SipCommand {
    Connect {
        account: SipAccountConfig,
        input_dev: Option<String>,
        output_dev: Option<String>,
    },
    Disconnect,
    Dial {
        target: String,
        input_dev: Option<String>,
        output_dev: Option<String>,
    },
    Answer {
        input_dev: Option<String>,
        output_dev: Option<String>,
    },
    Hangup,
    SendDtmf(char),
}

#[derive(Clone)]
pub struct SipController {
    pub state: Arc<SipSharedState>,
    cmd_tx: mpsc::UnboundedSender<SipCommand>,
}

impl SipController {
    pub fn new(audio: AudioEngine) -> Self {
        let rtp_stats = RtpStats::new();
        let state = SipSharedState::new(rtp_stats);
        let state_bg = state.clone();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        std::thread::Builder::new()
            .name("rustsip-tokio".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("Falha ao iniciar Tokio runtime");
                rt.block_on(run_sip_supervisor(state_bg, audio, cmd_rx));
            })
            .expect("Falha ao criar thread SIP");

        Self { state, cmd_tx }
    }

    pub fn send(&self, cmd: SipCommand) {
        let _ = self.cmd_tx.send(cmd);
    }
}

/// Estrutura de uma mensagem SIP decodificada
#[derive(Debug, Clone)]
struct SipMessage {
    start_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl SipMessage {
    fn parse(raw: &str) -> Option<Self> {
        let (head_part, body_part) = if let Some(idx) = raw.find("\r\n\r\n") {
            (&raw[..idx], &raw[idx + 4..])
        } else if let Some(idx) = raw.find("\n\n") {
            (&raw[..idx], &raw[idx + 2..])
        } else {
            (raw, "")
        };

        let mut lines = head_part.lines();
        let start_line = lines.next()?.trim().to_string();
        if start_line.is_empty() {
            return None;
        }

        let mut headers = Vec::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }

        Some(Self {
            start_line,
            headers,
            body: body_part.to_string(),
        })
    }

    fn is_response(&self) -> bool {
        self.start_line.starts_with("SIP/2.0 ")
    }

    fn status_code(&self) -> Option<u16> {
        if !self.is_response() {
            return None;
        }
        let mut parts = self.start_line.split_whitespace();
        let _ver = parts.next()?;
        parts.next()?.parse::<u16>().ok()
    }

    fn method(&self) -> Option<&str> {
        if self.is_response() {
            return None;
        }
        self.start_line.split_whitespace().next()
    }

    fn header(&self, name: &str) -> Option<&str> {
        let compact = match name.to_ascii_lowercase().as_str() {
            "via" => Some("v"),
            "from" => Some("f"),
            "to" => Some("t"),
            "call-id" => Some("i"),
            "contact" => Some("m"),
            "content-length" => Some("l"),
            "content-type" => Some("c"),
            _ => None,
        };

        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case(name) {
                return Some(v.as_str());
            }
            if let Some(short) = compact {
                if k.eq_ignore_ascii_case(short) {
                    return Some(v.as_str());
                }
            }
        }
        None
    }

    fn all_headers(&self, name: &str) -> Vec<&str> {
        let mut out = Vec::new();
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case(name) {
                out.push(v.as_str());
            }
        }
        out
    }

    fn cseq_method(&self) -> Option<&str> {
        let cseq = self.header("CSeq")?;
        cseq.split_whitespace().nth(1)
    }
}

#[derive(Debug, Clone)]
struct DigestChallenge {
    realm: String,
    nonce: String,
    qop: Option<String>,
    opaque: Option<String>,
    algorithm: String,
    is_proxy: bool,
    stale: bool,
}

fn parse_digest_challenge(header_val: &str, is_proxy: bool) -> Option<DigestChallenge> {
    let rest = header_val.trim();
    let rest = if let Some(r) = rest.strip_prefix("Digest ") {
        r
    } else if let Some(r) = rest.strip_prefix("digest ") {
        r
    } else {
        rest
    };

    let mut realm = None;
    let mut nonce = None;
    let mut qop = None;
    let mut opaque = None;
    let mut algorithm = "MD5".to_string();
    let mut stale = false;

    for part in split_auth_params(rest) {
        if let Some((k, v)) = part.split_once('=') {
            let key = k.trim();
            let val = v.trim().trim_matches('"').to_string();
            if key.eq_ignore_ascii_case("realm") {
                realm = Some(val);
            } else if key.eq_ignore_ascii_case("nonce") {
                nonce = Some(val);
            } else if key.eq_ignore_ascii_case("qop") {
                qop = Some(val);
            } else if key.eq_ignore_ascii_case("opaque") {
                opaque = Some(val);
            } else if key.eq_ignore_ascii_case("algorithm") {
                algorithm = val;
            } else if key.eq_ignore_ascii_case("stale") {
                // RFC 3261/2617: stale=true => nonce expirou, mas credenciais
                // estavam corretas -> reautenticar com o nonce novo é válido
                // (NÃO é senha errada).
                stale = val.eq_ignore_ascii_case("true");
            }
        }
    }

    Some(DigestChallenge {
        realm: realm?,
        nonce: nonce?,
        qop,
        opaque,
        algorithm,
        is_proxy,
        stale,
    })
}

fn split_auth_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut curr = String::new();
    let mut in_quotes = false;
    for ch in s.chars() {
        if ch == '"' {
            in_quotes = !in_quotes;
            curr.push(ch);
        } else if ch == ',' && !in_quotes {
            out.push(curr.trim().to_string());
            curr.clear();
        } else {
            curr.push(ch);
        }
    }
    if !curr.trim().is_empty() {
        out.push(curr.trim().to_string());
    }
    out
}

fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let res = hasher.finalize();
    format!("{:x}", res)
}

fn build_authorization_header(
    challenge: &DigestChallenge,
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    nc_counter: u32,
) -> (String, String) {
    let ha1 = md5_hex(&format!("{}:{}:{}", username, challenge.realm, password));
    let ha2 = md5_hex(&format!("{}:{}", method, uri));

    let use_qop_auth = challenge
        .qop
        .as_deref()
        .map(|q| q.split(',').any(|t| t.trim().eq_ignore_ascii_case("auth")))
        .unwrap_or(false);

    let cnonce = random_hex(8);
    let nc_str = format!("{:08x}", nc_counter);

    let response = if use_qop_auth {
        md5_hex(&format!(
            "{}:{}:{}:{}:auth:{}",
            ha1, challenge.nonce, nc_str, cnonce, ha2
        ))
    } else {
        md5_hex(&format!("{}:{}:{}", ha1, challenge.nonce, ha2))
    };

    let mut header_val = format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm={}",
        username, challenge.realm, challenge.nonce, uri, response, challenge.algorithm
    );

    if use_qop_auth {
        header_val.push_str(&format!(", qop=auth, nc={}, cnonce=\"{}\"", nc_str, cnonce));
    }

    if let Some(ref opq) = challenge.opaque {
        header_val.push_str(&format!(", opaque=\"{}\"", opq));
    }

    let header_name = if challenge.is_proxy {
        "Proxy-Authorization".to_string()
    } else {
        "Authorization".to_string()
    };

    (header_name, header_val)
}

fn random_hex(len: usize) -> String {
    let mut rng = rand::thread_rng();
    let mut s = String::with_capacity(len);
    for _ in 0..len {
        let n: u8 = rng.gen_range(0..16);
        s.push(std::char::from_digit(n as u32, 16).unwrap_or('a'));
    }
    s
}

#[derive(Debug, Clone)]
struct SdpMediaInfo {
    remote_ip: IpAddr,
    remote_port: u16,
    codec: AudioCodec,
    dtmf_pt: u8,
}

fn build_sdp_offer(local_ip: &str, rtp_port: u16, preferred_codec: Option<AudioCodec>) -> String {
    let sess_id: u32 = rand::thread_rng().gen_range(100000..999999);
    let codecs_line = match preferred_codec {
        Some(AudioCodec::Pcma) => "8 0 101",
        _ => "0 8 101",
    };
    format!(
        "v=0\r\n\
         o=rustsip {sess_id} {sess_id} IN IP4 {local_ip}\r\n\
         s=RustSIP\r\n\
         c=IN IP4 {local_ip}\r\n\
         t=0 0\r\n\
         m=audio {rtp_port} RTP/AVP {codecs_line}\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:8 PCMA/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-16\r\n\
         a=ptime:20\r\n\
         a=sendrecv\r\n"
    )
}

fn parse_sdp(sdp: &str) -> Option<SdpMediaInfo> {
    let mut conn_ip: Option<IpAddr> = None;
    let mut audio_port: Option<u16> = None;
    let mut payload_types: Vec<u8> = Vec::new();
    let mut dtmf_pt: u8 = 101;

    for raw_line in sdp.lines() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("c=IN IP4 ") {
            let ip_str = rest.split_whitespace().next().unwrap_or("");
            if let Ok(ip) = ip_str.parse::<IpAddr>() {
                conn_ip = Some(ip);
            }
        } else if let Some(rest) = line.strip_prefix("m=audio ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if let Some(port_str) = parts.first() {
                audio_port = port_str.parse::<u16>().ok();
            }
            for &pt_str in parts.iter().skip(2) {
                if let Ok(pt) = pt_str.parse::<u8>() {
                    payload_types.push(pt);
                }
            }
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            let l = rest.to_ascii_lowercase();
            if l.contains("telephone-event") {
                if let Some(pt_str) = l.split_whitespace().next() {
                    if let Ok(pt) = pt_str.parse::<u8>() {
                        dtmf_pt = pt;
                    }
                }
            }
        }
    }

    let remote_ip = conn_ip?;
    let remote_port = audio_port?;
    if remote_port == 0 {
        return None;
    }

    let mut chosen_codec = AudioCodec::Pcmu;
    for &pt in &payload_types {
        if let Some(c) = AudioCodec::from_payload_type(pt) {
            chosen_codec = c;
            break;
        }
    }

    Some(SdpMediaInfo {
        remote_ip,
        remote_port,
        codec: chosen_codec,
        dtmf_pt,
    })
}

fn extract_tag(header_val: &str) -> Option<String> {
    for part in header_val.split(';') {
        let p = part.trim();
        if let Some(tag) = p.strip_prefix("tag=") {
            return Some(tag.trim().to_string());
        }
    }
    None
}

fn extract_uri_from_header(header_val: &str) -> String {
    if let Some(start) = header_val.find('<') {
        if let Some(end) = header_val[start + 1..].find('>') {
            return header_val[start + 1..start + 1 + end].to_string();
        }
    }
    header_val
        .split(';')
        .next()
        .unwrap_or(header_val)
        .trim()
        .to_string()
}

fn extract_caller_display_and_user(from_header: &str) -> (String, String) {
    let uri = extract_uri_from_header(from_header);
    let user = uri
        .strip_prefix("sip:")
        .or_else(|| uri.strip_prefix("sips:"))
        .unwrap_or(&uri)
        .split('@')
        .next()
        .unwrap_or("Desconhecido")
        .to_string();

    let mut display = String::new();
    if let Some(idx) = from_header.find('<') {
        display = from_header[..idx].trim().trim_matches('"').to_string();
    }
    if display.is_empty() {
        display = user.clone();
    }
    (display, user)
}

/// Descobre qual IP local da máquina Linux roteia para o servidor FreeSWITCH.
///
/// Retorna `None` quando não foi possível determinar a rota (bind/connect UDP
/// falhou). O chamador DEVE tratar `None` como erro visível e abortar a sessão —
/// cair em "127.0.0.1" silenciosamente anuncia loopback em Via/Contact/SDP e
/// quebra o registro/áudio sem nenhuma mensagem de erro.
fn discover_local_ip_for(server_addr: SocketAddr) -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(server_addr).ok()?;
    let local = sock.local_addr().ok()?;
    let ip = local.ip();
    // Um IP não-especificado/loopback aqui não roteia para um servidor remoto real.
    if ip.is_unspecified() || ip.is_loopback() {
        return None;
    }
    Some(ip.to_string())
}

/// Extrai `received=` e `rport=` do cabeçalho Via retornado pelo FreeSWITCH
fn extract_via_received_rport(via: &str) -> (Option<String>, Option<u16>) {
    let mut received = None;
    let mut rport = None;
    for part in via.split(';') {
        let p = part.trim();
        if let Some(ip) = p.strip_prefix("received=") {
            if !ip.is_empty() {
                received = Some(ip.to_string());
            }
        } else if let Some(port_s) = p.strip_prefix("rport=") {
            if let Ok(port) = port_s.parse::<u16>() {
                rport = Some(port);
            }
        }
    }
    (received, rport)
}

#[allow(dead_code)]
struct ActiveCallDialog {
    call_id: String,
    local_tag: String,
    remote_tag: Option<String>,
    from_header: String,
    to_header: String,
    remote_target_uri: String,
    remote_number: String,
    remote_name: String,
    route_set: Vec<String>,
    cseq: u32,
    last_invite_branch: String,
    last_invite_uri: String,
    last_invite_auth_line: String,
    last_invite_raw: String,
    is_incoming: bool,
    incoming_invite_via: Vec<String>,
    incoming_invite_cseq: String,
    incoming_sdp: Option<SdpMediaInfo>,
    rtp_alloc: Option<RtpSocketAllocation>,
    rtp_handle: Option<RtpSessionHandle>,
    rtp_remote_addr: Option<SocketAddr>,
    negotiated_dtmf_pt: u8,
    connected_at: Option<Instant>,
    auth_attempts: u32,
    input_dev: Option<String>,
    output_dev: Option<String>,
}

/// Dados que um `send_*` de REQUISIÇÃO CLIENTE devolve para o loop principal
/// registrar uma transação cliente e poder retransmitir (RFC 3261 §17.1).
/// Só usado em UDP — TCP é confiável e dispensa retransmissão.
struct ClientTxn {
    method: String,
    cseq_num: u32,
    branch: String,
    raw: String,
    // Próximo instante de reenvio e intervalo atual (dobra até T2).
    next_fire: Instant,
    interval: Duration,
}

// Temporizadores de transação SIP (RFC 3261 §17.1.1.1 / §17.1.2.1).
const T1: Duration = Duration::from_millis(500); // RTT estimado inicial
const T2: Duration = Duration::from_millis(4000); // teto do intervalo de reenvio
// Teto absoluto por transação (~ 64*T1 = 32s é o ideal RFC; usamos um teto mais
// curto para não prender a UI). Ao estourar, a transação é abandonada.
const TXN_TOTAL_TIMEOUT: Duration = Duration::from_millis(8000);

/// Tabela de transações cliente ativas aguardando resposta (apenas UDP).
#[derive(Default)]
struct TxTable {
    txns: Vec<(ClientTxn, Instant)>, // (txn, criada_em)
}

impl TxTable {
    /// Registra uma nova transação cliente para retransmissão.
    fn register(&mut self, txn: ClientTxn) {
        // Uma nova transação do mesmo método+cseq substitui a anterior
        // (ex.: REGISTER reenviado com Authorization após 401).
        self.txns
            .retain(|(t, _)| !(t.method == txn.method && t.cseq_num == txn.cseq_num));
        self.txns.push((txn, Instant::now()));
    }

    /// Remove a transação casada por CSeq (num+método). O branch do Via é
    /// validado pelo chamador antes de chamar isto.
    fn ack_response(&mut self, cseq_num: u32, method: &str) {
        self.txns
            .retain(|(t, _)| !(t.cseq_num == cseq_num && t.method.eq_ignore_ascii_case(method)));
    }

    /// Procura a transação que casa com CSeq num+método E branch do Via.
    /// Retorna true se existe uma transação pendente correspondente.
    fn matches(&self, cseq_num: u32, method: &str, branch: &str) -> bool {
        self.txns.iter().any(|(t, _)| {
            t.cseq_num == cseq_num
                && t.method.eq_ignore_ascii_case(method)
                && (branch.is_empty() || t.branch == branch)
        })
    }

    /// Reenvia as transações vencidas; descarta (com log) as que estouraram o
    /// teto total. Retorna o próximo instante em que algo precisa disparar.
    fn tick(
        &mut self,
        now: Instant,
        out_tx: &mpsc::UnboundedSender<String>,
        state: &Arc<SipSharedState>,
    ) {
        let mut expired = Vec::new();
        for (txn, created) in self.txns.iter_mut() {
            if now.duration_since(*created) >= TXN_TOTAL_TIMEOUT {
                expired.push((txn.method.clone(), txn.cseq_num));
                continue;
            }
            if now >= txn.next_fire {
                let _ = out_tx.send(txn.raw.clone());
                state.add_log(
                    "TX",
                    format!("Retransmissão {} (CSeq {})", txn.method, txn.cseq_num),
                    "",
                );
                // Backoff exponencial com teto em T2 (RFC 3261 §17.1.2.1).
                txn.interval = (txn.interval * 2).min(T2);
                txn.next_fire = now + txn.interval;
            }
        }
        if !expired.is_empty() {
            self.txns.retain(|(_t, created)| {
                now.duration_since(*created) < TXN_TOTAL_TIMEOUT
            });
            for (method, cseq) in expired {
                state.add_log(
                    "ERR",
                    format!(
                        "Transação {} (CSeq {}) sem resposta — abandonada após {}ms",
                        method,
                        cseq,
                        TXN_TOTAL_TIMEOUT.as_millis()
                    ),
                    "",
                );
            }
        }
    }
}

/// Monta uma `ClientTxn` a partir de um pacote já construído.
fn make_txn(method: &str, cseq_num: u32, branch: &str, raw: &str) -> ClientTxn {
    ClientTxn {
        method: method.to_string(),
        cseq_num,
        branch: branch.to_string(),
        raw: raw.to_string(),
        next_fire: Instant::now() + T1,
        interval: T1,
    }
}

/// Extrai o `branch` do primeiro (topo) cabeçalho Via de uma mensagem.
fn top_via_branch(msg: &SipMessage) -> String {
    let Some(via) = msg.header("Via") else {
        return String::new();
    };
    for part in via.split(';') {
        let p = part.trim();
        if let Some(b) = p.strip_prefix("branch=") {
            return b.trim().to_string();
        }
    }
    String::new()
}

/// Extrai o número do CSeq (primeiro token) de uma mensagem.
fn cseq_num(msg: &SipMessage) -> Option<u32> {
    msg.header("CSeq")?.split_whitespace().next()?.parse().ok()
}

async fn run_sip_supervisor(
    state: Arc<SipSharedState>,
    audio: AudioEngine,
    mut cmd_rx: mpsc::UnboundedReceiver<SipCommand>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        if let SipCommand::Connect {
            account,
            input_dev,
            output_dev,
        } = cmd
        {
            run_active_session(
                &state,
                &audio,
                account,
                input_dev,
                output_dev,
                &mut cmd_rx,
            )
            .await;
        }
    }
}

async fn run_active_session(
    state: &Arc<SipSharedState>,
    audio: &AudioEngine,
    account: SipAccountConfig,
    mut current_in_dev: Option<String>,
    mut current_out_dev: Option<String>,
    cmd_rx: &mut mpsc::UnboundedReceiver<SipCommand>,
) {
    state.set_reg_status(RegistrationStatus::Connecting);
    let server_str = account.effective_server_addr();
    let domain = account.effective_domain();

    state.add_log(
        "INFO",
        format!(
            "Resolvendo servidor FreeSWITCH {} ({})",
            server_str,
            account.transport.as_str()
        ),
        format!("Servidor: {}\nDomínio/Realm: {}\nUsuário: {}", server_str, domain, account.username),
    );

    let server_addr = match tokio::net::lookup_host(&server_str).await {
        Ok(addrs) => {
            let list: Vec<SocketAddr> = addrs.collect();
            match list
                .iter()
                .copied()
                .find(|a| a.is_ipv4())
                .or_else(|| list.first().copied())
            {
                Some(a) => a,
                None => {
                    let msg = format!("Não foi possível resolver IP para {}", server_str);
                    state.add_log("ERR", &msg, "");
                    state.set_reg_status(RegistrationStatus::Failed(msg));
                    return;
                }
            }
        }
        Err(e) => {
            let msg = format!("Erro DNS em {}: {}", server_str, e);
            state.add_log("ERR", &msg, "");
            state.set_reg_status(RegistrationStatus::Failed(msg));
            return;
        }
    };

    let local_ip = match discover_local_ip_for(server_addr) {
        Some(ip) => ip,
        None => {
            let msg = format!(
                "Não foi possível determinar o IP local de rota para {} \
                 (verifique conectividade de rede/rota). Sessão abortada para \
                 evitar anunciar 127.0.0.1 em Via/Contact/SDP.",
                server_addr
            );
            state.add_log("ERR", &msg, "");
            state.set_reg_status(RegistrationStatus::Failed(msg));
            return;
        }
    };
    let (net_tx, mut net_rx) = mpsc::unbounded_channel::<String>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    // Sinaliza ao loop principal que o transporte caiu (ex.: TCP fechado pelo
    // servidor). Sem isso o estado fica "Registrado" fantasma para sempre.
    let (link_down_tx, mut link_down_rx) = mpsc::unbounded_channel::<String>();

    // Inicializa Transporte UDP ou TCP
    let local_port: u16;
    let transport_task: tokio::task::JoinHandle<()>;

    match account.transport {
        TransportProtocol::Udp => {
            let sock = match UdpSocket::bind("0.0.0.0:0").await {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    let msg = format!("Erro ao abrir porta UDP local: {}", e);
                    state.add_log("ERR", &msg, "");
                    state.set_reg_status(RegistrationStatus::Failed(msg));
                    return;
                }
            };
            local_port = sock.local_addr().map(|a| a.port()).unwrap_or(5060);
            let sock_rx = sock.clone();
            let sock_tx = sock.clone();
            let net_tx_udp = net_tx.clone();

            transport_task = tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                loop {
                    tokio::select! {
                        Some(pkt) = out_rx.recv() => {
                            let _ = sock_tx.send_to(pkt.as_bytes(), server_addr).await;
                        }
                        res = sock_rx.recv_from(&mut buf) => {
                            match res {
                                Ok((len, _from)) => {
                                    if len > 4 {
                                        let s = String::from_utf8_lossy(&buf[..len]).to_string();
                                        let _ = net_tx_udp.send(s);
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }
                }
            });
        }
        TransportProtocol::Tcp => {
            let connect_res =
                tokio::time::timeout(Duration::from_secs(6), TcpStream::connect(server_addr)).await;
            let stream = match connect_res {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    let msg = format!("Falha na conexão TCP com {}: {}", server_addr, e);
                    state.add_log("ERR", &msg, "");
                    state.set_reg_status(RegistrationStatus::Failed(msg));
                    return;
                }
                Err(_) => {
                    let msg = format!("Timeout ao conectar via TCP em {}", server_addr);
                    state.add_log("ERR", &msg, "");
                    state.set_reg_status(RegistrationStatus::Failed(msg));
                    return;
                }
            };
            let _ = stream.set_nodelay(true);
            local_port = stream.local_addr().map(|a| a.port()).unwrap_or(5060);
            let (mut reader, mut writer) = stream.into_split();
            let net_tx_tcp = net_tx.clone();
            let state_tcp = state.clone();
            let link_down_tcp = link_down_tx.clone();

            transport_task = tokio::spawn(async move {
                let mut stream_buf: Vec<u8> = Vec::with_capacity(16384);
                let mut read_chunk = [0u8; 4096];

                loop {
                    tokio::select! {
                        Some(pkt) = out_rx.recv() => {
                            if writer.write_all(pkt.as_bytes()).await.is_err() {
                                let _ = link_down_tcp.send(
                                    "Falha ao escrever na conexão TCP (conexão perdida)".to_string(),
                                );
                                break;
                            }
                        }
                        res = reader.read(&mut read_chunk) => {
                            match res {
                                Ok(0) | Err(_) => {
                                    state_tcp.add_log("ERR", "Conexão TCP encerrada pelo servidor", "");
                                    let _ = link_down_tcp.send(
                                        "Conexão TCP encerrada pelo servidor".to_string(),
                                    );
                                    break;
                                }
                                Ok(n) => {
                                    stream_buf.extend_from_slice(&read_chunk[..n]);
                                    // Extrai todos os quadros SIP completos do stream TCP
                                    while let Some(msg_str) = extract_next_sip_frame(&mut stream_buf) {
                                        let _ = net_tx_tcp.send(msg_str);
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }
    }

    let mut contact_ip = local_ip.clone();
    let mut contact_port = local_port;
    let reg_call_id = format!("{}@{}", random_hex(16), local_ip);
    let reg_from_tag = random_hex(10);
    let mut reg_cseq: u32 = 1;
    let mut last_auth_challenge: Option<DigestChallenge> = None;
    let mut auth_nc: u32 = 1;
    let mut reg_auth_attempts: u32 = 0;

    let mut active_call: Option<ActiveCallDialog> = None;

    // Retransmissão de transações cliente (RFC 3261 §17.1) — só UDP.
    let is_udp = account.transport == TransportProtocol::Udp;
    let mut tx_table = TxTable::default();

    // Envia primeiro REGISTER
    let (reg_branch, reg_raw) = send_register(
        &account,
        &domain,
        &contact_ip,
        contact_port,
        &reg_call_id,
        &reg_from_tag,
        reg_cseq,
        account.register_expires,
        None,
        &mut auth_nc,
        &out_tx,
        state,
    );
    if is_udp {
        tx_table.register(make_txn("REGISTER", reg_cseq, &reg_branch, &reg_raw));
    }

    let re_reg_secs = (account.register_expires.max(60) as u64 * 2) / 3;
    let mut re_reg_timer = tokio::time::interval(Duration::from_secs(re_reg_secs));
    re_reg_timer.tick().await; // pula o tick imediato

    let mut keepalive_timer = tokio::time::interval(Duration::from_secs(25));
    keepalive_timer.tick().await;

    // Varre a tabela de retransmissão a cada 100ms (resolução fina o bastante
    // para T1=500ms). Sem efeito quando a tabela está vazia / em TCP.
    let mut retransmit_timer = tokio::time::interval(Duration::from_millis(100));
    retransmit_timer.tick().await;

    loop {
        tokio::select! {
            _ = retransmit_timer.tick(), if is_udp => {
                tx_table.tick(Instant::now(), &out_tx, state);
            }
            Some(reason) = link_down_rx.recv() => {
                // Transporte caiu: limpa o estado "Registrado" fantasma e
                // encerra qualquer chamada ativa. A reconexão automática com
                // backoff NÃO está implementada (ver pendência no relatório) —
                // o usuário precisa reconectar manualmente por ora.
                state.add_log("ERR", format!("Conexão perdida: {}", reason), "");
                if let Some(ref dialog) = active_call {
                    if let Some(ref h) = dialog.rtp_handle {
                        h.stop();
                    }
                }
                audio.stop_audio();
                state.set_call_phase(CallPhase::Idle);
                let _ = active_call.take();
                state.set_reg_status(RegistrationStatus::Failed(format!(
                    "Conexão perdida: {}. Reconecte para registrar novamente.",
                    reason
                )));
                transport_task.abort();
                break;
            }
            _ = re_reg_timer.tick() => {
                reg_cseq += 1;
                reg_auth_attempts = 0;
                send_register(
                    &account,
                    &domain,
                    &contact_ip,
                    contact_port,
                    &reg_call_id,
                    &reg_from_tag,
                    reg_cseq,
                    account.register_expires,
                    last_auth_challenge.as_ref(),
                    &mut auth_nc,
                    &out_tx,
                    state,
                );
            }
            _ = keepalive_timer.tick() => {
                // Keep-alive CRLF RFC 5626 para manter NAT aberto no roteador e FreeSWITCH
                let _ = out_tx.send("\r\n\r\n".to_string());
            }
            Some(raw_msg) = net_rx.recv() => {
                let Some(msg) = SipMessage::parse(&raw_msg) else {
                    continue;
                };
                state.add_log("RX", &msg.start_line, &raw_msg);

                // Verifica se o FreeSWITCH informou nosso IP/porta públicos no Via (received/rport)
                if let Some(via) = msg.header("Via") {
                    let (rec_ip, rec_port) = extract_via_received_rport(via);
                    if let Some(ip) = rec_ip {
                        if let Some(port) = rec_port {
                            contact_ip = ip.clone();
                            contact_port = port;
                            if let Ok(mut det) = state.detected_public_addr.lock() {
                                *det = Some(format!("{}:{}", ip, port));
                            }
                        }
                    }
                }

                if msg.is_response() {
                    let status = msg.status_code().unwrap_or(0);
                    let cseq_method = msg.cseq_method().unwrap_or("");

                    // RFC 3261 §17.1: qualquer resposta (provisória ou final) para
                    // uma transação cliente pendente interrompe sua retransmissão.
                    // Casamos por CSeq (num+método) E branch do Via — uma resposta
                    // cujo CSeq/branch não casa nenhuma transação ativa é ignorada
                    // para fins de retransmissão (item 6b).
                    if is_udp {
                        if let Some(rcseq) = cseq_num(&msg) {
                            let rbranch = top_via_branch(&msg);
                            if tx_table.matches(rcseq, cseq_method, &rbranch) {
                                tx_table.ack_response(rcseq, cseq_method);
                            }
                        }
                    }

                    if cseq_method.eq_ignore_ascii_case("REGISTER") {
                        match status {
                            200 => {
                                reg_auth_attempts = 0;
                                state.set_reg_status(RegistrationStatus::Registered);
                            }
                            401 | 407 => {
                                let hdr = msg
                                    .header("WWW-Authenticate")
                                    .or_else(|| msg.header("Proxy-Authenticate"));
                                let is_proxy = status == 407 || msg.header("Proxy-Authenticate").is_some();
                                if let Some(hval) = hdr {
                                    if let Some(chal) = parse_digest_challenge(hval, is_proxy) {
                                        let nonce_changed = last_auth_challenge
                                            .as_ref()
                                            .map(|c| &c.nonce)
                                            != Some(&chal.nonce);
                                        if nonce_changed {
                                            auth_nc = 1;
                                        }
                                        // stale=true (ou nonce novo) significa que o servidor
                                        // pede reautenticação com nonce fresco — NÃO é senha
                                        // recusada. Permite uma tentativa extra sem estourar
                                        // o teto de forma a não entrar em loop.
                                        let stale_retry = chal.stale && nonce_changed;
                                        last_auth_challenge = Some(chal.clone());

                                        // Teto de 2 tentativas autenticadas por ciclo; stale
                                        // com nonce novo concede +1 (até no máx. 2).
                                        let may_retry = reg_auth_attempts < 1
                                            || (stale_retry && reg_auth_attempts < 2);

                                        if may_retry {
                                            reg_auth_attempts += 1;
                                            reg_cseq += 1;
                                            if stale_retry {
                                                state.add_log(
                                                    "INFO",
                                                    "REGISTER: nonce expirado (stale), reautenticando",
                                                    "",
                                                );
                                            }
                                            let (rb, rr) = send_register(
                                                &account,
                                                &domain,
                                                &contact_ip,
                                                contact_port,
                                                &reg_call_id,
                                                &reg_from_tag,
                                                reg_cseq,
                                                account.register_expires,
                                                Some(&chal),
                                                &mut auth_nc,
                                                &out_tx,
                                                state,
                                            );
                                            // nota: a resposta anterior já parou a
                                            // transação antiga; registra a nova.
                                            if is_udp {
                                                tx_table.register(make_txn(
                                                    "REGISTER", reg_cseq, &rb, &rr,
                                                ));
                                            }
                                        } else {
                                            state.set_reg_status(RegistrationStatus::Failed(
                                                "Usuário ou senha recusados (401/407)".to_string(),
                                            ));
                                        }
                                    }
                                }
                            }
                            403 => {
                                state.set_reg_status(RegistrationStatus::Failed(
                                    "403 Forbidden (Verifique usuário/senha/domínio)".to_string(),
                                ));
                            }
                            code if code >= 400 => {
                                state.set_reg_status(RegistrationStatus::Failed(format!(
                                    "Erro SIP {}",
                                    msg.start_line
                                )));
                            }
                            _ => {}
                        }
                    } else if cseq_method.eq_ignore_ascii_case("INVITE") {
                        if let Some(ref mut dialog) = active_call {
                            let msg_call_id = msg.header("Call-ID").unwrap_or("");
                            // Item 6b: além do Call-ID, valida CSeq (num) e o branch
                            // do Via — uma resposta de uma transação INVITE antiga
                            // (ex.: antes do re-INVITE autenticado) tem branch/CSeq
                            // diferentes e deve ser IGNORADA, não aplicada ao diálogo.
                            let resp_cseq = cseq_num(&msg);
                            let resp_branch = top_via_branch(&msg);
                            let cseq_ok = resp_cseq == Some(dialog.cseq);
                            let branch_ok = dialog.last_invite_branch.is_empty()
                                || resp_branch.is_empty()
                                || resp_branch == dialog.last_invite_branch;
                            if msg_call_id == dialog.call_id && cseq_ok && branch_ok {
                                if let Some(to_hdr) = msg.header("To") {
                                    if let Some(t) = extract_tag(to_hdr) {
                                        dialog.remote_tag = Some(t);
                                        dialog.to_header = to_hdr.to_string();
                                    }
                                }

                                match status {
                                    100 => {
                                        state.set_call_phase(CallPhase::Dialing {
                                            remote_number: dialog.remote_number.clone(),
                                            status_text: "100 Tentando...".to_string(),
                                        });
                                    }
                                    180 | 183 => {
                                        let label = if status == 180 {
                                            "180 Chamando..."
                                        } else {
                                            "183 Progresso de Sessão..."
                                        };
                                        state.set_call_phase(CallPhase::Dialing {
                                            remote_number: dialog.remote_number.clone(),
                                            status_text: label.to_string(),
                                        });

                                        // Se o 183/180 trouxe SDP (Early Media do FreeSWITCH/operadora),
                                        // desliga o ringback sintético local e inicia o RTP real!
                                        if !msg.body.is_empty() {
                                            if let Some(sdp_info) = parse_sdp(&msg.body) {
                                                audio.start_early_media();
                                                let rem_sock = SocketAddr::new(
                                                    sdp_info.remote_ip,
                                                    sdp_info.remote_port,
                                                );
                                                let need_spawn = dialog.rtp_handle.is_none()
                                                    || dialog.rtp_remote_addr != Some(rem_sock);
                                                if need_spawn {
                                                    if let Some(ref old_h) = dialog.rtp_handle {
                                                        old_h.stop();
                                                    }
                                                    if let Some(ref alloc) = dialog.rtp_alloc {
                                                        dialog.negotiated_dtmf_pt = sdp_info.dtmf_pt;
                                                        let handle = spawn_rtp_session(
                                                            alloc.socket.clone(),
                                                            rem_sock,
                                                            sdp_info.codec,
                                                            sdp_info.dtmf_pt,
                                                            audio.state.clone(),
                                                            state.rtp_stats.clone(),
                                                        );
                                                        dialog.rtp_handle = Some(handle);
                                                        dialog.rtp_remote_addr = Some(rem_sock);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    401 | 407 => {
                                        // 1. Envia ACK obrigatório para encerrar a transação INVITE desafiada (RFC 3261)
                                        send_non_2xx_ack(
                                            &account,
                                            &contact_ip,
                                            contact_port,
                                            dialog,
                                            msg.header("To").unwrap_or(&dialog.to_header),
                                            &out_tx,
                                            state,
                                        );

                                        let hdr = msg
                                            .header("Proxy-Authenticate")
                                            .or_else(|| msg.header("WWW-Authenticate"));
                                        let is_proxy =
                                            status == 407 || msg.header("Proxy-Authenticate").is_some();

                                        if let Some(hval) = hdr {
                                            if let Some(chal) = parse_digest_challenge(hval, is_proxy) {
                                                let nonce_changed = last_auth_challenge
                                                    .as_ref()
                                                    .map(|c| &c.nonce)
                                                    != Some(&chal.nonce);
                                                if nonce_changed {
                                                    auth_nc = 1;
                                                }
                                                let stale_retry = chal.stale && nonce_changed;
                                                last_auth_challenge = Some(chal.clone());

                                                // Teto de 2 INVITEs autenticados; stale com
                                                // nonce novo concede a tentativa extra.
                                                let may_retry = dialog.auth_attempts < 1
                                                    || (stale_retry && dialog.auth_attempts < 2);

                                                if may_retry {
                                                    dialog.auth_attempts += 1;
                                                    dialog.cseq += 1;
                                                    dialog.remote_tag = None;
                                                    dialog.to_header =
                                                        format!("<{}>", dialog.last_invite_uri);
                                                    if stale_retry {
                                                        state.add_log(
                                                            "INFO",
                                                            "INVITE: nonce expirado (stale), reautenticando",
                                                            "",
                                                        );
                                                    }
                                                    send_invite(
                                                        &account,
                                                        &domain,
                                                        &contact_ip,
                                                        contact_port,
                                                        dialog,
                                                        Some(&chal),
                                                        &mut auth_nc,
                                                        &out_tx,
                                                        state,
                                                    );
                                                    if is_udp {
                                                        let b = dialog.last_invite_branch.clone();
                                                        tx_table.register(make_txn(
                                                            "INVITE", dialog.cseq, &b,
                                                            &dialog.last_invite_raw,
                                                        ));
                                                    }
                                                } else {
                                                    audio.stop_audio();
                                                    state.set_call_phase(CallPhase::Idle);
                                                    active_call = None;
                                                }
                                            }
                                        }
                                    }
                                    200 => {
                                        // Captura Contact e Record-Route do 200 OK
                                        if let Some(contact_hdr) = msg.header("Contact") {
                                            dialog.remote_target_uri = extract_uri_from_header(contact_hdr);
                                        }
                                        let rr = msg.all_headers("Record-Route");
                                        if !rr.is_empty() {
                                            dialog.route_set =
                                                rr.into_iter().rev().map(|s| s.to_string()).collect();
                                        }

                                        // Envia ACK para 200 OK
                                        send_2xx_ack(
                                            &account,
                                            &contact_ip,
                                            contact_port,
                                            dialog,
                                            &out_tx,
                                            state,
                                        );

                                        let mut codec_used = AudioCodec::Pcmu;
                                        if let Some(sdp_info) = parse_sdp(&msg.body) {
                                            codec_used = sdp_info.codec;
                                            dialog.negotiated_dtmf_pt = sdp_info.dtmf_pt;
                                            let rem_sock = SocketAddr::new(
                                                sdp_info.remote_ip,
                                                sdp_info.remote_port,
                                            );
                                            // Se o RTP ainda não foi iniciado no 183 ou o endereço mudou no 200 OK, inicia/atualiza agora
                                            let need_spawn = dialog.rtp_handle.is_none()
                                                || dialog.rtp_remote_addr != Some(rem_sock);
                                            if need_spawn {
                                                if let Some(ref old_h) = dialog.rtp_handle {
                                                    old_h.stop();
                                                }
                                                if let Some(ref alloc) = dialog.rtp_alloc {
                                                    let handle = spawn_rtp_session(
                                                        alloc.socket.clone(),
                                                        rem_sock,
                                                        sdp_info.codec,
                                                        sdp_info.dtmf_pt,
                                                        audio.state.clone(),
                                                        state.rtp_stats.clone(),
                                                    );
                                                    dialog.rtp_handle = Some(handle);
                                                    dialog.rtp_remote_addr = Some(rem_sock);
                                                }
                                            }
                                        }

                                        let now = Instant::now();
                                        dialog.connected_at = Some(now);
                                        audio.start_call_audio(
                                            dialog.input_dev.clone(),
                                            dialog.output_dev.clone(),
                                        );
                                        state.set_call_phase(CallPhase::InCall {
                                            remote_number: dialog.remote_number.clone(),
                                            remote_name: dialog.remote_name.clone(),
                                            codec_name: codec_used.name().to_string(),
                                            connected_at: now,
                                        });
                                    }
                                    code if code >= 300 => {
                                        send_non_2xx_ack(
                                            &account,
                                            &contact_ip,
                                            contact_port,
                                            dialog,
                                            msg.header("To").unwrap_or(&dialog.to_header),
                                            &out_tx,
                                            state,
                                        );
                                        if let Some(ref h) = dialog.rtp_handle {
                                            h.stop();
                                        }
                                        audio.stop_audio();
                                        record_completed_call(state, dialog);
                                        state.set_call_phase(CallPhase::Idle);
                                        active_call = None;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                } else {
                    // Requisição SIP recebida do FreeSWITCH (INVITE, BYE, CANCEL, OPTIONS, NOTIFY, INFO, ACK)
                    let method = msg.method().unwrap_or("");
                    if method.eq_ignore_ascii_case("OPTIONS")
                        || method.eq_ignore_ascii_case("NOTIFY")
                        || method.eq_ignore_ascii_case("INFO")
                    {
                        send_simple_response(&msg, "200 OK", &contact_ip, contact_port, &account, &out_tx, state);
                    } else if method.eq_ignore_ascii_case("INVITE") {
                        let call_id = msg.header("Call-ID").unwrap_or("").to_string();
                        if active_call.is_some() {
                            // Já está em outra chamada -> responde 486 Busy Here
                            send_simple_response(
                                &msg,
                                "486 Busy Here",
                                &contact_ip,
                                contact_port,
                                &account,
                                &out_tx,
                                state,
                            );
                        } else {
                            let from_hdr = msg.header("From").unwrap_or("").to_string();
                            let to_hdr = msg.header("To").unwrap_or("").to_string();
                            let (caller_name, caller_num) = extract_caller_display_and_user(&from_hdr);
                            let remote_tag = extract_tag(&from_hdr);
                            let local_tag = random_hex(10);
                            let remote_target = msg
                                .header("Contact")
                                .map(extract_uri_from_header)
                                .unwrap_or_else(|| format!("sip:{}@{}", caller_num, domain));

                            let route_set: Vec<String> = msg
                                .all_headers("Record-Route")
                                .into_iter()
                                .map(|s| s.to_string())
                                .collect();

                            let vias: Vec<String> = msg
                                .all_headers("Via")
                                .into_iter()
                                .map(|s| s.to_string())
                                .collect();

                            let cseq_hdr = msg.header("CSeq").unwrap_or("1 INVITE").to_string();
                            let incoming_sdp = parse_sdp(&msg.body);

                            let tagged_to = if to_hdr.contains(";tag=") {
                                to_hdr.clone()
                            } else {
                                format!("{};tag={}", to_hdr, local_tag)
                            };

                            // Responde 100 Trying e 180 Ringing imediatamente
                            send_dialog_response(
                                "100 Trying",
                                &vias,
                                &from_hdr,
                                &to_hdr,
                                &call_id,
                                &cseq_hdr,
                                &contact_ip,
                                contact_port,
                                &account,
                                None,
                                &out_tx,
                                state,
                            );
                            send_dialog_response(
                                "180 Ringing",
                                &vias,
                                &from_hdr,
                                &tagged_to,
                                &call_id,
                                &cseq_hdr,
                                &contact_ip,
                                contact_port,
                                &account,
                                None,
                                &out_tx,
                                state,
                            );

                            audio.start_ringing_incoming(current_out_dev.clone());
                            state.set_call_phase(CallPhase::Incoming {
                                remote_number: caller_num.clone(),
                                remote_name: caller_name.clone(),
                            });

                            active_call = Some(ActiveCallDialog {
                                call_id,
                                local_tag,
                                remote_tag,
                                from_header: tagged_to,
                                to_header: from_hdr,
                                remote_target_uri: remote_target,
                                remote_number: caller_num,
                                remote_name: caller_name,
                                route_set,
                                cseq: 1,
                                last_invite_branch: String::new(),
                                last_invite_uri: String::new(),
                                last_invite_auth_line: String::new(),
                                last_invite_raw: String::new(),
                                is_incoming: true,
                                incoming_invite_via: vias,
                                incoming_invite_cseq: cseq_hdr,
                                incoming_sdp,
                                rtp_alloc: None,
                                rtp_handle: None,
                                rtp_remote_addr: None,
                                negotiated_dtmf_pt: 101,
                                connected_at: None,
                                auth_attempts: 0,
                                input_dev: current_in_dev.clone(),
                                output_dev: current_out_dev.clone(),
                            });
                        }
                    } else if method.eq_ignore_ascii_case("CANCEL") {
                        send_simple_response(&msg, "200 OK", &contact_ip, contact_port, &account, &out_tx, state);
                        if let Some(ref dialog) = active_call {
                            send_dialog_response(
                                "487 Request Terminated",
                                &dialog.incoming_invite_via,
                                &dialog.to_header,
                                &dialog.from_header,
                                &dialog.call_id,
                                &dialog.incoming_invite_cseq,
                                &contact_ip,
                                contact_port,
                                &account,
                                None,
                                &out_tx,
                                state,
                            );
                            if let Some(ref h) = dialog.rtp_handle {
                                h.stop();
                            }
                            record_completed_call(state, dialog);
                        }
                        audio.stop_audio();
                        state.set_call_phase(CallPhase::Idle);
                        active_call = None;
                    } else if method.eq_ignore_ascii_case("BYE") {
                        send_simple_response(&msg, "200 OK", &contact_ip, contact_port, &account, &out_tx, state);
                        if let Some(ref dialog) = active_call {
                            if let Some(ref h) = dialog.rtp_handle {
                                h.stop();
                            }
                            record_completed_call(state, dialog);
                        }
                        audio.stop_audio();
                        state.set_call_phase(CallPhase::Idle);
                        active_call = None;
                    }
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    SipCommand::Disconnect => {
                        if let Some(ref dialog) = active_call {
                            if let Some(ref h) = dialog.rtp_handle {
                                h.stop();
                            }
                        }
                        audio.stop_audio();
                        state.set_call_phase(CallPhase::Idle);
                        // Envia REGISTER com Expires: 0 para desregistrar limpo no FreeSWITCH
                        reg_cseq += 1;
                        send_register(
                            &account,
                            &domain,
                            &contact_ip,
                            contact_port,
                            &reg_call_id,
                            &reg_from_tag,
                            reg_cseq,
                            0,
                            last_auth_challenge.as_ref(),
                            &mut auth_nc,
                            &out_tx,
                            state,
                        );
                        tokio::time::sleep(Duration::from_millis(120)).await;
                        state.set_reg_status(RegistrationStatus::Disconnected);
                        transport_task.abort();
                        break;
                    }
                    SipCommand::Connect {
                        account: new_acc,
                        input_dev,
                        output_dev,
                    } => {
                        if let Some(ref dialog) = active_call {
                            if let Some(ref h) = dialog.rtp_handle {
                                h.stop();
                            }
                        }
                        audio.stop_audio();
                        transport_task.abort();
                        // Reinicia com as novas configurações
                        Box::pin(run_active_session(
                            state,
                            audio,
                            new_acc,
                            input_dev,
                            output_dev,
                            cmd_rx,
                        ))
                        .await;
                        break;
                    }
                    SipCommand::Dial {
                        target,
                        input_dev,
                        output_dev,
                    } => {
                        if active_call.is_some() {
                            continue;
                        }
                        current_in_dev = input_dev.clone();
                        current_out_dev = output_dev.clone();

                        let clean_target = target.trim().to_string();
                        if clean_target.is_empty() {
                            continue;
                        }

                        let alloc = match allocate_rtp_socket(
                            &local_ip,
                            account.use_stun,
                            &account.stun_server,
                        )
                        .await
                        {
                            Ok(a) => a,
                            Err(e) => {
                                state.add_log("ERR", format!("Erro ao alocar RTP: {}", e), "");
                                continue;
                            }
                        };

                        let call_id = format!("{}@{}", random_hex(16), local_ip);
                        let local_tag = random_hex(10);
                        let disp = if account.display_name.trim().is_empty() {
                            account.username.trim()
                        } else {
                            account.display_name.trim()
                        };
                        let from_header = format!(
                            "\"{}\" <sip:{}@{}>;tag={}",
                            disp,
                            account.username.trim(),
                            domain,
                            local_tag
                        );
                        let remote_uri = if clean_target.starts_with("sip:") {
                            clean_target.clone()
                        } else if clean_target.contains('@') {
                            format!("sip:{}", clean_target)
                        } else {
                            format!("sip:{}@{}", clean_target, domain)
                        };
                        let to_header = format!("<{}>", remote_uri);

                        let mut dialog = ActiveCallDialog {
                            call_id,
                            local_tag,
                            remote_tag: None,
                            from_header,
                            to_header,
                            remote_target_uri: remote_uri.clone(),
                            remote_number: clean_target.clone(),
                            remote_name: clean_target.clone(),
                            route_set: Vec::new(),
                            cseq: 1,
                            last_invite_branch: String::new(),
                            last_invite_uri: remote_uri,
                            last_invite_auth_line: String::new(),
                            last_invite_raw: String::new(),
                            is_incoming: false,
                            incoming_invite_via: Vec::new(),
                            incoming_invite_cseq: String::new(),
                            incoming_sdp: None,
                            rtp_alloc: Some(alloc),
                            rtp_handle: None,
                            rtp_remote_addr: None,
                            negotiated_dtmf_pt: 101,
                            connected_at: None,
                            auth_attempts: 0,
                            input_dev: input_dev.clone(),
                            output_dev: output_dev.clone(),
                        };

                        audio.start_ringback_outgoing(input_dev, output_dev);
                        state.set_call_phase(CallPhase::Dialing {
                            remote_number: clean_target,
                            status_text: "Discando...".to_string(),
                        });

                        send_invite(
                            &account,
                            &domain,
                            &contact_ip,
                            contact_port,
                            &mut dialog,
                            None,
                            &mut auth_nc,
                            &out_tx,
                            state,
                        );
                        if is_udp {
                            let b = dialog.last_invite_branch.clone();
                            tx_table.register(make_txn(
                                "INVITE", dialog.cseq, &b, &dialog.last_invite_raw,
                            ));
                        }

                        active_call = Some(dialog);
                    }
                    SipCommand::Answer {
                        input_dev,
                        output_dev,
                    } => {
                        if let Some(ref mut dialog) = active_call {
                            if dialog.is_incoming && dialog.connected_at.is_none() {
                                dialog.input_dev = input_dev.clone();
                                dialog.output_dev = output_dev.clone();

                                let alloc = match allocate_rtp_socket(
                                    &local_ip,
                                    account.use_stun,
                                    &account.stun_server,
                                )
                                .await
                                {
                                    Ok(a) => a,
                                    Err(e) => {
                                        state.add_log("ERR", format!("Erro RTP ao atender: {}", e), "");
                                        continue;
                                    }
                                };

                                let mut codec_used = AudioCodec::Pcmu;
                                if let Some(ref sdp_info) = dialog.incoming_sdp {
                                    codec_used = sdp_info.codec;
                                    dialog.negotiated_dtmf_pt = sdp_info.dtmf_pt;
                                    let rem_sock = SocketAddr::new(
                                        sdp_info.remote_ip,
                                        sdp_info.remote_port,
                                    );
                                    let handle = spawn_rtp_session(
                                        alloc.socket.clone(),
                                        rem_sock,
                                        sdp_info.codec,
                                        sdp_info.dtmf_pt,
                                        audio.state.clone(),
                                        state.rtp_stats.clone(),
                                    );
                                    dialog.rtp_handle = Some(handle);
                                }

                                let sdp_answer = build_sdp_offer(
                                    &alloc.advertised_ip,
                                    alloc.advertised_port,
                                    Some(codec_used),
                                );
                                dialog.rtp_alloc = Some(alloc);

                                send_dialog_response(
                                    "200 OK",
                                    &dialog.incoming_invite_via,
                                    &dialog.to_header,
                                    &dialog.from_header,
                                    &dialog.call_id,
                                    &dialog.incoming_invite_cseq,
                                    &contact_ip,
                                    contact_port,
                                    &account,
                                    Some(&sdp_answer),
                                    &out_tx,
                                    state,
                                );

                                let now = Instant::now();
                                dialog.connected_at = Some(now);
                                audio.start_call_audio(input_dev, output_dev);
                                state.set_call_phase(CallPhase::InCall {
                                    remote_number: dialog.remote_number.clone(),
                                    remote_name: dialog.remote_name.clone(),
                                    codec_name: codec_used.name().to_string(),
                                    connected_at: now,
                                });
                            }
                        }
                    }
                    SipCommand::Hangup => {
                        if let Some(ref mut dialog) = active_call {
                            if let Some(ref h) = dialog.rtp_handle {
                                h.stop();
                            }
                            if dialog.connected_at.is_some() {
                                // Chamada já estabelecida -> envia BYE
                                let (bye_branch, bye_cseq, bye_raw) = send_bye(
                                    &account,
                                    &contact_ip,
                                    contact_port,
                                    dialog,
                                    last_auth_challenge.as_ref(),
                                    &mut auth_nc,
                                    &out_tx,
                                    state,
                                );
                                if is_udp {
                                    tx_table.register(make_txn(
                                        "BYE", bye_cseq, &bye_branch, &bye_raw,
                                    ));
                                }
                            } else if dialog.is_incoming {
                                // Chamada recebida tocando -> rejeita com 486 Busy Here
                                send_dialog_response(
                                    "486 Busy Here",
                                    &dialog.incoming_invite_via,
                                    &dialog.to_header,
                                    &dialog.from_header,
                                    &dialog.call_id,
                                    &dialog.incoming_invite_cseq,
                                    &contact_ip,
                                    contact_port,
                                    &account,
                                    None,
                                    &out_tx,
                                    state,
                                );
                            } else {
                                // Chamada sainte ainda chamando -> envia CANCEL
                                send_cancel(
                                    &account,
                                    &contact_ip,
                                    contact_port,
                                    dialog,
                                    &out_tx,
                                    state,
                                );
                            }
                            record_completed_call(state, dialog);
                        }
                        audio.stop_audio();
                        state.set_call_phase(CallPhase::Idle);
                        active_call = None;
                    }
                    SipCommand::SendDtmf(digit) => {
                        if let Some(ref mut dialog) = active_call {
                            if let Some(ref h) = dialog.rtp_handle {
                                h.send_dtmf(digit);
                            }
                            // Também envia SIP INFO (dtmf-relay) caso o dialplan FreeSWITCH aguarde INFO
                            if dialog.connected_at.is_some() {
                                send_sip_info_dtmf(
                                    &account,
                                    &contact_ip,
                                    contact_port,
                                    dialog,
                                    digit,
                                    &out_tx,
                                    state,
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

fn record_completed_call(state: &Arc<SipSharedState>, dialog: &ActiveCallDialog) {
    let duration_secs = dialog
        .connected_at
        .map(|t| t.elapsed().as_secs())
        .unwrap_or(0);
    let direction = if dialog.is_incoming {
        if dialog.connected_at.is_some() {
            crate::config::CallDirection::Incoming
        } else {
            crate::config::CallDirection::Missed
        }
    } else {
        crate::config::CallDirection::Outgoing
    };

    if let Ok(mut q) = state.completed_calls.lock() {
        q.push_back(CompletedCallRecord {
            number: dialog.remote_number.clone(),
            name: dialog.remote_name.clone(),
            direction,
            duration_secs,
        });
    }
}

/// Extrai uma mensagem SIP completa de um buffer de stream TCP usando `\r\n\r\n` + `Content-Length`
fn extract_next_sip_frame(buf: &mut Vec<u8>) -> Option<String> {
    // Remove eventuais bytes de keep-alive (\r ou \n) no início
    while let Some(&b) = buf.first() {
        if b == b'\r' || b == b'\n' || b == b' ' {
            buf.remove(0);
        } else {
            break;
        }
    }

    if buf.is_empty() {
        return None;
    }

    let hdr_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let body_start = hdr_end + 4;
    let headers_str = String::from_utf8_lossy(&buf[..hdr_end]);

    let mut content_len: usize = 0;
    for line in headers_str.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            if key.eq_ignore_ascii_case("Content-Length") || key.eq_ignore_ascii_case("l") {
                content_len = v.trim().parse::<usize>().unwrap_or(0);
                break;
            }
        }
    }

    let total_frame_len = body_start + content_len;
    if buf.len() < total_frame_len {
        return None;
    }

    let frame_bytes: Vec<u8> = buf.drain(..total_frame_len).collect();
    Some(String::from_utf8_lossy(&frame_bytes).to_string())
}

#[allow(clippy::too_many_arguments)]
fn send_register(
    account: &SipAccountConfig,
    domain: &str,
    contact_ip: &str,
    contact_port: u16,
    call_id: &str,
    from_tag: &str,
    cseq: u32,
    expires: u32,
    challenge: Option<&DigestChallenge>,
    auth_nc: &mut u32,
    out_tx: &mpsc::UnboundedSender<String>,
    state: &Arc<SipSharedState>,
) -> (String, String) {
    let branch = format!("z9hG4bK-{}", random_hex(12));
    let tp = account.transport.as_str();
    let tp_param = if account.transport == TransportProtocol::Tcp {
        ";transport=tcp"
    } else {
        ""
    };
    let uri = format!("sip:{}", domain);
    let user = account.username.trim();
    let disp = if account.display_name.trim().is_empty() {
        user
    } else {
        account.display_name.trim()
    };

    let mut auth_line = String::new();
    if let Some(chal) = challenge {
        let (hname, hval) = build_authorization_header(
            chal,
            &account.effective_auth_id(),
            &account.password,
            "REGISTER",
            &uri,
            *auth_nc,
        );
        *auth_nc += 1;
        auth_line = format!("{}: {}\r\n", hname, hval);
    }

    let msg = format!(
        "REGISTER {uri} SIP/2.0\r\n\
         Via: SIP/2.0/{tp} {contact_ip}:{contact_port};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: \"{disp}\" <sip:{user}@{domain}>;tag={from_tag}\r\n\
         To: \"{disp}\" <sip:{user}@{domain}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} REGISTER\r\n\
         Contact: <sip:{user}@{contact_ip}:{contact_port}{tp_param};ob>\r\n\
         Expires: {expires}\r\n\
         Allow: INVITE, ACK, CANCEL, BYE, NOTIFY, OPTIONS, INFO\r\n\
         User-Agent: RustSIP/0.1.0 (Arch Linux)\r\n\
         {auth_line}\
         Content-Length: 0\r\n\r\n"
    );

    state.add_log("TX", format!("REGISTER {} (CSeq {})", uri, cseq), &msg);
    let _ = out_tx.send(msg.clone());
    (branch, msg)
}

#[allow(clippy::too_many_arguments)]
fn send_invite(
    account: &SipAccountConfig,
    domain: &str,
    contact_ip: &str,
    contact_port: u16,
    dialog: &mut ActiveCallDialog,
    challenge: Option<&DigestChallenge>,
    auth_nc: &mut u32,
    out_tx: &mpsc::UnboundedSender<String>,
    state: &Arc<SipSharedState>,
) -> String {
    let branch = format!("z9hG4bK-{}", random_hex(12));
    dialog.last_invite_branch = branch.clone();
    let tp = account.transport.as_str();
    let tp_param = if account.transport == TransportProtocol::Tcp {
        ";transport=tcp"
    } else {
        ""
    };
    let user = account.username.trim();
    let uri = dialog.last_invite_uri.clone();

    let (adv_ip, adv_port) = dialog
        .rtp_alloc
        .as_ref()
        .map(|a| (a.advertised_ip.as_str(), a.advertised_port))
        .unwrap_or((contact_ip, 16384));

    let sdp = build_sdp_offer(adv_ip, adv_port, None);

    let mut auth_line = String::new();
    if let Some(chal) = challenge {
        let (hname, hval) = build_authorization_header(
            chal,
            &account.effective_auth_id(),
            &account.password,
            "INVITE",
            &uri,
            *auth_nc,
        );
        *auth_nc += 1;
        auth_line = format!("{}: {}\r\n", hname, hval);
    }
    dialog.last_invite_auth_line = auth_line.clone();

    let _ = domain;
    let msg = format!(
        "INVITE {uri} SIP/2.0\r\n\
         Via: SIP/2.0/{tp} {contact_ip}:{contact_port};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: {from}\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} INVITE\r\n\
         Contact: <sip:{user}@{contact_ip}:{contact_port}{tp_param};ob>\r\n\
         Allow: INVITE, ACK, CANCEL, BYE, NOTIFY, OPTIONS, INFO\r\n\
         User-Agent: RustSIP/0.1.0 (Arch Linux)\r\n\
         {auth_line}\
         Content-Type: application/sdp\r\n\
         Content-Length: {len}\r\n\r\n\
         {sdp}",
        from = dialog.from_header,
        to = dialog.to_header,
        call_id = dialog.call_id,
        cseq = dialog.cseq,
        len = sdp.len()
    );

    state.add_log("TX", format!("INVITE {} (CSeq {})", uri, dialog.cseq), &msg);
    let _ = out_tx.send(msg.clone());
    dialog.last_invite_raw = msg.clone();
    msg
}

fn send_non_2xx_ack(
    account: &SipAccountConfig,
    contact_ip: &str,
    contact_port: u16,
    dialog: &ActiveCallDialog,
    response_to_header: &str,
    out_tx: &mpsc::UnboundedSender<String>,
    state: &Arc<SipSharedState>,
) {
    let tp = account.transport.as_str();
    let uri = &dialog.last_invite_uri;
    let branch = &dialog.last_invite_branch;

    let msg = format!(
        "ACK {uri} SIP/2.0\r\n\
         Via: SIP/2.0/{tp} {contact_ip}:{contact_port};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: {from}\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} ACK\r\n\
         User-Agent: RustSIP/0.1.0 (Arch Linux)\r\n\
         Content-Length: 0\r\n\r\n",
        from = dialog.from_header,
        to = response_to_header,
        call_id = dialog.call_id,
        cseq = dialog.cseq,
    );

    state.add_log("TX", format!("ACK {}", uri), &msg);
    let _ = out_tx.send(msg);
}

fn send_2xx_ack(
    account: &SipAccountConfig,
    contact_ip: &str,
    contact_port: u16,
    dialog: &ActiveCallDialog,
    out_tx: &mpsc::UnboundedSender<String>,
    state: &Arc<SipSharedState>,
) {
    let tp = account.transport.as_str();
    let branch = format!("z9hG4bK-{}", random_hex(12));
    let uri = &dialog.remote_target_uri;

    let mut route_lines = String::new();
    for r in &dialog.route_set {
        route_lines.push_str(&format!("Route: {}\r\n", r));
    }

    let msg = format!(
        "ACK {uri} SIP/2.0\r\n\
         Via: SIP/2.0/{tp} {contact_ip}:{contact_port};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         {route_lines}\
         From: {from}\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} ACK\r\n\
         {auth_line}\
         User-Agent: RustSIP/0.1.0 (Arch Linux)\r\n\
         Content-Length: 0\r\n\r\n",
        from = dialog.from_header,
        to = dialog.to_header,
        call_id = dialog.call_id,
        cseq = dialog.cseq,
        auth_line = dialog.last_invite_auth_line,
    );

    state.add_log("TX", format!("ACK (200 OK) {}", uri), &msg);
    let _ = out_tx.send(msg);
}

fn send_cancel(
    account: &SipAccountConfig,
    contact_ip: &str,
    contact_port: u16,
    dialog: &ActiveCallDialog,
    out_tx: &mpsc::UnboundedSender<String>,
    state: &Arc<SipSharedState>,
) {
    let tp = account.transport.as_str();
    let uri = &dialog.last_invite_uri;
    let branch = &dialog.last_invite_branch;

    let msg = format!(
        "CANCEL {uri} SIP/2.0\r\n\
         Via: SIP/2.0/{tp} {contact_ip}:{contact_port};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: {from}\r\n\
         To: <{uri}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} CANCEL\r\n\
         User-Agent: RustSIP/0.1.0 (Arch Linux)\r\n\
         Content-Length: 0\r\n\r\n",
        from = dialog.from_header,
        call_id = dialog.call_id,
        cseq = dialog.cseq,
    );

    state.add_log("TX", format!("CANCEL {}", uri), &msg);
    let _ = out_tx.send(msg);
}

#[allow(clippy::too_many_arguments)]
fn send_bye(
    account: &SipAccountConfig,
    contact_ip: &str,
    contact_port: u16,
    dialog: &mut ActiveCallDialog,
    challenge: Option<&DigestChallenge>,
    auth_nc: &mut u32,
    out_tx: &mpsc::UnboundedSender<String>,
    state: &Arc<SipSharedState>,
) -> (String, u32, String) {
    dialog.cseq += 1;
    let branch = format!("z9hG4bK-{}", random_hex(12));
    let tp = account.transport.as_str();
    let uri = &dialog.remote_target_uri;

    let mut route_lines = String::new();
    for r in &dialog.route_set {
        route_lines.push_str(&format!("Route: {}\r\n", r));
    }

    let mut auth_line = String::new();
    if let Some(chal) = challenge {
        let (hname, hval) = build_authorization_header(
            chal,
            &account.effective_auth_id(),
            &account.password,
            "BYE",
            uri,
            *auth_nc,
        );
        *auth_nc += 1;
        auth_line = format!("{}: {}\r\n", hname, hval);
    }

    let msg = format!(
        "BYE {uri} SIP/2.0\r\n\
         Via: SIP/2.0/{tp} {contact_ip}:{contact_port};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         {route_lines}\
         From: {from}\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} BYE\r\n\
         {auth_line}\
         User-Agent: RustSIP/0.1.0 (Arch Linux)\r\n\
         Content-Length: 0\r\n\r\n",
        from = dialog.from_header,
        to = dialog.to_header,
        call_id = dialog.call_id,
        cseq = dialog.cseq,
    );

    state.add_log("TX", format!("BYE {}", uri), &msg);
    let _ = out_tx.send(msg.clone());
    (branch, dialog.cseq, msg)
}

fn send_sip_info_dtmf(
    account: &SipAccountConfig,
    contact_ip: &str,
    contact_port: u16,
    dialog: &mut ActiveCallDialog,
    digit: char,
    out_tx: &mpsc::UnboundedSender<String>,
    state: &Arc<SipSharedState>,
) {
    dialog.cseq += 1;
    let branch = format!("z9hG4bK-{}", random_hex(12));
    let tp = account.transport.as_str();
    let uri = &dialog.remote_target_uri;

    let mut route_lines = String::new();
    for r in &dialog.route_set {
        route_lines.push_str(&format!("Route: {}\r\n", r));
    }

    let body = format!("Signal={}\r\nDuration=160\r\n", digit);
    let msg = format!(
        "INFO {uri} SIP/2.0\r\n\
         Via: SIP/2.0/{tp} {contact_ip}:{contact_port};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         {route_lines}\
         From: {from}\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} INFO\r\n\
         Content-Type: application/dtmf-relay\r\n\
         User-Agent: RustSIP/0.1.0 (Arch Linux)\r\n\
         Content-Length: {len}\r\n\r\n\
         {body}",
        from = dialog.from_header,
        to = dialog.to_header,
        call_id = dialog.call_id,
        cseq = dialog.cseq,
        len = body.len()
    );

    state.add_log("TX", format!("INFO DTMF '{}'", digit), &msg);
    let _ = out_tx.send(msg);
}

fn send_simple_response(
    req: &SipMessage,
    status: &str,
    contact_ip: &str,
    contact_port: u16,
    account: &SipAccountConfig,
    out_tx: &mpsc::UnboundedSender<String>,
    state: &Arc<SipSharedState>,
) {
    let vias = req
        .all_headers("Via")
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let from = req.header("From").unwrap_or("");
    let to = req.header("To").unwrap_or("");
    let call_id = req.header("Call-ID").unwrap_or("");
    let cseq = req.header("CSeq").unwrap_or("1 OPTIONS");

    send_dialog_response(
        status,
        &vias,
        from,
        to,
        call_id,
        cseq,
        contact_ip,
        contact_port,
        account,
        None,
        out_tx,
        state,
    );
}

#[allow(clippy::too_many_arguments)]
fn send_dialog_response(
    status: &str,
    vias: &[String],
    from: &str,
    to: &str,
    call_id: &str,
    cseq: &str,
    contact_ip: &str,
    contact_port: u16,
    account: &SipAccountConfig,
    sdp_body: Option<&str>,
    out_tx: &mpsc::UnboundedSender<String>,
    state: &Arc<SipSharedState>,
) {
    let mut via_lines = String::new();
    for v in vias {
        via_lines.push_str(&format!("Via: {}\r\n", v));
    }

    let tp_param = if account.transport == TransportProtocol::Tcp {
        ";transport=tcp"
    } else {
        ""
    };
    let user = account.username.trim();
    let (content_type, body) = match sdp_body {
        Some(b) => ("Content-Type: application/sdp\r\n", b),
        None => ("", ""),
    };

    let msg = format!(
        "SIP/2.0 {status}\r\n\
         {via_lines}\
         From: {from}\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq}\r\n\
         Contact: <sip:{user}@{contact_ip}:{contact_port}{tp_param};ob>\r\n\
         User-Agent: RustSIP/0.1.0 (Arch Linux)\r\n\
         {content_type}\
         Content-Length: {len}\r\n\r\n\
         {body}",
        len = body.len()
    );

    state.add_log("TX", format!("SIP/2.0 {} ({})", status, cseq), &msg);
    let _ = out_tx.send(msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_freeswitch_digest_challenge_and_auth() {
        let raw_hdr =
            r#"Digest realm="sip.freeswitch.cloud", nonce="7c9a8b12-44ef-11ef-9a21", algorithm=MD5, qop="auth""#;
        let chal = parse_digest_challenge(raw_hdr, false).expect("Deve parsear challenge FreeSWITCH");
        assert_eq!(chal.realm, "sip.freeswitch.cloud");
        assert_eq!(chal.nonce, "7c9a8b12-44ef-11ef-9a21");
        assert_eq!(chal.qop.as_deref(), Some("auth"));

        let (hname, hval) = build_authorization_header(
            &chal,
            "1001",
            "senhaForte123",
            "REGISTER",
            "sip:sip.freeswitch.cloud",
            1,
        );
        assert_eq!(hname, "Authorization");
        assert!(hval.contains("username=\"1001\""));
        assert!(hval.contains("qop=auth, nc=00000001"));
    }

    #[test]
    fn test_tcp_sip_frame_extraction() {
        let msg1 = "SIP/2.0 100 Trying\r\nVia: SIP/2.0/TCP 10.0.0.2:5060\r\nContent-Length: 0\r\n\r\n";
        let body2 = "v=0\r\nc=IN IP4 198.51.100.10\r\nm=audio 24500 RTP/AVP 0 101\r\n";
        let msg2 = format!(
            "SIP/2.0 200 OK\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
            body2.len(),
            body2
        );

        let mut stream = Vec::new();
        stream.extend_from_slice(b"\r\n\r\n"); // keepalive antes do pacote
        stream.extend_from_slice(msg1.as_bytes());
        stream.extend_from_slice(msg2.as_bytes());

        let f1 = extract_next_sip_frame(&mut stream).expect("Primeiro frame");
        assert!(f1.starts_with("SIP/2.0 100 Trying"));

        let f2 = extract_next_sip_frame(&mut stream).expect("Segundo frame com SDP");
        assert!(f2.starts_with("SIP/2.0 200 OK"));
        assert!(f2.ends_with(body2));
        assert!(stream.is_empty());
    }

    #[test]
    fn test_parse_sdp() {
        let sdp = "v=0\r\n\
                   o=FreeSWITCH 17000 17001 IN IP4 203.0.113.50\r\n\
                   s=FreeSWITCH\r\n\
                   c=IN IP4 203.0.113.50\r\n\
                   t=0 0\r\n\
                   m=audio 18442 RTP/AVP 8 101\r\n\
                   a=rtpmap:8 PCMA/8000\r\n\
                   a=rtpmap:101 telephone-event/8000\r\n";
        let info = parse_sdp(sdp).expect("SDP válido");
        assert_eq!(info.remote_ip.to_string(), "203.0.113.50");
        assert_eq!(info.remote_port, 18442);
        assert_eq!(info.codec, AudioCodec::Pcma);
        assert_eq!(info.dtmf_pt, 101);
    }

    // ---- Item 4: parse de `stale` no desafio Digest ----
    #[test]
    fn test_digest_stale_parsing() {
        let with_stale = r#"Digest realm="fs", nonce="abc", algorithm=MD5, stale=true"#;
        let c = parse_digest_challenge(with_stale, false).expect("parseia");
        assert!(c.stale, "stale=true deve virar true");

        let with_stale_false = r#"Digest realm="fs", nonce="abc", stale=false"#;
        let c2 = parse_digest_challenge(with_stale_false, false).expect("parseia");
        assert!(!c2.stale);

        let no_stale = r#"Digest realm="fs", nonce="abc""#;
        let c3 = parse_digest_challenge(no_stale, false).expect("parseia");
        assert!(!c3.stale, "ausência de stale => false (default)");
    }

    // ---- Item 6b: extratores de CSeq e branch do Via ----
    #[test]
    fn test_cseq_and_branch_extractors() {
        let raw = "SIP/2.0 200 OK\r\n\
                   Via: SIP/2.0/UDP 10.0.0.2:5060;branch=z9hG4bK-abc123;rport=5060\r\n\
                   CSeq: 42 INVITE\r\n\
                   Content-Length: 0\r\n\r\n";
        let msg = SipMessage::parse(raw).expect("parseia");
        assert_eq!(cseq_num(&msg), Some(42));
        assert_eq!(top_via_branch(&msg), "z9hG4bK-abc123");
    }

    // ---- Item 6a: TxTable casa por CSeq+método+branch, faz ack e retransmite ----
    #[test]
    fn test_txtable_match_and_ack() {
        let mut t = TxTable::default();
        t.register(make_txn("INVITE", 42, "z9hG4bK-abc123", "INVITE sip:x SIP/2.0\r\n\r\n"));

        // Casa com CSeq+método+branch corretos.
        assert!(t.matches(42, "INVITE", "z9hG4bK-abc123"));
        // NÃO casa se o branch for de outra transação.
        assert!(!t.matches(42, "INVITE", "z9hG4bK-OUTRO"));
        // NÃO casa com método/cseq diferentes.
        assert!(!t.matches(42, "BYE", "z9hG4bK-abc123"));
        assert!(!t.matches(99, "INVITE", "z9hG4bK-abc123"));

        // ack remove a transação -> deixa de casar.
        t.ack_response(42, "INVITE");
        assert!(!t.matches(42, "INVITE", "z9hG4bK-abc123"));
    }

    #[test]
    fn test_txtable_register_replaces_same_cseq() {
        // Um REGISTER reenviado com Authorization (mesmo método+cseq) substitui
        // a transação anterior em vez de acumular duas.
        let mut t = TxTable::default();
        t.register(make_txn("REGISTER", 1, "b1", "REGISTER v1"));
        t.register(make_txn("REGISTER", 1, "b2", "REGISTER v2"));
        assert_eq!(t.txns.len(), 1, "mesmo método+cseq não deve duplicar");
        // A entrada vigente é a v2 (branch b2).
        assert!(t.matches(1, "REGISTER", "b2"));
        assert!(!t.matches(1, "REGISTER", "b1"));
    }

    #[test]
    fn test_txtable_retransmits_due_then_stops() {
        use std::thread::sleep;
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let state = SipSharedState::new(RtpStats::new());

        let mut t = TxTable::default();
        t.register(make_txn("INVITE", 7, "br7", "INVITE sip:x SIP/2.0\r\n\r\n"));

        // Antes de T1 (500ms) nada deve ser reenviado.
        t.tick(Instant::now(), &tx, &state);
        assert!(rx.try_recv().is_err(), "não reenvia antes de T1");

        // Após T1, um tick reenvia exatamente 1 vez.
        sleep(T1 + Duration::from_millis(60));
        t.tick(Instant::now(), &tx, &state);
        assert!(rx.try_recv().is_ok(), "deve reenviar após T1");
        assert!(rx.try_recv().is_err(), "apenas 1 reenvio por tick vencido");

        // Chegou a resposta casando CSeq+método+branch -> para de reenviar.
        assert!(t.matches(7, "INVITE", "br7"));
        t.ack_response(7, "INVITE");
        sleep(T1 + Duration::from_millis(60));
        t.tick(Instant::now(), &tx, &state);
        assert!(rx.try_recv().is_err(), "após ack não reenvia mais");
    }
}

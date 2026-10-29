use crate::audio::AudioSharedState;
use crate::codec::{dtmf_to_event_code, AudioCodec};
use rand::Rng;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

pub struct RtpStats {
    pub tx_packets: AtomicU64,
    pub rx_packets: AtomicU64,
    pub active: AtomicBool,
    pub remote_addr: Mutex<Option<SocketAddr>>,
    pub codec: Mutex<AudioCodec>,
}

impl RtpStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tx_packets: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
            active: AtomicBool::new(false),
            remote_addr: Mutex::new(None),
            codec: Mutex::new(AudioCodec::Pcmu),
        })
    }

    pub fn reset(&self) {
        self.tx_packets.store(0, Ordering::Relaxed);
        self.rx_packets.store(0, Ordering::Relaxed);
        self.active.store(false, Ordering::Relaxed);
        if let Ok(mut rem) = self.remote_addr.lock() {
            *rem = None;
        }
    }
}

pub struct RtpSocketAllocation {
    pub socket: Arc<UdpSocket>,
    pub advertised_ip: String,
    pub advertised_port: u16,
}

/// Aloca um socket UDP em uma porta par na faixa RTP padrão (16384..32768)
/// e opcionalmente descobre o IP/porta públicos via STUN (RFC 5389) no mesmo socket.
pub async fn allocate_rtp_socket(
    local_ip: &str,
    use_stun: bool,
    stun_server: &str,
) -> anyhow::Result<RtpSocketAllocation> {
    let mut bound_sock = None;
    let start_port = {
        let mut rng = rand::thread_rng();
        let p: u16 = rng.gen_range(8192..16000) * 2; // Sempre par!
        p
    };

    for offset in 0..200u16 {
        let port = start_port.wrapping_add(offset * 2);
        if let Ok(sock) = UdpSocket::bind(("0.0.0.0", port)).await {
            bound_sock = Some(sock);
            break;
        }
    }

    let sock = match bound_sock {
        Some(s) => s,
        None => UdpSocket::bind("0.0.0.0:0").await?,
    };

    let local_port = sock.local_addr()?.port();
    let mut advertised_ip = local_ip.to_string();
    let mut advertised_port = local_port;

    if use_stun && !stun_server.trim().is_empty() {
        if let Ok(Some(mapped)) = query_stun_rfc5389(&sock, stun_server.trim()).await {
            advertised_ip = mapped.ip().to_string();
            advertised_port = mapped.port();
        }
    }

    Ok(RtpSocketAllocation {
        socket: Arc::new(sock),
        advertised_ip,
        advertised_port,
    })
}

/// Cliente STUN (RFC 5389) 100% em Rust para descobrir IP e porta pública do socket RTP
async fn query_stun_rfc5389(
    sock: &UdpSocket,
    stun_server: &str,
) -> anyhow::Result<Option<SocketAddr>> {
    let target = if stun_server.contains(':') {
        stun_server.to_string()
    } else {
        format!("{}:19302", stun_server)
    };

    let mut addrs = tokio::net::lookup_host(&target).await?;
    let Some(stun_addr) = addrs.find(|a| a.is_ipv4()) else {
        return Ok(None);
    };

    // Retransmissão RFC 5389 §7.2.1: STUN sobre UDP é não-confiável, então
    // reenviamos a Binding Request algumas vezes com backoff curto antes de
    // desistir. Sem retry, um único datagrama perdido (comum em NAT) faz a
    // descoberta cair no IP local -> áudio one-way. Teto total ~2.3s para não
    // travar a discagem (650 + 850 + 800ms de espera).
    const ATTEMPT_TIMEOUTS_MS: [u64; 3] = [650, 850, 800];

    let mut buf = [0u8; 512];
    for (attempt, &timeout_ms) in ATTEMPT_TIMEOUTS_MS.iter().enumerate() {
        // Transaction ID NOVO por tentativa (uma resposta atrasada da tentativa
        // anterior não deve ser aceita como resposta desta).
        let mut req = [0u8; 20];
        req[0] = 0x00;
        req[1] = 0x01; // Binding Request
        req[2] = 0x00;
        req[3] = 0x00; // Message Length = 0
        // Magic Cookie: 0x2112A442
        req[4] = 0x21;
        req[5] = 0x12;
        req[6] = 0xA4;
        req[7] = 0x42;
        // Transaction ID (12 bytes)
        rand::thread_rng().fill(&mut req[8..20]);
        let txid = req[8..20].to_vec();

        if sock.send_to(&req, stun_addr).await.is_err() {
            continue;
        }

        // Pode chegar lixo/resposta antiga antes da resposta correta: fica no
        // laço até casar o transaction ID OU estourar o timeout desta tentativa.
        let attempt_deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let remaining = attempt_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let recv_res = tokio::time::timeout(remaining, sock.recv_from(&mut buf)).await;
            let Ok(Ok((len, _from))) = recv_res else {
                break; // timeout desta tentativa -> tenta a próxima
            };

            if len < 20 || buf[0] != 0x01 || buf[1] != 0x01 {
                continue; // não é Binding Success Response
            }
            // Confere o Magic Cookie e o Transaction ID desta tentativa.
            if buf[4..8] != [0x21, 0x12, 0xA4, 0x42] || buf[8..20] != txid[..] {
                continue; // resposta de outra transação / atrasada
            }

            if let Some(mapped) = parse_stun_mapped_address(&buf, len) {
                return Ok(Some(mapped));
            }
            // Resposta válida mas sem endereço mapeável: não adianta reenviar.
            return Ok(None);
        }
        let _ = attempt; // documenta intenção; backoff é o próprio timeout crescente
    }

    Ok(None)
}

/// Extrai XOR-MAPPED-ADDRESS (ou MAPPED-ADDRESS legado) de uma resposta STUN.
fn parse_stun_mapped_address(buf: &[u8], len: usize) -> Option<SocketAddr> {
    let msg_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = (20 + msg_len).min(len);
    let mut pos = 20;

    while pos + 4 <= end {
        let attr_type = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let attr_len = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
        pos += 4;
        if pos + attr_len > end {
            break;
        }

        // 0x0020 = XOR-MAPPED-ADDRESS, 0x8020 = XOR-MAPPED-ADDRESS (antigo)
        if (attr_type == 0x0020 || attr_type == 0x8020) && attr_len >= 8 {
            let family = buf[pos + 1];
            if family == 0x01 {
                let xport = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]);
                let port = xport ^ 0x2112;
                let ip = Ipv4Addr::new(
                    buf[pos + 4] ^ 0x21,
                    buf[pos + 5] ^ 0x12,
                    buf[pos + 6] ^ 0xA4,
                    buf[pos + 7] ^ 0x42,
                );
                return Some(SocketAddr::new(IpAddr::V4(ip), port));
            }
        } else if attr_type == 0x0001 && attr_len >= 8 {
            // 0x0001 = MAPPED-ADDRESS
            let family = buf[pos + 1];
            if family == 0x01 {
                let port = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]);
                let ip = Ipv4Addr::new(buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]);
                return Some(SocketAddr::new(IpAddr::V4(ip), port));
            }
        }

        // Atributos STUN são alinhados em 4 bytes
        let padded_len = (attr_len + 3) & !3;
        pos += padded_len;
    }

    None
}

pub struct RtpSessionHandle {
    stop_tx: mpsc::Sender<()>,
    dtmf_tx: mpsc::Sender<char>,
}

impl RtpSessionHandle {
    pub fn send_dtmf(&self, digit: char) {
        let _ = self.dtmf_tx.try_send(digit);
    }

    pub fn stop(&self) {
        let _ = self.stop_tx.try_send(());
    }
}

/// Inicia a sessão de mídia RTP bidirecional (TX a cada 20ms + RX contínuo)
pub fn spawn_rtp_session(
    socket: Arc<UdpSocket>,
    remote_addr: SocketAddr,
    codec: AudioCodec,
    dtmf_payload_type: u8,
    audio_state: Arc<AudioSharedState>,
    stats: Arc<RtpStats>,
) -> RtpSessionHandle {
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(2);
    let (dtmf_tx, mut dtmf_rx) = mpsc::channel::<char>(32);

    stats.reset();
    stats.active.store(true, Ordering::Relaxed);
    if let Ok(mut rem) = stats.remote_addr.lock() {
        *rem = Some(remote_addr);
    }
    if let Ok(mut c) = stats.codec.lock() {
        *c = codec;
    }

    let sock_rx = socket.clone();
    let audio_rx = audio_state.clone();
    let stats_rx = stats.clone();

    // Task de Recepção RTP (RX)
    let rx_task = tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let mut decoded_pcm = Vec::with_capacity(320);

        loop {
            match sock_rx.recv_from(&mut buf).await {
                Ok((len, _src)) => {
                    if len < 12 {
                        continue;
                    }
                    let version = buf[0] >> 6;
                    if version != 2 {
                        continue;
                    }
                    let padding = (buf[0] & 0x20) != 0;
                    let extension = (buf[0] & 0x10) != 0;
                    let csrc_count = (buf[0] & 0x0F) as usize;
                    let payload_type = buf[1] & 0x7F;

                    let mut header_len = 12 + csrc_count * 4;
                    if header_len > len {
                        continue;
                    }

                    if extension {
                        if header_len + 4 > len {
                            continue;
                        }
                        let ext_words =
                            u16::from_be_bytes([buf[header_len + 2], buf[header_len + 3]]) as usize;
                        header_len += 4 + ext_words * 4;
                        if header_len > len {
                            continue;
                        }
                    }

                    let mut end_pos = len;
                    if padding && end_pos > header_len {
                        let pad_len = buf[len - 1] as usize;
                        if pad_len <= end_pos - header_len {
                            end_pos -= pad_len;
                        }
                    }

                    let payload = &buf[header_len..end_pos];
                    if payload.is_empty() {
                        continue;
                    }

                    if let Some(rx_codec) = AudioCodec::from_payload_type(payload_type) {
                        stats_rx.rx_packets.fetch_add(1, Ordering::Relaxed);
                        rx_codec.decode_frame(payload, &mut decoded_pcm);
                        audio_rx.push_rx_samples(&decoded_pcm);
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Task de Transmissão RTP (TX a cada 20ms = 160 amostras @ 8000 Hz)
    tokio::spawn(async move {
        let ssrc: u32 = rand::thread_rng().gen();
        let mut seq: u16 = rand::thread_rng().gen();
        let mut timestamp: u32 = rand::thread_rng().gen();
        let mut first_packet = true;

        let mut frame_160 = [0i16; 160];
        let mut encoded_payload = Vec::with_capacity(160);
        let mut rtp_packet = Vec::with_capacity(172);

        let mut interval = tokio::time::interval(Duration::from_millis(20));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = stop_rx.recv() => {
                    break;
                }
                Some(digit) = dtmf_rx.recv() => {
                    // Envia evento DTMF RFC 4733 / RFC 2833 (telephone-event)
                    if let Some(event_code) = dtmf_to_event_code(digit) {
                        let event_ts = timestamp;
                        let durations = [160u16, 320, 480, 640, 800, 960];
                        for (idx, &dur) in durations.iter().enumerate() {
                            let is_first = idx == 0;
                            let is_end = idx == durations.len() - 1;
                            let repeats = if is_end { 3 } else { 1 };
                            for _ in 0..repeats {
                                let marker = if is_first { 0x80 } else { 0x00 };
                                let end_bit = if is_end { 0x80 } else { 0x00 };
                                let mut pkt = [0u8; 16];
                                pkt[0] = 0x80; // RTP v2
                                pkt[1] = marker | (dtmf_payload_type & 0x7F);
                                pkt[2..4].copy_from_slice(&seq.to_be_bytes());
                                pkt[4..8].copy_from_slice(&event_ts.to_be_bytes());
                                pkt[8..12].copy_from_slice(&ssrc.to_be_bytes());
                                // Payload RFC 4733 (4 bytes): Event, E|R|Volume, Duration
                                pkt[12] = event_code;
                                pkt[13] = end_bit | 10; // Volume = 10 (-10 dBm0)
                                pkt[14..16].copy_from_slice(&dur.to_be_bytes());

                                let _ = socket.send_to(&pkt, remote_addr).await;
                                stats.tx_packets.fetch_add(1, Ordering::Relaxed);
                                seq = seq.wrapping_add(1);
                            }
                            if !is_end {
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                        }
                        timestamp = timestamp.wrapping_add(960);
                    }
                }
                _ = interval.tick() => {
                    if audio_state.on_hold.load(Ordering::Relaxed) {
                        timestamp = timestamp.wrapping_add(160);
                        continue;
                    }

                    // Puxa 160 amostras do microfone ou envia silêncio (importante para NAT simétrico no FreeSWITCH!)
                    if !audio_state.pop_tx_frame_160(&mut frame_160) {
                        frame_160.fill(0);
                    }

                    codec.encode_frame(&frame_160, &mut encoded_payload);

                    rtp_packet.clear();
                    rtp_packet.push(0x80); // Version 2
                    let marker = if first_packet { 0x80 } else { 0x00 };
                    first_packet = false;
                    rtp_packet.push(marker | (codec.payload_type() & 0x7F));
                    rtp_packet.extend_from_slice(&seq.to_be_bytes());
                    rtp_packet.extend_from_slice(&timestamp.to_be_bytes());
                    rtp_packet.extend_from_slice(&ssrc.to_be_bytes());
                    rtp_packet.extend_from_slice(&encoded_payload);

                    if socket.send_to(&rtp_packet, remote_addr).await.is_ok() {
                        stats.tx_packets.fetch_add(1, Ordering::Relaxed);
                    }

                    seq = seq.wrapping_add(1);
                    timestamp = timestamp.wrapping_add(160);
                }
            }
        }

        rx_task.abort();
        stats.active.store(false, Ordering::Relaxed);
    });

    RtpSessionHandle { stop_tx, dtmf_tx }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monta uma Binding Success Response STUN com XOR-MAPPED-ADDRESS e verifica
    /// que o parser decodifica IP/porta corretamente (item 3 — retry STUN).
    #[test]
    fn test_parse_stun_xor_mapped_address() {
        // IP público fictício 203.0.113.7, porta 54321.
        let real_ip = [203u8, 0, 113, 7];
        let real_port: u16 = 54321;

        let mut buf = Vec::new();
        // Cabeçalho STUN (20 bytes): tipo 0x0101 (Binding Success), len=12
        buf.extend_from_slice(&[0x01, 0x01, 0x00, 0x0C]);
        buf.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]); // magic cookie
        buf.extend_from_slice(&[0u8; 12]); // transaction id (irrelevante p/ o parser)

        // Atributo XOR-MAPPED-ADDRESS (0x0020), len=8
        buf.extend_from_slice(&[0x00, 0x20, 0x00, 0x08]);
        buf.push(0x00); // reserved
        buf.push(0x01); // family IPv4
        let xport = real_port ^ 0x2112;
        buf.extend_from_slice(&xport.to_be_bytes());
        buf.push(real_ip[0] ^ 0x21);
        buf.push(real_ip[1] ^ 0x12);
        buf.push(real_ip[2] ^ 0xA4);
        buf.push(real_ip[3] ^ 0x42);

        let len = buf.len();
        let mapped = parse_stun_mapped_address(&buf, len).expect("deve extrair endereço");
        assert_eq!(mapped.ip().to_string(), "203.0.113.7");
        assert_eq!(mapped.port(), 54321);
    }

    /// Resposta sem atributo de endereço => None (não entra em loop nem panica).
    #[test]
    fn test_parse_stun_no_address_attr() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]);
        buf.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
        buf.extend_from_slice(&[0u8; 12]);
        let len = buf.len();
        assert!(parse_stun_mapped_address(&buf, len).is_none());
    }
}

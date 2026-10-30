use crate::codec::synthesize_dtmf_8k;
use serde::Deserialize;
use std::collections::VecDeque;
use std::f32::consts::PI;
use std::ffi::{c_char, c_int, c_void, CString};
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const MODE_IDLE: u8 = 0;
pub const MODE_IN_CALL: u8 = 1;
pub const MODE_RINGING_INCOMING: u8 = 2;
pub const MODE_RINGBACK_OUTGOING: u8 = 3;
pub const MODE_LOOPBACK_TEST: u8 = 4;
pub const MODE_EARLY_MEDIA: u8 = 5;

// Bindings FFI diretos para libpulse-simple (nativo PipeWire / PulseAudio no Arch Linux)
#[repr(C)]
struct pa_simple {
    _private: [u8; 0],
}

const PA_STREAM_PLAYBACK: c_int = 1;
const PA_STREAM_RECORD: c_int = 2;
const PA_SAMPLE_S16LE: c_int = 3;

#[repr(C)]
struct pa_sample_spec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct pa_buffer_attr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

#[link(name = "pulse-simple")]
#[link(name = "pulse")]
extern "C" {
    fn pa_simple_new(
        server: *const c_char,
        name: *const c_char,
        dir: c_int,
        dev: *const c_char,
        stream_name: *const c_char,
        ss: *const pa_sample_spec,
        map: *const c_void,
        attr: *const pa_buffer_attr,
        error: *mut c_int,
    ) -> *mut pa_simple;

    fn pa_simple_read(
        s: *mut pa_simple,
        data: *mut c_void,
        bytes: usize,
        error: *mut c_int,
    ) -> c_int;

    fn pa_simple_write(
        s: *mut pa_simple,
        data: *const c_void,
        bytes: usize,
        error: *mut c_int,
    ) -> c_int;

    fn pa_simple_free(s: *mut pa_simple);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDeviceInfo {
    /// Nome técnico do node no PipeWire/PulseAudio (ex: "bluez_output.5C_D3_3D_17_B9_FB.1")
    pub id: String,
    /// Nome amigável do dispositivo (ex: "Buds4 Pro de João")
    pub description: String,
    pub is_default: bool,
    pub available: bool,
}

impl AudioDeviceInfo {
    pub fn display_label(&self) -> String {
        if self.is_default {
            format!("★ {} (Padrão)", self.description)
        } else if !self.available {
            format!("{} (Desconectado)", self.description)
        } else {
            self.description.clone()
        }
    }
}

#[derive(Deserialize)]
struct PactlNodeJson {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    monitor_source: String,
    #[serde(default)]
    properties: serde_json::Value,
    #[serde(default)]
    ports: Vec<PactlPortJson>,
}

#[derive(Deserialize)]
struct PactlPortJson {
    #[serde(default)]
    availability: String,
}

pub struct AudioSharedState {
    /// Amostras 8000 Hz mono capturadas do microfone -> prontas para envio RTP
    pub tx_pcm_8k: Mutex<VecDeque<i16>>,
    /// Amostras 8000 Hz mono recebidas via RTP -> prontas para tocar no alto-falante
    pub rx_pcm_8k: Mutex<VecDeque<i16>>,
    /// Amostras 8000 Hz mono de tons locais (DTMF, teste)
    pub tone_pcm_8k: Mutex<VecDeque<i16>>,
    pub mode: AtomicU8,
    pub muted: AtomicBool,
    pub on_hold: AtomicBool,
    mic_gain_bits: AtomicU32,
    speaker_vol_bits: AtomicU32,
    mic_level_bits: AtomicU32,
    speaker_level_bits: AtomicU32,
    /// Incrementado a cada nova sessão para encerrar threads antigas imediatamente
    session_gen: AtomicU64,
}

impl AudioSharedState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tx_pcm_8k: Mutex::new(VecDeque::with_capacity(8000)),
            rx_pcm_8k: Mutex::new(VecDeque::with_capacity(8000)),
            tone_pcm_8k: Mutex::new(VecDeque::with_capacity(4000)),
            mode: AtomicU8::new(MODE_IDLE),
            muted: AtomicBool::new(false),
            on_hold: AtomicBool::new(false),
            mic_gain_bits: AtomicU32::new(1.0f32.to_bits()),
            speaker_vol_bits: AtomicU32::new(1.0f32.to_bits()),
            mic_level_bits: AtomicU32::new(0.0f32.to_bits()),
            speaker_level_bits: AtomicU32::new(0.0f32.to_bits()),
            session_gen: AtomicU64::new(0),
        })
    }

    pub fn set_mic_gain(&self, gain: f32) {
        self.mic_gain_bits
            .store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
    }

    pub fn mic_gain(&self) -> f32 {
        f32::from_bits(self.mic_gain_bits.load(Ordering::Relaxed))
    }

    pub fn set_speaker_volume(&self, vol: f32) {
        self.speaker_vol_bits
            .store(vol.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
    }

    pub fn speaker_volume(&self) -> f32 {
        f32::from_bits(self.speaker_vol_bits.load(Ordering::Relaxed))
    }

    pub fn mic_level(&self) -> f32 {
        f32::from_bits(self.mic_level_bits.load(Ordering::Relaxed))
    }

    pub fn set_mic_level(&self, lvl: f32) {
        self.mic_level_bits
            .store(lvl.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn speaker_level(&self) -> f32 {
        f32::from_bits(self.speaker_level_bits.load(Ordering::Relaxed))
    }

    pub fn set_speaker_level(&self, lvl: f32) {
        self.speaker_level_bits
            .store(lvl.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn clear_buffers(&self) {
        if let Ok(mut tx) = self.tx_pcm_8k.lock() {
            tx.clear();
        }
        if let Ok(mut rx) = self.rx_pcm_8k.lock() {
            rx.clear();
        }
        self.set_mic_level(0.0);
        self.set_speaker_level(0.0);
    }

    pub fn push_rx_samples(&self, samples: &[i16]) {
        if let Ok(mut rx) = self.rx_pcm_8k.lock() {
            if rx.len() + samples.len() > 1920 {
                let drop_count = (rx.len() + samples.len()).saturating_sub(1280);
                for _ in 0..drop_count.min(rx.len()) {
                    rx.pop_front();
                }
            }
            rx.extend(samples.iter().copied());
        }
    }

    pub fn pop_tx_frame_160(&self, out: &mut [i16; 160]) -> bool {
        if let Ok(mut tx) = self.tx_pcm_8k.lock() {
            if tx.len() >= 160 {
                for slot in out.iter_mut() {
                    *slot = tx.pop_front().unwrap_or(0);
                }
                return true;
            }
        }
        false
    }

    pub fn queue_dtmf_tone(&self, digit: char) {
        let samples = synthesize_dtmf_8k(digit, 120);
        if !samples.is_empty() {
            if let Ok(mut tone) = self.tone_pcm_8k.lock() {
                if tone.len() < 4000 {
                    tone.extend(samples);
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct AudioEngine {
    pub state: Arc<AudioSharedState>,
}

impl AudioEngine {
    pub fn new() -> Self {
        Self {
            state: AudioSharedState::new(),
        }
    }

    /// Envolve um `AudioSharedState` EXISTENTE sem abrir dispositivo algum. Usado
    /// no modo `--remote` (F1): a janela lê os níveis de VU/flags deste espelho,
    /// que a thread de IPC preenche a partir das `ServerMsg::Hot`/`Flags`. O
    /// daemon é o dono do áudio real — aqui nenhum PipeWire/pactl é tocado.
    pub fn from_state(state: Arc<AudioSharedState>) -> Self {
        Self { state }
    }

    /// Lista todos os microfones reais do PipeWire / PulseAudio (ignorando monitores de saída)
    pub fn list_input_devices() -> Vec<AudioDeviceInfo> {
        let default_source = get_pactl_default("get-default-source");
        let mut list = Vec::new();

        if let Ok(out) = Command::new("pactl")
            .args(["-f", "json", "list", "sources"])
            .output()
        {
            if let Ok(nodes) = serde_json::from_slice::<Vec<PactlNodeJson>>(&out.stdout) {
                for n in nodes {
                    if n.name.is_empty() || n.name.ends_with(".monitor") {
                        continue;
                    }
                    let dev_class = n
                        .properties
                        .get("device.class")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let media_class = n
                        .properties
                        .get("media.class")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    if dev_class == "monitor" || media_class == "Audio/Sink" || !n.monitor_source.is_empty()
                    {
                        continue;
                    }

                    let desc = if n.description.trim().is_empty() {
                        n.name.clone()
                    } else {
                        n.description.trim().to_string()
                    };

                    let available = n
                        .ports
                        .iter()
                        .all(|p| p.availability != "not available");

                    let is_default = n.name == default_source;
                    list.push(AudioDeviceInfo {
                        id: n.name,
                        description: desc,
                        is_default,
                        available,
                    });
                }
            }
        }

        list.sort_by(|a, b| {
            b.is_default
                .cmp(&a.is_default)
                .then_with(|| b.available.cmp(&a.available))
                .then_with(|| a.description.cmp(&b.description))
        });
        list
    }

    /// Lista todos os alto-falantes e fones de ouvido do PipeWire / PulseAudio
    pub fn list_output_devices() -> Vec<AudioDeviceInfo> {
        let default_sink = get_pactl_default("get-default-sink");
        let mut list = Vec::new();

        if let Ok(out) = Command::new("pactl")
            .args(["-f", "json", "list", "sinks"])
            .output()
        {
            if let Ok(nodes) = serde_json::from_slice::<Vec<PactlNodeJson>>(&out.stdout) {
                for n in nodes {
                    if n.name.is_empty() {
                        continue;
                    }
                    let desc = if n.description.trim().is_empty() {
                        n.name.clone()
                    } else {
                        n.description.trim().to_string()
                    };

                    let available = n.ports.is_empty()
                        || n.ports.iter().any(|p| p.availability != "not available");

                    let is_default = n.name == default_sink;
                    list.push(AudioDeviceInfo {
                        id: n.name,
                        description: desc,
                        is_default,
                        available,
                    });
                }
            }
        }

        list.sort_by(|a, b| {
            b.is_default
                .cmp(&a.is_default)
                .then_with(|| b.available.cmp(&a.available))
                .then_with(|| a.description.cmp(&b.description))
        });
        list
    }

    pub fn start_call_audio(&self, input_device: Option<String>, output_device: Option<String>) {
        let prev_mode = self.state.mode.load(Ordering::Relaxed);
        // Se já estávamos em discagem ou early media (180/183), os streams do PipeWire/PulseAudio
        // (captura + reprodução) já estão abertos; basta liberar a transmissão do microfone sem
        // destruir/recriar os streams (evita corte de áudio ao atender 200 OK).
        if prev_mode == MODE_RINGBACK_OUTGOING || prev_mode == MODE_EARLY_MEDIA {
            if let Ok(mut tx) = self.state.tx_pcm_8k.lock() {
                tx.clear();
            }
            self.state.mode.store(MODE_IN_CALL, Ordering::Relaxed);
        } else {
            self.state.clear_buffers();
            self.state.mode.store(MODE_IN_CALL, Ordering::Relaxed);
            self.spawn_streams(input_device, output_device, true, false);
        }
    }

    /// Transiciona do tom de ringback local (425 Hz) para o áudio real da operadora (183 Early Media)
    /// sem reiniciar os streams do PipeWire/PulseAudio.
    pub fn start_early_media(&self) {
        let prev = self.state.mode.load(Ordering::Relaxed);
        if prev == MODE_RINGBACK_OUTGOING {
            self.state.mode.store(MODE_EARLY_MEDIA, Ordering::Relaxed);
        }
    }

    pub fn start_ringing_incoming(&self, output_device: Option<String>) {
        self.state.clear_buffers();
        self.state
            .mode
            .store(MODE_RINGING_INCOMING, Ordering::Relaxed);
        self.spawn_streams(None, output_device, false, false);
    }

    pub fn start_ringback_outgoing(
        &self,
        input_device: Option<String>,
        output_device: Option<String>,
    ) {
        self.state.clear_buffers();
        self.state
            .mode
            .store(MODE_RINGBACK_OUTGOING, Ordering::Relaxed);
        self.spawn_streams(input_device, output_device, true, false);
    }

    pub fn start_loopback_test(
        &self,
        input_device: Option<String>,
        output_device: Option<String>,
    ) {
        self.state.clear_buffers();
        self.state.mode.store(MODE_LOOPBACK_TEST, Ordering::Relaxed);
        self.spawn_streams(input_device, output_device, true, false);
    }

    pub fn stop_audio(&self) {
        self.state.mode.store(MODE_IDLE, Ordering::Relaxed);
        self.state.session_gen.fetch_add(1, Ordering::SeqCst);
        self.state.clear_buffers();
    }

    /// Reinicia os streams de áudio em tempo real caso o usuário troque o dispositivo nas configurações
    pub fn reload_devices_if_active(
        &self,
        input_device: Option<String>,
        output_device: Option<String>,
    ) {
        let mode = self.state.mode.load(Ordering::Relaxed);
        if mode != MODE_IDLE {
            let need_in = mode == MODE_IN_CALL
                || mode == MODE_RINGBACK_OUTGOING
                || mode == MODE_EARLY_MEDIA
                || mode == MODE_LOOPBACK_TEST;
            self.spawn_streams(input_device, output_device, need_in, false);
        }
    }

    pub fn play_keypad_dtmf(&self, digit: char, output_device: Option<String>) {
        self.state.queue_dtmf_tone(digit);
        if self.state.mode.load(Ordering::Relaxed) == MODE_IDLE {
            self.spawn_streams(None, output_device, false, true);
        }
    }

    fn spawn_streams(
        &self,
        input_device: Option<String>,
        output_device: Option<String>,
        need_input: bool,
        ephemeral_tone_only: bool,
    ) {
        let gen = self.state.session_gen.fetch_add(1, Ordering::SeqCst) + 1;

        // 1. Thread de Saída (Playback @ 8000 Hz S16LE Mono via PipeWire/PulseAudio)
        let state_out = self.state.clone();
        thread::Builder::new()
            .name("rustsip-pa-out".to_string())
            .spawn(move || {
                run_pulse_playback_loop(state_out, output_device, gen, ephemeral_tone_only);
            })
            .ok();

        // 2. Thread de Entrada (Capture @ 8000 Hz S16LE Mono via PipeWire/PulseAudio)
        if need_input {
            let state_in = self.state.clone();
            thread::Builder::new()
                .name("rustsip-pa-in".to_string())
                .spawn(move || {
                    run_pulse_capture_loop(state_in, input_device, gen);
                })
                .ok();
        }
    }
}

fn get_pactl_default(subcmd: &str) -> String {
    Command::new("pactl")
        .arg(subcmd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn open_pulse_stream(dir: c_int, device: Option<&str>, stream_title: &str) -> *mut pa_simple {
    let app_name = CString::new("RustSIP").unwrap();
    let title = CString::new(stream_title).unwrap();
    let ss = pa_sample_spec {
        format: PA_SAMPLE_S16LE,
        rate: 8000,
        channels: 1,
    };

    // Baixa latência: 20ms = 320 bytes (160 amostras i16 @ 8000 Hz)
    let attr = pa_buffer_attr {
        maxlength: u32::MAX,
        tlength: 640, // 40ms alvo de playback
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: 320, // 20ms fragmento de captura
    };

    let mut err: c_int = 0;

    if let Some(dev_str) = device {
        let trimmed = dev_str.trim();
        if !trimmed.is_empty() {
            if let Ok(dev_cstr) = CString::new(trimmed) {
                let ptr = unsafe {
                    pa_simple_new(
                        ptr::null(),
                        app_name.as_ptr(),
                        dir,
                        dev_cstr.as_ptr(),
                        title.as_ptr(),
                        &ss,
                        ptr::null(),
                        &attr,
                        &mut err,
                    )
                };
                if !ptr.is_null() {
                    return ptr;
                }
            }
        }
    }

    // Fallback para o dispositivo Padrão do Sistema no PipeWire/PulseAudio
    unsafe {
        pa_simple_new(
            ptr::null(),
            app_name.as_ptr(),
            dir,
            ptr::null(),
            title.as_ptr(),
            &ss,
            ptr::null(),
            &attr,
            &mut err,
        )
    }
}

fn run_pulse_capture_loop(
    state: Arc<AudioSharedState>,
    input_device: Option<String>,
    my_gen: u64,
) {
    let pa = open_pulse_stream(
        PA_STREAM_RECORD,
        input_device.as_deref(),
        "Microfone VoIP",
    );
    if pa.is_null() {
        return;
    }

    let mut raw_samples = [0i16; 160]; // 20ms @ 8000 Hz
    let mut err: c_int = 0;

    while state.session_gen.load(Ordering::Relaxed) == my_gen {
        let mode = state.mode.load(Ordering::Relaxed);
        if mode == MODE_IDLE {
            break;
        }

        let res = unsafe {
            pa_simple_read(
                pa,
                raw_samples.as_mut_ptr() as *mut c_void,
                std::mem::size_of_val(&raw_samples),
                &mut err,
            )
        };
        if res < 0 {
            break;
        }

        let muted = state.muted.load(Ordering::Relaxed);
        let gain = state.mic_gain();
        let mut peak: f32 = 0.0;

        for s in raw_samples.iter_mut() {
            let scaled = ((*s as f32) / 32768.0) * gain;
            let abs = scaled.abs();
            if abs > peak {
                peak = abs;
            }
            *s = if muted {
                0
            } else {
                (scaled.clamp(-1.0, 1.0) * 32767.0) as i16
            };
        }

        let prev_lvl = state.mic_level();
        let new_lvl = if muted {
            0.0
        } else {
            (prev_lvl * 0.65) + ((peak * 2.2).clamp(0.0, 1.0) * 0.35)
        };
        state.set_mic_level(new_lvl);

        if mode == MODE_LOOPBACK_TEST {
            state.push_rx_samples(&raw_samples);
        } else if mode == MODE_IN_CALL {
            if let Ok(mut tx) = state.tx_pcm_8k.lock() {
                if tx.len() + raw_samples.len() > 3200 {
                    let drop_n = (tx.len() + raw_samples.len()).saturating_sub(1600);
                    for _ in 0..drop_n.min(tx.len()) {
                        tx.pop_front();
                    }
                }
                tx.extend(raw_samples.iter().copied());
            }
        }
    }

    state.set_mic_level(0.0);
    unsafe {
        pa_simple_free(pa);
    }
}

fn run_pulse_playback_loop(
    state: Arc<AudioSharedState>,
    output_device: Option<String>,
    my_gen: u64,
    ephemeral_tone_only: bool,
) {
    let pa = open_pulse_stream(
        PA_STREAM_PLAYBACK,
        output_device.as_deref(),
        "Áudio VoIP",
    );
    if pa.is_null() {
        return;
    }

    let mut out_samples = [0i16; 160]; // 20ms @ 8000 Hz
    let mut synth_time: f32 = 0.0;
    let dt: f32 = 1.0 / 8000.0;
    let mut err: c_int = 0;
    let mut idle_empty_ticks: u32 = 0;

    while state.session_gen.load(Ordering::Relaxed) == my_gen {
        let mode = state.mode.load(Ordering::Relaxed);
        let vol = state.speaker_volume();
        let on_hold = state.on_hold.load(Ordering::Relaxed);
        let mut peak: f32 = 0.0;

        let mut had_tone = false;

        for slot in out_samples.iter_mut() {
            let mut val_f32: f32 = 0.0;

            // 1. Verifica fila de tom DTMF
            let mut sample_opt: Option<i16> = None;
            if let Ok(mut tone_q) = state.tone_pcm_8k.lock() {
                sample_opt = tone_q.pop_front();
                if sample_opt.is_some() {
                    had_tone = true;
                }
            }

            // 2. Verifica fila RX de chamada / loopback / early media
            if sample_opt.is_none()
                && !on_hold
                && (mode == MODE_IN_CALL
                    || mode == MODE_EARLY_MEDIA
                    || mode == MODE_LOOPBACK_TEST
                    || mode == MODE_RINGBACK_OUTGOING)
            {
                if let Ok(mut rx_q) = state.rx_pcm_8k.lock() {
                    sample_opt = rx_q.pop_front();
                }
            }

            if let Some(s) = sample_opt {
                val_f32 = (s as f32) / 32768.0;
            }

            // 3. Sintetizador de Ringback (chamando sem early media) ou Ringtone (recebendo)
            synth_time = (synth_time + dt) % 5.0;
            if mode == MODE_RINGBACK_OUTGOING && sample_opt.is_none() {
                if synth_time < 1.0 {
                    val_f32 += (2.0 * PI * 425.0 * synth_time).sin() * 0.25;
                }
            } else if mode == MODE_RINGING_INCOMING {
                let active = (synth_time < 0.4) || (synth_time > 0.6 && synth_time < 1.0);
                if active {
                    let wave = (2.0 * PI * 440.0 * synth_time).sin()
                        + (2.0 * PI * 554.37 * synth_time).sin()
                        + 0.5 * (2.0 * PI * 659.25 * synth_time).sin();
                    val_f32 += wave * 0.16;
                }
            }

            let final_f32 = (val_f32 * vol).clamp(-1.0, 1.0);
            let abs = final_f32.abs();
            if abs > peak {
                peak = abs;
            }
            *slot = (final_f32 * 32767.0) as i16;
        }

        let prev_lvl = state.speaker_level();
        let new_lvl = (prev_lvl * 0.65) + ((peak * 2.0).clamp(0.0, 1.0) * 0.35);
        state.set_speaker_level(new_lvl);

        let res = unsafe {
            pa_simple_write(
                pa,
                out_samples.as_ptr() as *const c_void,
                std::mem::size_of_val(&out_samples),
                &mut err,
            )
        };
        if res < 0 {
            break;
        }

        if ephemeral_tone_only || mode == MODE_IDLE {
            if !had_tone {
                idle_empty_ticks += 1;
                if idle_empty_ticks > 6 {
                    break;
                }
            } else {
                idle_empty_ticks = 0;
            }
        }
    }

    state.set_speaker_level(0.0);
    let _ = Duration::from_millis(1);
    unsafe {
        pa_simple_free(pa);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pipewire_device_enumeration() {
        let inputs = AudioEngine::list_input_devices();
        let outputs = AudioEngine::list_output_devices();
        println!("Microfones detectados ({}):", inputs.len());
        for d in &inputs {
            println!("  - {} [id={}]", d.display_label(), d.id);
        }
        println!("Saídas detectadas ({}):", outputs.len());
        for d in &outputs {
            println!("  - {} [id={}]", d.display_label(), d.id);
        }
        assert!(!inputs.is_empty(), "Deve detectar ao menos 1 microfone no PipeWire");
        assert!(!outputs.is_empty(), "Deve detectar ao menos 1 saída no PipeWire");
    }
}

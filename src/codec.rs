use std::f32::consts::PI;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Pcmu, // Payload Type 0 (G.711 u-law, 8000 Hz)
    Pcma, // Payload Type 8 (G.711 A-law, 8000 Hz)
}

impl AudioCodec {
    pub fn payload_type(&self) -> u8 {
        match self {
            AudioCodec::Pcmu => 0,
            AudioCodec::Pcma => 8,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            AudioCodec::Pcmu => "PCMU (G.711 μ-law)",
            AudioCodec::Pcma => "PCMA (G.711 A-law)",
        }
    }

    pub fn from_payload_type(pt: u8) -> Option<Self> {
        match pt {
            0 => Some(AudioCodec::Pcmu),
            8 => Some(AudioCodec::Pcma),
            _ => None,
        }
    }

    pub fn encode_sample(&self, sample: i16) -> u8 {
        match self {
            AudioCodec::Pcmu => linear_to_ulaw(sample),
            AudioCodec::Pcma => linear_to_alaw(sample),
        }
    }

    pub fn decode_sample(&self, byte: u8) -> i16 {
        match self {
            AudioCodec::Pcmu => ulaw_to_linear(byte),
            AudioCodec::Pcma => alaw_to_linear(byte),
        }
    }

    pub fn encode_frame(&self, pcm: &[i16], out: &mut Vec<u8>) {
        out.clear();
        out.reserve(pcm.len());
        for &s in pcm {
            out.push(self.encode_sample(s));
        }
    }

    pub fn decode_frame(&self, data: &[u8], out: &mut Vec<i16>) {
        out.clear();
        out.reserve(data.len());
        for &b in data {
            out.push(self.decode_sample(b));
        }
    }
}

/// ITU-T G.711 μ-law encoder (16-bit signed linear PCM -> 8-bit μ-law)
pub fn linear_to_ulaw(pcm_val: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32635;

    let mut sample = pcm_val as i32;
    let sign = if sample < 0 {
        sample = -sample;
        0x80
    } else {
        0x00
    };

    if sample > CLIP {
        sample = CLIP;
    }
    sample += BIAS;

    let mut exponent: u8 = 7;
    let mut exp_mask: i32 = 0x4000;
    while (sample & exp_mask) == 0 && exponent > 0 {
        exponent -= 1;
        exp_mask >>= 1;
    }

    let mantissa = ((sample >> (exponent + 3)) & 0x0F) as u8;
    let ulaw_byte = sign | (exponent << 4) | mantissa;
    !ulaw_byte
}

/// ITU-T G.711 μ-law decoder (8-bit μ-law -> 16-bit signed linear PCM)
pub fn ulaw_to_linear(ulaw_byte: u8) -> i16 {
    let u = !ulaw_byte;
    let sign = u & 0x80;
    let exponent = ((u >> 4) & 0x07) as i32;
    let mantissa = (u & 0x0F) as i32;

    let mut sample = ((mantissa << 3) + 0x84) << exponent;
    sample -= 0x84;

    if sign != 0 {
        -(sample as i16)
    } else {
        sample as i16
    }
}

/// ITU-T G.711 A-law encoder (16-bit signed linear PCM -> 8-bit A-law)
pub fn linear_to_alaw(pcm_val: i16) -> u8 {
    let mut sample = pcm_val as i32;
    let mask: u8;

    if sample >= 0 {
        mask = 0xD5;
    } else {
        mask = 0x55;
        sample = -sample - 1;
        if sample < 0 {
            sample = 0;
        }
    }

    if sample > 32767 {
        sample = 32767;
    }

    let aval: u8 = if sample < 256 {
        (sample >> 4) as u8
    } else {
        let mut seg: u8 = 1;
        let mut temp = sample >> 8;
        while (temp > 1) && seg < 7 {
            temp >>= 1;
            seg += 1;
        }
        let mant = ((sample >> (seg + 3)) & 0x0F) as u8;
        (seg << 4) | mant
    };

    aval ^ mask
}

/// ITU-T G.711 A-law decoder (8-bit A-law -> 16-bit signed linear PCM)
pub fn alaw_to_linear(alaw_byte: u8) -> i16 {
    let a = alaw_byte ^ 0x55;
    let sign = a & 0x80;
    let seg = ((a >> 4) & 0x07) as i32;
    let mut t = ((a & 0x0F) as i32) << 4;

    match seg {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= seg - 1;
        }
    }

    if sign != 0 {
        t as i16
    } else {
        -(t as i16)
    }
}

/// Retorna o código RFC 4733 (RFC 2833) telephone-event para um dígito DTMF
pub fn dtmf_to_event_code(digit: char) -> Option<u8> {
    match digit {
        '0'..='9' => Some((digit as u8) - b'0'),
        '*' => Some(10),
        '#' => Some(11),
        'A' | 'a' => Some(12),
        'B' | 'b' => Some(13),
        'C' | 'c' => Some(14),
        'D' | 'd' => Some(15),
        _ => None,
    }
}

/// Retorna as duas frequências (Hz) de um dígito DTMF
pub fn dtmf_frequencies(digit: char) -> Option<(f32, f32)> {
    let (row, col) = match digit {
        '1' => (697.0, 1209.0),
        '2' => (697.0, 1336.0),
        '3' => (697.0, 1477.0),
        'A' | 'a' => (697.0, 1633.0),
        '4' => (770.0, 1209.0),
        '5' => (770.0, 1336.0),
        '6' => (770.0, 1477.0),
        'B' | 'b' => (770.0, 1633.0),
        '7' => (852.0, 1209.0),
        '8' => (852.0, 1336.0),
        '9' => (852.0, 1477.0),
        'C' | 'c' => (852.0, 1633.0),
        '*' => (941.0, 1209.0),
        '0' => (941.0, 1336.0),
        '#' => (941.0, 1477.0),
        'D' | 'd' => (941.0, 1633.0),
        _ => return None,
    };
    Some((row, col))
}

/// Gera amostras PCM 8000 Hz para um tom DTMF (ex: 120ms)
pub fn synthesize_dtmf_8k(digit: char, duration_ms: u32) -> Vec<i16> {
    let Some((f1, f2)) = dtmf_frequencies(digit) else {
        return Vec::new();
    };
    let num_samples = (8 * duration_ms) as usize;
    let mut out = Vec::with_capacity(num_samples);
    for i in 0..num_samples {
        let t = (i as f32) / 8000.0;
        // Envelope suave nas bordas para evitar estalos
        let env = if i < 64 {
            (i as f32) / 64.0
        } else if i > num_samples.saturating_sub(64) {
            ((num_samples - i) as f32) / 64.0
        } else {
            1.0
        };
        let val = ((2.0 * PI * f1 * t).sin() + (2.0 * PI * f2 * t).sin()) * 0.25 * env;
        out.push((val * 32767.0) as i16);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ulaw_roundtrip() {
        for sample in [-24000i16, -8000, -1000, -100, 0, 100, 1000, 8000, 24000] {
            let encoded = linear_to_ulaw(sample);
            let decoded = ulaw_to_linear(encoded);
            let diff = (sample as i32 - decoded as i32).abs();
            assert!(diff <= 1200, "sample={} decoded={} diff={}", sample, decoded, diff);
        }
    }

    #[test]
    fn test_alaw_roundtrip() {
        for sample in [-24000i16, -8000, -1000, -100, 0, 100, 1000, 8000, 24000] {
            let encoded = linear_to_alaw(sample);
            let decoded = alaw_to_linear(encoded);
            let diff = (sample as i32 - decoded as i32).abs();
            assert!(diff <= 1200, "sample={} decoded={} diff={}", sample, decoded, diff);
        }
    }

    /// Verificação de POLARIDADE do decoder A-law (item 1 da auditoria de robustez).
    ///
    /// O roundtrip acima NÃO pega inversão de sinal: A-law é simétrico e a
    /// tolerância ±1200 esconde um espelhamento. Este teste compara o SINAL
    /// (não a magnitude) contra vetores conhecidos do decoder de referência
    /// ITU-T G.711, Anexo A:
    ///   - byte 0xD5 = menor amostra POSITIVA  -> +8
    ///   - byte 0x55 = menor amostra NEGATIVA  -> -8
    ///   - segmentos altos: 0xAA -> +32256, 0x2A -> -32256
    ///   - 0x80 -> +5504, 0x00 -> -5504
    ///
    /// Conclusão da auditoria: a polaridade atual está CORRETA (0xD5>0, 0x55<0),
    /// portanto nenhuma mudança no decoder — só este teste de regressão fixa o
    /// contrato de sinal para que uma futura inversão falhe o build.
    #[test]
    fn test_alaw_polarity_itu_g711_vectors() {
        // (byte A-law, amostra linear esperada) — valores do decoder ITU-T de referência.
        let vectors: &[(u8, i16)] = &[
            (0xD5, 8),       // menor positivo
            (0x55, -8),      // menor negativo
            (0xD4, 24),      // positivo próximo de zero
            (0x54, -24),     // negativo próximo de zero
            (0xAA, 32256),   // positivo de segmento alto
            (0x2A, -32256),  // negativo de segmento alto
            (0x80, 5504),    // positivo meio de escala
            (0x00, -5504),   // negativo meio de escala
        ];
        for &(byte, expected) in vectors {
            let got = alaw_to_linear(byte);
            assert_eq!(
                got, expected,
                "A-law 0x{:02X}: esperado {} (sinal {}), obtido {} (sinal {}) -- POLARIDADE INVERTIDA?",
                byte,
                expected,
                expected.signum(),
                got,
                got.signum()
            );
        }
        // Reforço explícito de sinal nos dois bytes âncora.
        assert!(alaw_to_linear(0xD5) > 0, "0xD5 deve decodificar POSITIVO");
        assert!(alaw_to_linear(0x55) < 0, "0x55 deve decodificar NEGATIVO");
    }
}

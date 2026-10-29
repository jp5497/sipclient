//! Cifra LOCAL da senha persistida no config.json (item 7 da auditoria).
//!
//! ponytail: ISTO É OFUSCAÇÃO LOCAL, NÃO CRIPTOGRAFIA FORTE.
//! - A chave é DERIVÁVEL por qualquer processo que rode como o mesmo usuário na
//!   mesma máquina (vem de /etc/machine-id + uid + um salt fixo embutido no
//!   binário). Um atacante com esse acesso reproduz a chave e decifra.
//! - A senha fica em CLARO na memória do processo (o Digest SIP precisa dela);
//!   só a FORMA PERSISTIDA em disco muda.
//! - Objetivo: evitar que a senha apareça em texto plano no arquivo de config
//!   (backup, sync de dotfiles, ombro). Nada além disso.
//! Upgrade path: mover o segredo para o keyring do SO (libsecret / Secret
//! Service, gnome-keyring) atrás do prefixo de versão "v1:".
//!
//! Construção (revisada pelo dev-security):
//! - HMAC-MD5 (RFC 2104) sobre o crate `md-5` — NUNCA md5(chave||dados) cru
//!   (vulnerável a extensão de comprimento).
//! - Chaves separadas por rótulo: k_enc, k_mac.
//! - Keystream modo CTR: HMAC-MD5(k_enc, nonce || ctr_le); ct = pt XOR keystream.
//! - MAC autenticando versão+nonce+ct (encrypt-then-MAC).
//! - nonce de 16 bytes NOVO por save (keystream nunca reutilizado).
//! - Formato: "v1:" hex(nonce) ":" hex(ct) ":" hex(tag).

use md5::{Digest, Md5};

const MD5_BLOCK: usize = 64;
const MD5_OUT: usize = 16;
const NONCE_LEN: usize = 16;

/// Salt fixo da aplicação (NÃO é segredo — é um rótulo de domínio embutido no
/// binário para que a chave derivada seja específica do RustSIP).
const APP_SALT: &[u8] = b"rustsip-config-key-salt-v1";

/// HMAC-MD5 (RFC 2104). ipad/opad sobre bloco de 64 bytes.
fn hmac_md5(key: &[u8], data: &[u8]) -> [u8; MD5_OUT] {
    let mut key_block = [0u8; MD5_BLOCK];
    if key.len() > MD5_BLOCK {
        let mut h = Md5::new();
        h.update(key);
        let d = h.finalize();
        key_block[..MD5_OUT].copy_from_slice(&d);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; MD5_BLOCK];
    let mut opad = [0x5cu8; MD5_BLOCK];
    for i in 0..MD5_BLOCK {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }

    let mut hi = Md5::new();
    hi.update(ipad);
    hi.update(data);
    let inner = hi.finalize();

    let mut ho = Md5::new();
    ho.update(opad);
    ho.update(inner);
    let out = ho.finalize();

    let mut res = [0u8; MD5_OUT];
    res.copy_from_slice(&out);
    res
}

/// Lê /etc/machine-id (128 bits estáveis por instalação, FORA do $HOME).
/// Fallback para hostname apenas se o arquivo não existir (ex.: containers) —
/// esse fallback é MAIS FRACO (hostname é previsível/compartilhável).
fn machine_id() -> Vec<u8> {
    if let Ok(id) = std::fs::read_to_string("/etc/machine-id") {
        let trimmed = id.trim();
        if !trimmed.is_empty() {
            return trimmed.as_bytes().to_vec();
        }
    }
    match std::fs::read_to_string("/etc/hostname") {
        Ok(h) if !h.trim().is_empty() => h.trim().as_bytes().to_vec(),
        _ => b"rustsip-unknown-host".to_vec(),
    }
}

/// uid numérico do dono de `anchor_path` (sem libc).
#[cfg(unix)]
fn uid_of(anchor_path: &std::path::Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(anchor_path).map(|m| m.uid()).unwrap_or(0)
}
#[cfg(not(unix))]
fn uid_of(_anchor_path: &std::path::Path) -> u32 {
    0
}

/// Chave mestra: HMAC-MD5(APP_SALT, machine_id || ":" || uid).
fn derive_key(anchor_path: &std::path::Path) -> [u8; MD5_OUT] {
    let mut material = machine_id();
    material.push(b':');
    material.extend_from_slice(uid_of(anchor_path).to_string().as_bytes());
    hmac_md5(APP_SALT, &material)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Some(out)
}

/// Keystream CTR: bloco_i = HMAC-MD5(k_enc, nonce || i_u32_le).
fn keystream(k_enc: &[u8; MD5_OUT], nonce: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + MD5_OUT);
    let mut ctr: u32 = 0;
    while out.len() < len {
        let mut input = Vec::with_capacity(nonce.len() + 4);
        input.extend_from_slice(nonce);
        input.extend_from_slice(&ctr.to_le_bytes());
        let block = hmac_md5(k_enc, &input);
        out.extend_from_slice(&block);
        ctr = ctr.wrapping_add(1);
    }
    out.truncate(len);
    out
}

/// Comparação em tempo ~constante (evita timing-oracle no MAC).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Gera um nonce aleatório de 16 bytes (rand 0.8 thread_rng é CSPRNG).
pub fn random_nonce() -> [u8; NONCE_LEN] {
    use rand::RngCore;
    let mut n = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut n);
    n
}

/// Cifra `plaintext` (a senha) no formato "v1:nonce:ct:tag".
/// NUNCA inclui plaintext/chave/keystream em mensagens de erro (não há erro aqui).
pub fn encrypt_password(
    plaintext: &str,
    anchor_path: &std::path::Path,
    nonce: [u8; NONCE_LEN],
) -> String {
    let key = derive_key(anchor_path);
    let k_enc = hmac_md5(&key, b"rustsip-enc-v1");
    let k_mac = hmac_md5(&key, b"rustsip-mac-v1");

    let pt = plaintext.as_bytes();
    let ks = keystream(&k_enc, &nonce, pt.len());
    let ct: Vec<u8> = pt.iter().zip(ks.iter()).map(|(p, k)| p ^ k).collect();

    // tag = HMAC-MD5(k_mac, "v1" || nonce || ct)  (encrypt-then-MAC)
    let mut mac_input = Vec::with_capacity(2 + nonce.len() + ct.len());
    mac_input.extend_from_slice(b"v1");
    mac_input.extend_from_slice(&nonce);
    mac_input.extend_from_slice(&ct);
    let tag = hmac_md5(&k_mac, &mac_input);

    format!(
        "v1:{}:{}:{}",
        hex_encode(&nonce),
        hex_encode(&ct),
        hex_encode(&tag)
    )
}

/// Resultado tolerante da decifragem de um campo de senha persistido.
pub enum DecryptResult {
    /// Texto cifrado "v1:..." decifrado com sucesso.
    Ok(String),
    /// Valor legado em TEXTO PLANO (sem prefixo "v1:") — carrega como está e
    /// deve ser re-cifrado no próximo save (retrocompat).
    Legacy(String),
    /// Falha de decifragem (chave errada, tag adulterada, hex inválido, troca
    /// de máquina). NÃO é fatal: o chamador carrega senha vazia e marca
    /// reentrada, PRESERVANDO o resto da config.
    Failed,
}

/// Decifra um campo de senha persistido de forma TOLERANTE e ISOLADA.
/// Nunca entra em pânico; nunca vaza plaintext/chave em log.
pub fn decrypt_password(stored: &str, anchor_path: &std::path::Path) -> DecryptResult {
    let Some(rest) = stored.strip_prefix("v1:") else {
        // Sem prefixo de versão => config legado em texto plano.
        return DecryptResult::Legacy(stored.to_string());
    };

    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 3 {
        return DecryptResult::Failed;
    }
    let (Some(nonce), Some(ct), Some(tag)) =
        (hex_decode(parts[0]), hex_decode(parts[1]), hex_decode(parts[2]))
    else {
        return DecryptResult::Failed;
    };
    if nonce.len() != NONCE_LEN || tag.len() != MD5_OUT {
        return DecryptResult::Failed;
    }

    let key = derive_key(anchor_path);
    let k_mac = hmac_md5(&key, b"rustsip-mac-v1");

    // Verifica a tag ANTES de decifrar (encrypt-then-MAC).
    let mut mac_input = Vec::with_capacity(2 + nonce.len() + ct.len());
    mac_input.extend_from_slice(b"v1");
    mac_input.extend_from_slice(&nonce);
    mac_input.extend_from_slice(&ct);
    let expected = hmac_md5(&k_mac, &mac_input);
    if !ct_eq(&expected, &tag) {
        return DecryptResult::Failed; // chave errada ou adulteração
    }

    let k_enc = hmac_md5(&key, b"rustsip-enc-v1");
    let ks = keystream(&k_enc, &nonce, ct.len());
    let pt: Vec<u8> = ct.iter().zip(ks.iter()).map(|(c, k)| c ^ k).collect();
    match String::from_utf8(pt) {
        Ok(s) => DecryptResult::Ok(s),
        Err(_) => DecryptResult::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn anchor() -> &'static Path {
        Path::new("/tmp")
    }

    #[test]
    fn test_roundtrip() {
        let anchor = anchor();
        let enc = encrypt_password("senhaSupersecreta!42", anchor, random_nonce());
        assert!(enc.starts_with("v1:"));
        match decrypt_password(&enc, anchor) {
            DecryptResult::Ok(p) => assert_eq!(p, "senhaSupersecreta!42"),
            _ => panic!("roundtrip falhou"),
        }
    }

    #[test]
    fn test_two_saves_differ_ciphertext() {
        let anchor = anchor();
        let a = encrypt_password("mesmaSenha", anchor, random_nonce());
        let b = encrypt_password("mesmaSenha", anchor, random_nonce());
        assert_ne!(a, b, "nonce novo por save => ciphertexts diferentes");
        for e in [a, b] {
            match decrypt_password(&e, anchor) {
                DecryptResult::Ok(p) => assert_eq!(p, "mesmaSenha"),
                _ => panic!("deveria decifrar"),
            }
        }
    }

    #[test]
    fn test_tampered_tag_fails_tolerant() {
        let anchor = anchor();
        let enc = encrypt_password("segredo", anchor, random_nonce());
        let mut chars: Vec<char> = enc.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'a' { 'b' } else { 'a' };
        let tampered: String = chars.into_iter().collect();
        assert!(matches!(
            decrypt_password(&tampered, anchor),
            DecryptResult::Failed
        ));
    }

    #[test]
    fn test_corrupt_nonce_fails_tolerant() {
        let anchor = anchor();
        let enc = encrypt_password("segredo", anchor, random_nonce());
        let parts: Vec<&str> = enc.splitn(4, ':').collect();
        let bad = format!(
            "v1:{}:{}:{}",
            "00000000000000000000000000000000", parts[2], parts[3]
        );
        assert!(matches!(decrypt_password(&bad, anchor), DecryptResult::Failed));
    }

    #[test]
    fn test_malformed_hex_fails_tolerant() {
        let anchor = anchor();
        assert!(matches!(
            decrypt_password("v1:zz:zz:zz", anchor),
            DecryptResult::Failed
        ));
        assert!(matches!(
            decrypt_password("v1:deadbeef", anchor),
            DecryptResult::Failed
        ));
    }

    #[test]
    fn test_legacy_plaintext_loads() {
        let anchor = anchor();
        match decrypt_password("senhaAntigaEmTextoPlano", anchor) {
            DecryptResult::Legacy(p) => assert_eq!(p, "senhaAntigaEmTextoPlano"),
            _ => panic!("texto plano legado deve virar Legacy"),
        }
    }

    #[test]
    fn test_ciphertext_does_not_contain_password() {
        let anchor = anchor();
        let secret = "MinhaSenhaLiteral123";
        let enc = encrypt_password(secret, anchor, random_nonce());
        assert!(
            !enc.contains(secret),
            "ciphertext não pode conter a senha em claro"
        );
    }

    #[test]
    fn test_hmac_md5_rfc2104_vector() {
        // RFC 2104 / RFC 2202: key=0x0b*16, data="Hi There"
        // => 9294727a3638bb1c13f48ef8158bfc9d
        let key = [0x0bu8; 16];
        let mac = hmac_md5(&key, b"Hi There");
        assert_eq!(hex_encode(&mac), "9294727a3638bb1c13f48ef8158bfc9d");
    }
}

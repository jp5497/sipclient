use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportProtocol {
    Udp,
    Tcp,
}

impl TransportProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransportProtocol::Udp => "UDP",
            TransportProtocol::Tcp => "TCP",
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct SipAccountConfig {
    pub display_name: String,
    pub username: String,
    pub auth_id: String,
    pub password: String,
    /// Ex: "sip.meuservidor.com:5060" ou "200.100.50.25"
    pub server: String,
    /// Domínio/Realm SIP opcional (se vazio, usa o host do servidor)
    pub domain: String,
    pub transport: TransportProtocol,
    pub register_expires: u32,
    pub use_stun: bool,
    pub stun_server: String,
    pub auto_connect: bool,
}

// Debug manual: a senha NUNCA aparece num `{:?}` (log/erro). Um
// #[derive(Debug)] com password:String vazaria o segredo em qualquer dump.
impl std::fmt::Debug for SipAccountConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SipAccountConfig")
            .field("display_name", &self.display_name)
            .field("username", &self.username)
            .field("auth_id", &self.auth_id)
            .field("password", &"<redacted>")
            .field("server", &self.server)
            .field("domain", &self.domain)
            .field("transport", &self.transport)
            .field("register_expires", &self.register_expires)
            .field("use_stun", &self.use_stun)
            .field("stun_server", &self.stun_server)
            .field("auto_connect", &self.auto_connect)
            .finish()
    }
}

impl Default for SipAccountConfig {
    fn default() -> Self {
        Self {
            display_name: String::new(),
            username: String::new(),
            auth_id: String::new(),
            password: String::new(),
            server: String::new(),
            domain: String::new(),
            transport: TransportProtocol::Udp,
            register_expires: 300,
            use_stun: true,
            stun_server: "stun.l.google.com:19302".to_string(),
            auto_connect: true,
        }
    }
}

impl SipAccountConfig {
    pub fn effective_domain(&self) -> String {
        let dom = self.domain.trim();
        if !dom.is_empty() {
            return dom.to_string();
        }
        let srv = self.server.trim();
        if let Some((host, _port)) = srv.rsplit_once(':') {
            host.to_string()
        } else {
            srv.to_string()
        }
    }

    pub fn effective_server_addr(&self) -> String {
        let srv = self.server.trim();
        if srv.contains(':') {
            srv.to_string()
        } else {
            format!("{}:5060", srv)
        }
    }

    pub fn effective_auth_id(&self) -> String {
        let auth = self.auth_id.trim();
        if !auth.is_empty() {
            auth.to_string()
        } else {
            self.username.trim().to_string()
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.server.trim().is_empty() && !self.username.trim().is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AudioConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub mic_gain: f32,
    pub speaker_volume: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            mic_gain: 1.0,
            speaker_volume: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Contact {
    pub id: String,
    pub name: String,
    pub number: String,
    pub notes: String,
}

/// Monta um `Contact` validando/normalizando os campos, com o `id` fornecido
/// pelo chamador (o daemon gera o id a partir do relógio — manter a geração do
/// id FORA desta função a torna pura e testável). Regra atual: `name` e
/// `number` são trimados e, se qualquer um ficar vazio, o contato é rejeitado
/// (`None`); `notes` é trimado. Fonte única usada pelo handler `AddContact`.
pub fn build_contact(name: &str, number: &str, notes: &str, id: String) -> Option<Contact> {
    let name = name.trim().to_string();
    let number = number.trim().to_string();
    if name.is_empty() || number.is_empty() {
        return None;
    }
    Some(Contact {
        id,
        name,
        number,
        notes: notes.trim().to_string(),
    })
}

/// Remove da lista o contato cujo `id` casa, preservando os demais. Retorna
/// `true` se removeu algo (o daemon só faz broadcast quando mudou). Fonte única
/// usada pelo handler `DeleteContact`.
pub fn remove_contact(contacts: &mut Vec<Contact>, id: &str) -> bool {
    let before = contacts.len();
    contacts.retain(|ct| ct.id != id);
    contacts.len() != before
}

/// Cópia edge-triggered de um campo-espelho (conta/áudio na janela remota):
/// aplica `incoming` sobre `current` e atualiza `prev` SOMENTE quando o valor
/// vindo do daemon mudou em relação ao último visto. Retorna `true` quando
/// aplicou. Mantém o formulário do usuário intacto entre mudanças do daemon
/// (ver `prev_mirror_*` em `ui.rs`).
pub fn apply_if_changed<T: Clone + PartialEq>(current: &mut T, incoming: &T, prev: &mut T) -> bool {
    if incoming != prev {
        *current = incoming.clone();
        *prev = incoming.clone();
        true
    } else {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallDirection {
    Outgoing,
    Incoming,
    Missed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallHistoryEntry {
    pub timestamp: String,
    pub number: String,
    pub name: String,
    pub direction: CallDirection,
    pub duration_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub account: SipAccountConfig,
    pub audio: AudioConfig,
    pub contacts: Vec<Contact>,
    pub history: Vec<CallHistoryEntry>,
    /// Marcado em memória (NÃO persistido) quando a senha cifrada não pôde ser
    /// decifrada no load (chave errada / troca de máquina / adulteração). A UI
    /// pode usar isto para pedir que o usuário redigite a senha. A senha fica
    /// vazia nesse caso, mas o RESTO da config é preservado.
    #[serde(skip)]
    pub password_needs_reentry: bool,
    /// Valor CRU do campo password como veio do disco (cifrado ou legado). NÃO
    /// persistido. Serve de salvaguarda: se a decifragem falhou (transitória —
    /// /etc/machine-id ilegível, HOME/uid diferente), um save() subsequente com
    /// senha vazia regrava ESTE valor em vez de "" — assim não apaga o
    /// ciphertext recuperável de forma definitiva.
    #[serde(skip)]
    pub stored_password_raw: String,
}

impl AppConfig {
    fn config_dir() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let dir = PathBuf::from(home).join(".config").join("rustsip");
        // Diretório 0700 (só o dono) — só Unix. DirBuilder com mode evita a
        // janela em que o dir existe com permissão default antes do chmod.
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            use std::os::unix::fs::PermissionsExt;
            if !dir.exists() {
                let _ = fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir);
            } else {
                // Corrige um dir pré-existente com permissão larga.
                let _ = fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = fs::create_dir_all(&dir);
        }
        dir
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        let dir = Self::config_dir();
        let Ok(data) = fs::read_to_string(&path) else {
            return Self::default();
        };
        let Ok(mut cfg) = serde_json::from_str::<AppConfig>(&data) else {
            // Parse do JSON inteiro falhou (arquivo corrompido). Só aqui
            // retornamos default — a FALHA DE DECIFRAGEM da senha NUNCA chega
            // aqui (é isolada no campo abaixo), então contatos/histórico não
            // são perdidos por causa de uma senha que não decifrou.
            return Self::default();
        };

        // Guarda o valor CRU do disco ANTES de decifrar (salvaguarda #1).
        cfg.stored_password_raw = cfg.account.password.clone();

        // Decifragem TOLERANTE e ISOLADA NO CAMPO password.
        let mut migrate_legacy = false;
        match crate::crypto::decrypt_password(&cfg.account.password, &dir) {
            crate::crypto::DecryptResult::Ok(pw) => {
                cfg.account.password = pw;
                cfg.password_needs_reentry = false;
            }
            crate::crypto::DecryptResult::Legacy(pw) => {
                // Texto plano legado: carrega e re-cifra+0600 já no load (M2),
                // para não ficar legível por outros até o 1º save do usuário.
                cfg.account.password = pw;
                cfg.password_needs_reentry = false;
                migrate_legacy = !cfg.account.password.is_empty();
            }
            crate::crypto::DecryptResult::Failed => {
                // Não apaga a config: zera só a senha e sinaliza reentrada. O
                // ciphertext cru fica em stored_password_raw para o save()
                // preservá-lo em vez de gravar "".
                cfg.account.password = String::new();
                cfg.password_needs_reentry = true;
            }
        }

        if migrate_legacy {
            // Re-cifra o legado imediatamente (grava com 0600 via write_atomic).
            cfg.save();
        }

        cfg
    }

    /// Decide o valor do campo `password` a GRAVAR em disco, sem tocar `self`.
    /// Regras (salvaguarda #1):
    /// - reentrada pendente + senha vazia em memória => regrava o cru do disco
    ///   INALTERADO (não apaga o ciphertext recuperável numa falha transitória);
    /// - senha não-vazia => re-cifra com nonce novo;
    /// - senha vazia sem flag => grava "" (conta sem senha, intencional).
    pub fn persisted_password_field(&self, dir: &std::path::Path) -> String {
        if self.password_needs_reentry && self.account.password.is_empty() {
            self.stored_password_raw.clone()
        } else if !self.account.password.is_empty() {
            let nonce = crate::crypto::random_nonce();
            crate::crypto::encrypt_password(&self.account.password, dir, nonce)
        } else {
            String::new()
        }
    }

    pub fn save(&self) {
        let path = Self::config_path();
        let dir = Self::config_dir();

        // Cifra a senha SÓ na forma persistida; a cópia em memória (self) fica
        // intacta em claro para o Digest.
        let mut to_persist = self.clone();
        to_persist.account.password = self.persisted_password_field(&dir);

        let json = match serde_json::to_string_pretty(&to_persist) {
            Ok(j) => j,
            Err(e) => {
                // Loga a falha SEM conteúdo (nada de senha/JSON no log).
                eprintln!("rustsip: falha ao serializar config: {}", e);
                return;
            }
        };

        if let Err(e) = Self::write_atomic(&dir, &path, json.as_bytes()) {
            eprintln!("rustsip: falha ao gravar config: {}", e);
        }
    }

    /// Escrita atômica com permissão 0600 em todo momento: grava num tmp no
    /// MESMO diretório criado com create_new+mode(0o600), fsync, depois rename.
    /// O arquivo final nunca passa por 0644 — isso também corrige configs
    /// antigas que estavam 0644.
    #[cfg(unix)]
    fn write_atomic(dir: &std::path::Path, path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;

        // Nome de tmp único no mesmo dir (mesmo filesystem => rename atômico).
        let pid = std::process::id();
        let tmp = dir.join(format!(".config.json.tmp.{}", pid));
        // Remove um tmp órfão de uma execução anterior (create_new falharia).
        let _ = fs::remove_file(&tmp);

        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        // Garante 0600 também no destino após o rename (herdado do tmp).
        fs::rename(&tmp, path)?;
        let _ = fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        Ok(())
    }

    #[cfg(not(unix))]
    fn write_atomic(dir: &std::path::Path, path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let tmp = dir.join(format!(".config.json.tmp.{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        {
            let mut f = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn anchor() -> &'static Path {
        Path::new("/tmp")
    }

    /// BLOQUEADOR #1: decifragem falhou (password_needs_reentry) e o usuário
    /// não digitou senha nova => um save() NÃO pode sobrescrever o ciphertext
    /// recuperável com "". O campo persistido deve ser IDÊNTICO ao cru do disco.
    #[test]
    fn test_failed_decrypt_save_preserves_ciphertext() {
        let raw_from_disk =
            "v1:0123456789abcdef0123456789abcdef:deadbeef:00112233445566778899aabbccddeeff"
                .to_string();
        let mut cfg = AppConfig::default();
        cfg.stored_password_raw = raw_from_disk.clone();
        cfg.account.password = String::new(); // zerada no load por falha
        cfg.password_needs_reentry = true;

        let persisted = cfg.persisted_password_field(anchor());
        assert_eq!(
            persisted, raw_from_disk,
            "save em estado de reentrada deve regravar o cru INALTERADO, não \"\""
        );
    }

    /// Senha nova não-vazia re-cifra (vira container v1 novo) e NÃO contém a
    /// senha em claro.
    #[test]
    fn test_new_password_reencrypts() {
        let mut cfg = AppConfig::default();
        cfg.account.password = "SenhaNova123".to_string();
        cfg.password_needs_reentry = false;
        let persisted = cfg.persisted_password_field(anchor());
        assert!(persisted.starts_with("v1:"));
        assert!(!persisted.contains("SenhaNova123"));
    }

    /// Conta deliberadamente sem senha (vazia, sem flag) grava "".
    #[test]
    fn test_empty_password_no_flag_writes_empty() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.persisted_password_field(anchor()), "");
    }

    // --- B3 (gate F2): lógica de comando de contatos e edge-trigger, testada
    // SEM tocar o ~/.config real (funções puras extraídas de daemon/ui). ---

    fn ct(id: &str) -> Contact {
        Contact {
            id: id.to_string(),
            name: format!("nome-{id}"),
            number: format!("55{id}"),
            notes: String::new(),
        }
    }

    /// DeleteContact: remove exatamente o id pedido, preserva os outros, e
    /// retorna true só quando removeu algo.
    #[test]
    fn test_remove_contact_by_id() {
        let mut list = vec![ct("a"), ct("b"), ct("c")];

        assert!(remove_contact(&mut list, "b"), "remover id existente => true");
        assert_eq!(
            list.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "c"],
            "mantém os demais na ordem, remove só o 'b'"
        );

        // id inexistente: lista intacta, retorna false (daemon não faz broadcast).
        assert!(!remove_contact(&mut list, "zzz"), "id ausente => false");
        assert_eq!(list.len(), 2);
    }

    /// AddContact: gera o contato com o id dado, trima os campos, e REJEITA
    /// quando name OU number ficam vazios após o trim.
    #[test]
    fn test_build_contact_validates_and_normalizes() {
        // Feliz: trima name/number/notes, usa o id fornecido.
        let c = build_contact("  Alice  ", "  1001 ", "  vip ", "id1".into())
            .expect("name+number não-vazios => Some");
        assert_eq!(c.id, "id1");
        assert_eq!(c.name, "Alice");
        assert_eq!(c.number, "1001");
        assert_eq!(c.notes, "vip");

        // name vazio (ou só espaços) => rejeita.
        assert!(build_contact("   ", "1001", "", "id2".into()).is_none());
        // number vazio => rejeita.
        assert!(build_contact("Bob", "  ", "", "id3".into()).is_none());
        // ambos vazios => rejeita.
        assert!(build_contact("", "", "", "id4".into()).is_none());
    }

    /// Edge-trigger do espelho (prev_mirror_*): só "aplica" quando o valor
    /// mudou; não aplica (nem mexe no current) quando é igual ao último visto.
    #[test]
    fn test_apply_if_changed_edge_trigger() {
        // Começa alinhado: current == prev == incoming => não aplica.
        let mut current = 1u32;
        let mut prev = 1u32;
        assert!(!apply_if_changed(&mut current, &1, &mut prev), "igual => não aplica");
        assert_eq!((current, prev), (1, 1));

        // Daemon mudou o valor (incoming != prev): aplica uma vez e atualiza prev.
        assert!(apply_if_changed(&mut current, &2, &mut prev), "mudou => aplica");
        assert_eq!((current, prev), (2, 2));

        // Mesmo incoming de novo (já visto): não reaplica, mesmo que o usuário
        // tenha editado o `current` no meio-tempo (formulário preservado).
        current = 99; // simula edição local do formulário
        assert!(!apply_if_changed(&mut current, &2, &mut prev), "já visto => não reaplica");
        assert_eq!(current, 99, "não sobrescreve a edição local do usuário");
        assert_eq!(prev, 2);
    }
}

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

#[derive(Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub id: String,
    pub name: String,
    pub number: String,
    pub notes: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallDirection {
    Outgoing,
    Incoming,
    Missed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

        // Decifragem TOLERANTE e ISOLADA NO CAMPO password.
        match crate::crypto::decrypt_password(&cfg.account.password, &dir) {
            crate::crypto::DecryptResult::Ok(pw) => {
                cfg.account.password = pw;
                cfg.password_needs_reentry = false;
            }
            crate::crypto::DecryptResult::Legacy(pw) => {
                // Texto plano legado: carrega e será re-cifrado no próximo save.
                cfg.account.password = pw;
                cfg.password_needs_reentry = false;
            }
            crate::crypto::DecryptResult::Failed => {
                // Não apaga a config: zera só a senha e sinaliza reentrada.
                cfg.account.password = String::new();
                cfg.password_needs_reentry = true;
            }
        }

        cfg
    }

    pub fn save(&self) {
        let path = Self::config_path();
        let dir = Self::config_dir();

        // Cifra a senha SÓ na forma persistida; a cópia em memória (self) fica
        // intacta em claro para o Digest. Serializa uma cópia com a senha
        // cifrada. Senha vazia é gravada como "" (sem cifrar nada).
        let mut to_persist = self.clone();
        if !to_persist.account.password.is_empty() {
            let nonce = crate::crypto::random_nonce();
            to_persist.account.password =
                crate::crypto::encrypt_password(&self.account.password, &dir, nonce);
        }

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

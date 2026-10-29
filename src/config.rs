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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl AppConfig {
    pub fn config_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let dir = PathBuf::from(home).join(".config").join("rustsip");
        let _ = fs::create_dir_all(&dir);
        dir.join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if let Ok(data) = fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str::<AppConfig>(&data) {
                return cfg;
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        let path = Self::config_path();
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, json);
        }
    }
}

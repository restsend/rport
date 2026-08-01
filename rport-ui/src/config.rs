use std::path::PathBuf;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardRule {
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub enabled: bool,
}

impl Default for ForwardRule {
    fn default() -> Self {
        Self {
            local_port: 8080,
            remote_host: "127.0.0.1".to_string(),
            remote_port: 80,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub id: String,
    pub name: String,
    pub server_addr: String,
    pub token: String,
    pub agent_id: String,
    pub forwards: Vec<ForwardRule>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            server_addr: String::new(),
            token: String::new(),
            agent_id: String::new(),
            forwards: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub clients: Vec<ClientConfig>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { clients: vec![] }
    }
}

impl UiConfig {
    pub fn config_path() -> PathBuf {
        let home = home::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.join(".rport-ui.toml")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    if let Ok(config) = toml::from_str(&content) {
                        return config;
                    }
                    tracing::warn!("Failed to parse config, using defaults");
                }
                Err(e) => {
                    tracing::warn!("Failed to read config: {}", e);
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        match toml::to_string_pretty(self) {
            Ok(content) => {
                std::fs::write(&path, content).ok();
            }
            Err(e) => {
                tracing::warn!("Failed to serialize config: {}", e);
            }
        }
    }
}

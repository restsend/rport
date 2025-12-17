use anyhow::{anyhow, Result};
use rustrtc::IceServer;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::cli::Cli;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

impl Default for IceServerConfig {
    fn default() -> Self {
        Self {
            urls: vec!["stun:restsend.com:3478".to_string()],
            username: None,
            credential: None,
        }
    }
}

impl From<IceServerConfig> for IceServer {
    fn from(config: IceServerConfig) -> Self {
        IceServer {
            urls: config.urls,
            username: config.username,
            credential: config.credential,
            ..Default::default()
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RportConfig {
    pub id: Option<String>,
    pub server: Option<String>,
    pub token: Option<String>,
    pub ice_servers: Option<Vec<IceServerConfig>>,
    pub target: Option<String>,
    pub port: Option<u16>,
    pub log_file: Option<String>,
    pub connect_timeout: Option<u32>,
}

impl Default for RportConfig {
    fn default() -> Self {
        Self {
            id: None,
            server: None,
            token: None,
            ice_servers: None,
            target: None,
            port: None,
            log_file: None,
            connect_timeout: Some(30),
        }
    }
}

impl RportConfig {
    pub fn load_from_file(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)?;
        let config: RportConfig = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn load_default() -> Result<Self> {
        let home_dir =
            home::home_dir().ok_or_else(|| anyhow!("Could not determine home directory"))?;
        let config_path = home_dir.join(".rport.toml");
        Self::load_from_file(&config_path)
    }

    pub fn merge_with_cli(&mut self, cli: Cli) {
        if let Some(token) = cli.token {
            self.token = Some(token);
        }
        if let Some(server) = cli.server {
            self.server = Some(server);
        }
        if let Some(id) = cli.id {
            self.id = Some(id);
        }
        if let Some(target) = cli.target {
            self.target = Some(target);
        }
        if let Some(port) = cli.port {
            self.port = Some(port);
        }
        if let Some(log_file) = cli.log_file {
            self.log_file = Some(log_file.to_string_lossy().to_string());
        }
        if let Some(connect_timeout) = cli.timeout {
            self.connect_timeout = Some(connect_timeout);
        }
    }
}

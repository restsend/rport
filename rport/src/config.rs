use anyhow::{anyhow, Result};
use rustrtc::IceServer;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::cli::Cli;
use crate::acl;

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
pub struct ForwardMapping {
    pub local_port: u16,
    pub remote_host: Option<String>,
    pub remote_port: u16,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct DtlsConfig {
    pub listen: Option<String>,
    pub cert: Option<String>,
    pub key: Option<String>,
    pub connect: Option<String>,
    pub server: Option<String>,
    pub allow: Option<String>,
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
    pub upnp: Option<bool>,
    pub dtls: Option<DtlsConfig>,
    pub forward: Option<Vec<ForwardMapping>>,
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
            upnp: Some(true),
            dtls: None,
            forward: None,
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
        self.upnp = Some(cli.upnp);
        // Merge DTLS config
        let dtls = self.dtls.get_or_insert_with(|| DtlsConfig {
            listen: None,
            cert: None,
            key: None,
            connect: None,
            server: None,
            allow: None,
        });
        if let Some(dtls_listen) = cli.dtls_listen {
            dtls.listen = Some(dtls_listen);
        }
        if let Some(dtls_cert) = cli.dtls_cert {
            dtls.cert = Some(dtls_cert.to_string_lossy().to_string());
        }
        if let Some(dtls_key) = cli.dtls_key {
            dtls.key = Some(dtls_key.to_string_lossy().to_string());
        }
        if let Some(connect) = cli.connect {
            dtls.connect = Some(connect);
        }
        if let Some(dtls_server) = cli.dtls_server {
            dtls.server = Some(dtls_server);
        }
        if let Some(allow) = cli.allow {
            dtls.allow = Some(allow);
        }
        // Merge -L forward mappings
        if !cli.forward.is_empty() {
            let forwards: Vec<ForwardMapping> = cli
                .forward
                .iter()
                .filter_map(|f| parse_local_spec(f).ok())
                .collect();
            if !forwards.is_empty() {
                self.forward = Some(forwards);
            }
        }
        // If --remote is given, use it as the single forward target (legacy)
        if let Some(remote) = cli.remote {
            if let Ok((host, port)) = crate::parse_target(&remote) {
                let fwds = self.forward.get_or_insert_with(Vec::new);
                // Only add if no forwards already specified
                if fwds.is_empty() {
                    fwds.push(ForwardMapping {
                        local_port: cli.port.unwrap_or(0),
                        remote_host: Some(host),
                        remote_port: port,
                    });
                }
            }
        }
    }

    pub fn parse_acl(&self) -> Option<acl::Acl> {
        let dtls = self.dtls.as_ref()?;
        let allow_spec = dtls.allow.as_ref()?;
        acl::parse_acl_spec(allow_spec).ok()
    }
}

fn parse_local_spec(spec: &str) -> Result<ForwardMapping> {
    let parts: Vec<&str> = spec.split(':').collect();
    match parts.len() {
        2 => {
            // -L local_port:remote_port  (target = 127.0.0.1)
            let local_port: u16 = parts[0].parse()?;
            let remote_port: u16 = parts[1].parse()?;
            Ok(ForwardMapping {
                local_port,
                remote_host: None,
                remote_port,
            })
        }
        3 => {
            // -L local_port:remote_host:remote_port
            let local_port: u16 = parts[0].parse()?;
            let remote_host = Some(parts[1].to_string());
            let remote_port: u16 = parts[2].parse()?;
            Ok(ForwardMapping {
                local_port,
                remote_host,
                remote_port,
            })
        }
        _ => Err(anyhow!("Invalid -L format: {}. Use local_port:remote_host:remote_port or local_port:remote_port", spec)),
    }
}

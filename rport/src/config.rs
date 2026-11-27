use anyhow::{anyhow, Result};
use rustrtc::IceServer;
use serde::{Deserialize, Serialize};

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

/// Transport protocol for a forward mapping (TCP default, UDP opt-in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    #[serde(rename = "tcp")]
    Tcp,
    #[serde(rename = "udp")]
    Udp,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        }
    }

    /// Parse a `/tcp` or `/udp` spec suffix. An empty/missing suffix is `None`.
    fn from_suffix(suffix: &str) -> Result<Option<Self>> {
        match suffix.to_ascii_lowercase().as_str() {
            "" => Ok(None),
            "tcp" => Ok(Some(Protocol::Tcp)),
            "udp" => Ok(Some(Protocol::Udp)),
            other => Err(anyhow!("Unknown protocol '/{}' (use tcp or udp)", other)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ForwardMapping {
    pub local_port: Option<u16>,
    pub host: String,
    pub port: u16,
    pub proto: Protocol,
}

impl ForwardMapping {
    pub fn is_proxy_command(&self) -> bool {
        self.local_port.is_none()
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RportConfig {
    pub server: Option<String>,
    pub token: Option<String>,
    pub ice_servers: Option<Vec<IceServerConfig>>,
    pub log_file: Option<String>,
    pub connect_timeout: Option<u32>,
    pub upnp: Option<bool>,
    // Advanced WebRTC / SCTP tuning
    pub ice_disconnect_threshold: Option<u64>,
    pub ice_disconnect_grace: Option<u64>,
    pub ice_connection_timeout: Option<u64>,
    pub sctp_rto_initial_ms: Option<u64>,
    pub sctp_rto_min_ms: Option<u64>,
    pub sctp_rto_max_sec: Option<u64>,
    pub sctp_max_association_retransmits: Option<u32>,
    pub sctp_receive_window_kb: Option<usize>,
}

impl Default for RportConfig {
    fn default() -> Self {
        Self {
            server: None,
            token: None,
            ice_servers: None,
            log_file: None,
            connect_timeout: Some(30),
            upnp: Some(true),
            ice_disconnect_threshold: None,
            ice_disconnect_grace: None,
            ice_connection_timeout: None,
            sctp_rto_initial_ms: None,
            sctp_rto_min_ms: None,
            sctp_rto_max_sec: None,
            sctp_max_association_retransmits: None,
            sctp_receive_window_kb: None,
        }
    }
}

impl RportConfig {
    pub fn load_from_file(path: &std::path::PathBuf) -> Result<Self> {
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

    pub fn merge_with_cli(&mut self, cli: Cli) -> Vec<ForwardMapping> {
        if let Some(token) = cli.token {
            self.token = Some(token);
        }
        if let Some(server) = cli.server {
            self.server = Some(server);
        }
        if let Some(connect_timeout) = cli.timeout {
            self.connect_timeout = Some(connect_timeout);
        }
        if let Some(log_file) = cli.log_file {
            self.log_file = Some(log_file.to_string_lossy().to_string());
        }
        self.upnp = Some(cli.upnp);

        // Parse -L forward specs (TCP unless suffixed /udp)
        let mut forwards: Vec<ForwardMapping> = cli
            .forward
            .iter()
            .filter_map(|f| parse_forward_spec(f).ok())
            .collect();

        // Parse -U forward specs (UDP unless suffixed /tcp)
        let udp_forwards: Vec<ForwardMapping> = cli
            .forward_udp
            .iter()
            .filter_map(|f| parse_forward_spec_with_default(f, Protocol::Udp).ok())
            .collect();
        forwards.extend(udp_forwards);

        forwards
    }
}

/// Parse a forward spec: `local_port:host:port`, `local_port:port`, `port`,
/// or `host:port` (ProxyCommand). An optional `/tcp` or `/udp` suffix selects
/// the transport protocol; `default_proto` applies when the suffix is absent.
pub fn parse_forward_spec_with_default(spec: &str, default_proto: Protocol) -> Result<ForwardMapping> {
    // Split off the optional protocol suffix: ".../udp" / ".../tcp"
    let (body, proto) = match spec.rsplit_once('/') {
        // A single bare "port" spec with a suffix ("-L 53/udp") has no '/'
        // beyond the protocol marker, so rsplit_once is safe here.
        Some((body, suffix)) => (body, Protocol::from_suffix(suffix)?),
        None => (spec, None),
    };
    let proto = proto.unwrap_or(default_proto);

    let parts: Vec<&str> = body.split(':').collect();
    match parts.len() {
        1 => {
            // -L port  →  shorthand for -L 127.0.0.1:port (ProxyCommand)
            let port: u16 = parts[0].parse()?;
            Ok(ForwardMapping {
                local_port: None,
                host: "127.0.0.1".to_string(),
                port,
                proto,
            })
        }
        2 => {
            // -L local_port:remote_port  (port forward, host=127.0.0.1)
            // or -L host:port (ProxyCommand)
            if let Ok(local_port) = parts[0].parse::<u16>() {
                let remote_port: u16 = parts[1].parse()?;
                Ok(ForwardMapping {
                    local_port: Some(local_port),
                    host: "127.0.0.1".to_string(),
                    port: remote_port,
                    proto,
                })
            } else {
                // ProxyCommand: host:port
                let remote_port: u16 = parts[1].parse()?;
                Ok(ForwardMapping {
                    local_port: None,
                    host: parts[0].to_string(),
                    port: remote_port,
                    proto,
                })
            }
        }
        3 => {
            // -L local_port:host:port
            let local_port: u16 = parts[0].parse()?;
            let host = parts[1].to_string();
            let remote_port: u16 = parts[2].parse()?;
            Ok(ForwardMapping {
                local_port: Some(local_port),
                host,
                port: remote_port,
                proto,
            })
        }
        _ => Err(anyhow!(
            "Invalid -L format: {}. Use port, host:port, local_port:port, or local_port:host:port",
            spec
        )),
    }
}

fn parse_forward_spec(spec: &str) -> Result<ForwardMapping> {
    parse_forward_spec_with_default(spec, Protocol::Tcp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tcp_specs_default() {
        let f = parse_forward_spec("8080:127.0.0.1:80").unwrap();
        assert_eq!(f.local_port, Some(8080));
        assert_eq!(f.host, "127.0.0.1");
        assert_eq!(f.port, 80);
        assert_eq!(f.proto, Protocol::Tcp);
    }

    #[test]
    fn test_parse_udp_suffix() {
        let f = parse_forward_spec("5353:127.0.0.1:53/udp").unwrap();
        assert_eq!(f.local_port, Some(5353));
        assert_eq!(f.port, 53);
        assert_eq!(f.proto, Protocol::Udp);
    }

    #[test]
    fn test_parse_udp_two_part() {
        let f = parse_forward_spec("5353:53/udp").unwrap();
        assert_eq!(f.local_port, Some(5353));
        assert_eq!(f.host, "127.0.0.1");
        assert_eq!(f.port, 53);
        assert_eq!(f.proto, Protocol::Udp);
    }

    #[test]
    fn test_parse_explicit_tcp_suffix_wins() {
        // -U with an explicit /tcp suffix overrides the UDP default
        let f = parse_forward_spec_with_default("8080:10.0.0.1:80/tcp", Protocol::Udp).unwrap();
        assert_eq!(f.proto, Protocol::Tcp);
    }

    #[test]
    fn test_parse_udp_default_flag() {
        // -U specs default to UDP
        let f = parse_forward_spec_with_default("5353:8.8.8.8:53", Protocol::Udp).unwrap();
        assert_eq!(f.proto, Protocol::Udp);
        assert_eq!(f.port, 53);
    }

    #[test]
    fn test_parse_invalid_protocol_suffix() {
        assert!(parse_forward_spec("5353:127.0.0.1:53/sctp").is_err());
    }

    #[test]
    fn test_protocol_serde_roundtrip() {
        // Target.proto strings and config serialization agree
        assert_eq!(serde_json::to_value(Protocol::Udp).unwrap(), "udp");
        assert_eq!(serde_json::to_value(Protocol::Tcp).unwrap(), "tcp");
        let p: Protocol = serde_json::from_value(serde_json::json!("udp")).unwrap();
        assert_eq!(p, Protocol::Udp);
    }
}

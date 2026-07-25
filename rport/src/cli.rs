use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Clone)]
#[command(name = "rport")]
#[command(about = "Remote port forwarding client and agent")]
#[command(version = env!("CARGO_PKG_VERSION"))]
pub struct Cli {
    /// Configuration file path
    #[arg(short = 'f', long = "conf")]
    pub config: Option<PathBuf>,

    //=== HTTP/SSE signaling server mode ===
    /// Server URL (HTTP signaling server, e.g. https://rport.example.com)
    #[arg(short, long)]
    pub server: Option<String>,
    /// Authentication token
    #[arg(short = 'k', long)]
    pub token: Option<String>,
    /// Agent ID (required for ProxyCommand and port forwarding modes)
    #[arg(short, long)]
    pub id: Option<String>,

    //=== Agent mode ===
    /// Target address for agent mode (e.g., 127.0.0.1:22 or just 22) — fixed target
    #[arg(short = 't', long)]
    pub target: Option<String>,

    //=== DTLS signaling mode ===
    /// DTLS listen address for agent mode (e.g. 0.0.0.0:4443)
    #[arg(long = "dtls-listen")]
    pub dtls_listen: Option<String>,
    /// DTLS certificate path (self-signed if omitted)
    #[arg(long = "dtls-cert")]
    pub dtls_cert: Option<PathBuf>,
    /// DTLS private key path
    #[arg(long = "dtls-key")]
    pub dtls_key: Option<PathBuf>,
    /// Agent DTLS connect address (client mode, e.g. agent:4443)
    #[arg(long = "connect")]
    pub connect: Option<String>,
    /// DTLS server address for agent to connect to (replaces HTTP/SSE, e.g. rport.example.com:8443)
    #[arg(long = "dtls-server")]
    pub dtls_server: Option<String>,
    /// Remote target for client mode (e.g. 192.168.1.5:22 or :22)
    #[arg(long = "remote")]
    pub remote: Option<String>,

    //=== Client port forwarding mode ===
    /// Local port for CLI port forwarding mode (single port, legacy)
    #[arg(short, long)]
    pub port: Option<u16>,
    /// Local port forwarding rules: -L local_port:remote_host:remote_port or -L local_port:remote_port
    #[arg(short = 'L', long = "forward", value_name = "LOCAL:REMOTE")]
    pub forward: Vec<String>,

    //=== Access control ===
    /// Agent access control whitelist: "network/mask:ports;..." e.g. "127.0.0.0/8:22,80;10.0.0.0/8:3000-4000"
    #[arg(long = "allow")]
    pub allow: Option<String>,

    /// Skip known-hosts fingerprint check (automatically set for ProxyCommand)
    #[arg(long = "no-known-hosts-check")]
    pub no_known_hosts_check: bool,

    /// Run as daemon (detach from terminal)
    #[arg(short = 'd', long)]
    pub daemon: bool,
    /// Log file path for daemon mode
    #[arg(long = "log-file")]
    pub log_file: Option<PathBuf>,
    /// ProxyCommand arguments: hostname and port (for SSH ProxyCommand usage)
    #[arg(value_name = "HOST")]
    pub proxy_args: Vec<String>,

    /// Connection timeout in seconds
    #[arg(long = "timeout")]
    pub timeout: Option<u32>,

    #[arg(long = "debug", default_value_t = false)]
    pub debug: bool,

    #[arg(long = "upnp", default_value_t = false)]
    pub upnp: bool,
}

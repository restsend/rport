use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Clone)]
#[command(name = "rport")]
#[command(about = "Remote port forwarding over DTLS signaling + WebRTC")]
#[command(version = env!("CARGO_PKG_VERSION"))]
pub struct Cli {
    /// Configuration file path
    #[arg(short = 'f', long = "conf")]
    pub config: Option<PathBuf>,

    /// DTLS signaling server address (e.g. rport.example.com:8443)
    #[arg(short, long)]
    pub server: Option<String>,
    /// Authentication token
    #[arg(short = 'k', long)]
    pub token: Option<String>,
    /// Agent/client identifier
    #[arg(short, long)]
    pub id: Option<String>,

    //=== Client port forwarding mode ===
    /// Forward spec: local_port:host:port (port forward) or host:port (ProxyCommand pipe mode)
    #[arg(short = 'L', value_name = "SPEC")]
    pub forward: Vec<String>,

    //=== Agent access control mode ===
    /// Access control rule (repeatable): port, port-port, or host:port-port
    /// e.g. -A 22 -A 3000-4000 -A 127.0.0.1:22
    #[arg(short = 'A', long = "allow", value_name = "RULE")]
    pub allow: Vec<String>,

    /// Connection timeout in seconds
    #[arg(long = "timeout")]
    pub timeout: Option<u32>,

    #[arg(long = "debug", default_value_t = false)]
    pub debug: bool,

    #[arg(long = "upnp", default_value_t = false)]
    pub upnp: bool,

    /// Wait for all ICE candidates before sending offer (needed for old HTTP/SSE agents)
    #[arg(long = "wait-candidates")]
    pub wait_candidates: bool,

    /// Daemonize the process (Unix only)
    #[arg(short = 'd', long)]
    pub daemon: bool,

    /// Log file path (used with --daemon or config)
    #[arg(long = "log-file")]
    pub log_file: Option<PathBuf>,
}

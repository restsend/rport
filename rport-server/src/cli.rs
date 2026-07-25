use clap::Parser;

#[derive(Parser)]
#[command(name = "rport-server")]
#[command(about = "Remote port forwarding server")]
pub struct ServerCli {
    /// HTTP server bind address
    #[arg(short, long, default_value = "0.0.0.0:3000")]
    pub addr: String,

    /// DTLS server bind address (e.g. 0.0.0.0:8443)
    #[arg(long = "dtls-addr")]
    pub dtls_addr: Option<String>,

    /// DTLS certificate path
    #[arg(long = "dtls-cert")]
    pub dtls_cert: Option<String>,

    /// DTLS private key path
    #[arg(long = "dtls-key")]
    pub dtls_key: Option<String>,

    /// Disable TURN server
    #[arg(long, default_value_t = false)]
    pub disable_turn: bool,

    #[arg(short, long, default_value = "0.0.0.0:13478")]
    pub turn_addr: Option<String>,

    /// Public IP address for TURN server
    #[arg(long)]
    pub public_ip: Option<String>,

    /// Enable debug logging
    #[arg(long, default_value_t = false)]
    pub debug: bool,
}

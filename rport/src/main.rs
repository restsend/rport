use clap::Parser;
pub mod acl;
mod agent;
mod cli;
mod client;
mod config;
#[cfg(unix)]
mod daemon;
mod dtls_signaling;
mod known_hosts;
mod webrtc_config;

use agent::Agent;
use cli::Cli;
use client::CliClient;
use config::RportConfig;
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfferMessage {
    pub id: String,
    pub offer: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AnswerMessage {
    pub answer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ServerMessage {
    pub message_type: String,
    pub data: serde_json::Value,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Handle daemon mode
    if cli.daemon {
        #[cfg(unix)]
        {
            let log_file = cli.log_file.as_ref().map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| {
                    if cli.target.is_some() { "/tmp/rport-agent.log".to_string() }
                    else if cli.forward.is_empty() && cli.port.is_some() { "/tmp/rport-forward.log".to_string() }
                    else if !cli.proxy_args.is_empty() { "/tmp/rport-proxy.log".to_string() }
                    else { "/tmp/rport.log".to_string() }
                });
            daemon::daemonize_with_log(&log_file)?;
        }
        #[cfg(not(unix))]
        { return Err(anyhow::anyhow!("Daemon mode is only supported on Unix systems")); }
    }

    tokio::runtime::Runtime::new()?.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> anyhow::Result<()> {
    let mut config = if let Some(config_path) = &cli.config {
        RportConfig::load_from_file(config_path)?
    } else {
        RportConfig::load_default()?
    };

    let log_env = if cli.debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::from_default_env()
    };

    let proxy_args = cli.proxy_args.clone();
    config.merge_with_cli(cli);

    let server = config.server.clone();
    let token = config.token.clone();
    let dtls_connect = config.dtls.as_ref().and_then(|d| d.connect.clone());
    let dtls_listen = config.dtls.as_ref().and_then(|d| d.listen.clone());
    let dtls_server = config.dtls.as_ref().and_then(|d| d.server.clone());

    // Determine mode
    let is_dtls_agent_listen = dtls_listen.is_some();
    let is_dtls_agent_server = dtls_server.is_some();
    let is_dtls_client = dtls_connect.is_some() && server.is_none();
    let is_http_agent = config.target.is_some() && server.is_some();
    let has_any_target = config.port.is_some() || config.forward.is_some() || !proxy_args.is_empty();

    if !is_dtls_agent_listen && !is_dtls_agent_server && !is_dtls_client && !is_http_agent && !has_any_target {
        anyhow::bail!("No mode specified. Use --target for agent, --port/--forward for client, --dtls-listen for DTLS agent, --dtls-server for DTLS agent via server, or --connect for DTLS client");
    }

    // Agent: DTLS connect to server mode
    if is_dtls_agent_server {
        tracing_subscriber::fmt().with_env_filter(log_env).init();
        let svr = dtls_server.unwrap();
        let (target_host, target_port) = config.target.as_ref()
            .map(|t| parse_target(t).unwrap_or(("127.0.0.1".to_string(), 22)))
            .unwrap_or(("127.0.0.1".to_string(), 22));
        let acl = config.parse_acl();
        let agent = Agent::new(
            Some(svr), token.clone(), config.id.clone(),
            target_host, target_port,
            config.ice_servers.clone(), config.upnp.unwrap_or(false),
            None, None, acl,
        );
        agent.run_via_dtls_server().await?;
        return Ok(());
    }

    // Agent: DTLS listen mode (direct)
    if is_dtls_agent_listen {
        tracing_subscriber::fmt().with_env_filter(log_env).init();
        let listen_addr = dtls_listen.unwrap_or_else(|| "0.0.0.0:4443".to_string());
        let acl = config.parse_acl();
        let (target_host, target_port) = config.target.as_ref()
            .map(|t| parse_target(t).unwrap_or(("127.0.0.1".to_string(), 22)))
            .unwrap_or(("127.0.0.1".to_string(), 22));

        let agent = Agent::new(
            None, None, config.id.clone(),
            target_host, target_port,
            config.ice_servers.clone(), config.upnp.unwrap_or(false),
            Some(listen_addr), None, acl,
        );
        agent.run_dtls().await?;
        return Ok(());
    }

    // Client: DTLS mode (direct, no server)
    if is_dtls_client {
        tracing_subscriber::fmt().with_env_filter(log_env).init();
        let connect_addr = dtls_connect.unwrap();
        let is_proxy_command = !proxy_args.is_empty();
        let no_kh_check = config.id.as_ref().map_or(false, |_| false) || is_proxy_command;

        let client = CliClient::new(
            None, None,
            config.ice_servers.clone(), config.upnp.unwrap_or(false),
            Some(connect_addr), no_kh_check,
        );

        let forwards = config.forward.clone().unwrap_or_default();
        if forwards.is_empty() && config.port.is_some() {
            // Single port backward compat
            let agent_id = config.id.clone()
                .unwrap_or_else(|| "default".to_string());
            // Create a WebRTC connection per incoming connection (as before)
            client.connect_port_forward(agent_id, config.port.unwrap()).await?;
        } else if !forwards.is_empty() {
            client.connect_via_dtls(&forwards).await?;
        } else if is_proxy_command {
            let agent_id = config.id.clone().unwrap_or_else(|| "default".to_string());
            client.connect_proxy_command(config.connect_timeout, agent_id).await?;
        } else {
            anyhow::bail!("No forward targets specified. Use --forward (-L) or --port");
        }
        return Ok(());
    }

    // Agent: HTTP/SSE mode
    if is_http_agent {
        tracing_subscriber::fmt().with_env_filter(log_env).init();
        let (host, port) = parse_target(config.target.as_ref().unwrap())?;
        let agent_id = config.id.clone()
            .unwrap_or_else(|| format!("agent-{}", std::process::id()));
        let agent = Agent::new(
            server, token.clone(), Some(agent_id),
            host, port,
            config.ice_servers.clone(), config.upnp.unwrap_or(false),
            None, None, None,
        );
        agent.run().await?;
        return Ok(());
    }

    // Client: HTTP/SSE mode (port forward)
    if let Some(local_port) = config.port {
        tracing_subscriber::fmt().with_env_filter(log_env).init();
        let agent_id = config.id.clone().ok_or_else(|| {
            anyhow::anyhow!("Agent ID is required for port forwarding mode. Use --id <AGENT_ID>")
        })?;
        let client = CliClient::new(
            server, token.clone(),
            config.ice_servers.clone(), config.upnp.unwrap_or(false),
            None, true,
        );
        client.connect_port_forward(agent_id, local_port).await?;
        return Ok(());
    }

    // Client: HTTP/SSE mode (ProxyCommand) - default if nothing else matches
    {
        if let Some(log_file) = config.log_file.clone() {
            let file = std::fs::OpenOptions::new()
                .create(true).append(true).open(log_file)?;
            tracing_subscriber::fmt()
                .with_env_filter(log_env)
                .with_writer(file)
                .init();
        } else {
            tracing_subscriber::fmt()
                .with_env_filter(log_env)
                .with_writer(std::io::stderr)
                .init();
        }
        let agent_id = config.id.clone()
            .ok_or_else(|| anyhow::anyhow!("Agent ID is required for ProxyCommand mode. Use --id <AGENT_ID>"))?;
        let client = CliClient::new(
            server, token,
            config.ice_servers.clone(), config.upnp.unwrap_or(false),
            None, true,
        );
        client.connect_proxy_command(config.connect_timeout.clone(), agent_id).await?;
    }

    Ok(())
}

pub fn parse_target(target: &str) -> anyhow::Result<(String, u16)> {
    if let Some(colon_pos) = target.rfind(':') {
        let host = target[..colon_pos].to_string();
        let port: u16 = target[colon_pos + 1..].parse()?;
        Ok((host, port))
    } else {
        let port: u16 = target.parse()?;
        Ok(("127.0.0.1".to_string(), port))
    }
}

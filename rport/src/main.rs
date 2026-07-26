use std::path::PathBuf;

use clap::Parser;
pub mod acl;
mod agent;
mod cli;
mod client;
mod config;
mod daemon;
mod dtls_signaling;
mod known_hosts;
mod webrtc_config;

#[cfg(test)]
mod e2e_test;

use agent::Agent;
use cli::Cli;
use client::CliClient;
use config::RportConfig;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.daemon {
        let log_file = cli.log_file.clone().unwrap_or_else(|| PathBuf::from("rport.log"));
        daemon::daemonize_with_log(&log_file)?;
    }

    tokio::runtime::Runtime::new()?.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> anyhow::Result<()> {
    let mut config = if let Some(config_path) = &cli.config {
        RportConfig::load_from_file(config_path)?
    } else {
        RportConfig::load_default()?
    };

    let is_debug = cli.debug;
    let cli_allow = cli.allow.clone();
    let cli_id = cli.id.clone();
    let wait_candidates = cli.wait_candidates;

    let log_env = if is_debug {
        EnvFilter::new("debug,hyper=warn,gather=warn,igd=warn,neli=warn,rustls_platform_verifier=warn")
    } else {
        EnvFilter::from_default_env()
    };

    let forwards = config.merge_with_cli(cli);

    let server = config.server.clone();
    let token = config.token.clone();
    let ice_servers = config.ice_servers.clone();
    let upnp = config.upnp.unwrap_or(false);

    // Initialize tracing — both agent and client modes share the same logic
    if let Some(log_file) = config.log_file.as_ref() {
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

    let is_agent_mode = !cli_allow.is_empty();
    let is_client_mode = !forwards.is_empty();

    if !is_agent_mode && !is_client_mode {
        anyhow::bail!(
            "No mode specified.\n\
             Agent: use -A/--allow for access control rules\n\
             Client: use -L for forwarding specs"
        );
    }

    if is_agent_mode && is_client_mode {
        anyhow::bail!("Cannot specify both -A (agent) and -L (client)");
    }

    let srv = server.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Server address required. Use -s/--server or set 'server' in config")
    })?;
    let tok = token.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Token required. Use -k/--token or set 'token' in config")
    })?;

    if is_agent_mode {
        let agent_id = cli_id.as_deref().unwrap_or("default");
        let acl = crate::acl::parse_allow_rules(&cli_allow)
            .map_err(|e| anyhow::anyhow!("Invalid --allow rule: {}", e))?;
        let agent = Agent::new(srv, tok, agent_id, ice_servers, upnp, acl, &config);
        agent.run().await?;
    } else {
        // Client mode
        let agent_id = cli_id.as_deref().ok_or_else(|| {
            anyhow::anyhow!("Agent ID required for client mode. Use -i/--id")
        })?;

        let is_proxy_command = forwards.iter().any(|f| f.is_proxy_command());
        if forwards.len() > 1 && is_proxy_command {
            anyhow::bail!("ProxyCommand mode (-L host:port) cannot be combined with other -L specs");
        }

        let client = CliClient::new(srv, tok, agent_id, ice_servers, upnp, wait_candidates, &config);
        if is_proxy_command {
            let fwd = forwards.into_iter().next().unwrap();
            client.connect_proxy_command(config.connect_timeout, &fwd.host, fwd.port).await?;
        } else {
            client.connect_port_forwards(config.connect_timeout, &forwards).await?;
        }
    }

    Ok(())
}

use anyhow::Result;
use clap::Parser;
use cli::ServerCli;
use dtls_handler::load_certificate;
use rport_server::{handler::create_router_with_state, AppState, TurnServer};
use std::{
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::Arc,
};
use tracing::info;
use tracing_subscriber::{self, filter::EnvFilter};
mod cli;
mod dtls_handler;

pub fn get_first_non_loopback_interface() -> Result<IpAddr> {
    for i in get_if_addrs::get_if_addrs()? {
        if !i.is_loopback() {
            match i.addr {
                get_if_addrs::IfAddr::V4(ref addr) => return Ok(std::net::IpAddr::V4(addr.ip)),
                _ => continue,
            }
        }
    }
    Err(anyhow::anyhow!("No IPV4 interface found"))
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = ServerCli::parse();

    let log_filter = if cli.debug {
        EnvFilter::new("debug,hyper=warn,gather=warn,igd=warn,neli=warn,rustls_platform_verifier=warn")
    } else {
        EnvFilter::from_default_env()
    };
    tracing_subscriber::fmt()
        .with_env_filter(log_filter)
        .init();

    // Start TURN server
    let addr_parts: Vec<&str> = cli.addr.split(':').collect();
    let ip: IpAddr = if let Ok(parsed_ip) = addr_parts.get(0).unwrap().parse::<IpAddr>() {
        if parsed_ip.is_unspecified() {
            get_first_non_loopback_interface()?
        } else {
            parsed_ip
        }
    } else {
        get_first_non_loopback_interface()?
    };

    let public_ip = cli.public_ip.clone();
    let turn_addr = cli
        .turn_addr
        .unwrap_or_else(|| format!("{}:13478", ip))
        .parse::<SocketAddr>()?;
    let turn_server = Arc::new(TurnServer::new(cli.disable_turn, turn_addr, public_ip).await?);
    turn_server.start().await.ok();

    // Create shared AppState
    let app_state = AppState::new_with_turn(turn_server);

    // Start DTLS signaling server (if configured)
    if let Some(dtls_addr) = &cli.dtls_addr {
        info!("Starting DTLS signaling server on {}", dtls_addr);
        let cert = match (&cli.dtls_cert, &cli.dtls_key) {
            (Some(cert_path), Some(key_path)) => {
                Some(load_certificate(Path::new(cert_path), Path::new(key_path))?)
            }
            _ => None,
        };
        let _dtls_handle = dtls_handler::DtlsHandler::listen(dtls_addr.clone(), cert, app_state.clone()).await?;
    }

    // Start HTTP server
    let app = create_router_with_state(app_state);
    let listener = tokio::net::TcpListener::bind(&cli.addr).await?;
    println!(
        "Server running on http://{}:{}",
        ip,
        addr_parts.get(1).unwrap_or(&"3000")
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .ok();

    Ok(())
}

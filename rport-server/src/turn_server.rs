use anyhow::Result;
use serde::Serialize;
use std::sync::RwLock;
use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tracing::info;
use turn::{
    relay::relay_static::RelayAddressGeneratorStatic,
    server::{config::*, Server},
};

#[derive(Clone, Serialize)]
pub struct TurnCredentials {
    pub username: String,
    pub password: String,
}
/// Simple TURN server with temporary authentication
pub struct TurnServer {
    pub auth_duration: Duration,
    pub disabled: bool,
    listen_addr: SocketAddr,
    public_ip: Option<String>,
    server: Arc<RwLock<Option<Server>>>,
    secret: String,
}

impl TurnServer {
    pub async fn new(
        disabled: bool,
        listen_addr: SocketAddr,
        public_ip: Option<String>,
    ) -> Result<Self> {
        let secret = format!("{}", rand::random::<u64>());
        Ok(Self {
            auth_duration: Duration::from_secs(3600),
            disabled,
            listen_addr,
            public_ip,
            server: Arc::new(RwLock::new(None)),
            secret,
        })
    }

    /// Start the TURN server using the turn crate
    pub async fn start(&self) -> Result<()> {
        if self.disabled {
            info!("TURN server is disabled.");
            return Ok(());
        }

        info!(
            "TURN server listening on {}, public IP: {}",
            self.listen_addr,
            self.public_ip.as_deref().unwrap_or("<not set>")
        );

        // Create UDP socket connection
        let socket = tokio::net::UdpSocket::bind(self.listen_addr).await?;
        let conn = Arc::new(socket);

        // Determine public IP for relay address generator
        let relay_ip = if let Some(ref public_ip) = self.public_ip {
            IpAddr::from_str(public_ip)?
        } else {
            IpAddr::from_str("127.0.0.1")?
        };

        // Create relay address generator
        let relay_addr_generator = Box::new(RelayAddressGeneratorStatic {
            relay_address: relay_ip,
            address: "0.0.0.0".to_owned(),
            net: Arc::new(webrtc_util::vnet::net::Net::new(None)),
        });
        let auth_handler = turn::auth::LongTermAuthHandler::new(self.secret.clone());
        // Create server configuration
        let config = ServerConfig {
            conn_configs: vec![ConnConfig {
                conn,
                relay_addr_generator,
            }],
            realm: "rport.turn".to_owned(),
            auth_handler: Arc::new(auth_handler),
            channel_bind_timeout: Duration::from_secs(600), // 1 minutes
            alloc_close_notify: None,
        };

        // Create and start the TURN server
        let server = Server::new(config).await?;
        *self.server.write().unwrap() = Some(server);

        info!("TURN server started successfully");
        Ok(())
    }

    /// Generate temporary credentials valid for 1 minute
    pub async fn generate_credentials(&self) -> Option<TurnCredentials> {
        if self.disabled {
            return None;
        }

        let cred =
            turn::auth::generate_long_term_credentials(self.secret.as_str(), self.auth_duration)
                .ok()?;

        let credentials = TurnCredentials {
            username: cred.0.clone(),
            password: cred.1.clone(),
        };

        Some(credentials)
    }

    /// Get the TURN server address
    pub fn get_turn_url(&self) -> String {
        let port = self.listen_addr.port();
        match &self.public_ip {
            Some(ip) => format!("turn:{}:{}", ip, port),
            None => format!("turn:{}", self.listen_addr),
        }
    }
    pub fn get_stun_url(&self) -> String {
        let port = self.listen_addr.port();
        match &self.public_ip {
            Some(ip) => format!("stun:{}:{}", ip, port),
            None => format!("stun:{}", self.listen_addr),
        }
    }

    /// Close the TURN server
    pub async fn close(&self) -> Result<()> {
        let mut server_guard = self.server.write().unwrap();
        if let Some(server) = server_guard.take() {
            server
                .close()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to close TURN server: {}", e))?;
        }
        Ok(())
    }
}

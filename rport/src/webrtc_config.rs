use anyhow::Result;
use reqwest::Client;
use rustrtc::{IceServer, PeerConnection, RtcConfiguration};
use std::{sync::Arc, time::Duration};

use crate::config::IceServerConfig;

#[derive(Clone)]
pub struct WebRTCConfig {
    pub server: String,
    pub token: String,
    pub ice_servers: Vec<IceServerConfig>,
    pub enable_upnp: bool,
}

impl WebRTCConfig {
    pub fn new(
        server: String,
        token: String,
        ice_servers: Vec<IceServerConfig>,
        enable_upnp: bool,
    ) -> Self {
        Self {
            server,
            token,
            ice_servers,
            enable_upnp,
        }
    }

    pub async fn get_ice_servers(&self) -> Vec<IceServer> {
        if self.ice_servers.len() > 0 {
            return self
                .ice_servers
                .clone()
                .into_iter()
                .map(|c| c.into())
                .collect();
        }
        let url = format!("{}/rport/iceservers?token={}", self.server, self.token);
        let response = match Client::new().get(&url).send().await {
            Ok(resp) => resp,
            Err(_) => {
                return vec![IceServerConfig::default().into()];
            }
        };

        if !response.status().is_success() {
            return vec![IceServerConfig::default().into()];
        }
        let ice_servers: Vec<IceServer> = response
            .json::<Vec<IceServerConfig>>()
            .await
            .map(|configs| configs.into_iter().map(|c| c.into()).collect())
            .unwrap_or_default();
        ice_servers
    }

    pub async fn create_peer_connection(&self) -> Result<Arc<PeerConnection>> {
        let ice_servers = self.get_ice_servers().await;
        let config = RtcConfiguration {
            ice_servers,
            ice_connection_timeout: Duration::from_secs(120),
            sctp_rto_initial: Duration::from_millis(1000),
            sctp_rto_min: Duration::from_millis(500),
            sctp_rto_max: Duration::from_secs(60),
            sctp_max_association_retransmits: 30,
            sctp_heartbeat_interval: Duration::from_secs(30),
            sctp_max_heartbeat_failures: 10,
            sctp_max_tsn_retransmits: 30,
            sctp_receive_window: 2 * 1024 * 1024,
            enable_upnp: self.enable_upnp,
            ..Default::default()
        };
        let peer_connection = Arc::new(PeerConnection::new(config));
        Ok(peer_connection)
    }
}

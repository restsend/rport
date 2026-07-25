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
            sctp_rto_initial: Duration::from_millis(400),
            sctp_rto_min: Duration::from_millis(200),
            sctp_rto_max: Duration::from_secs(30),
            sctp_max_association_retransmits: 20,
            sctp_receive_window: 2 * 1024 * 1024,
            // Tunnel robustness on lossy links:
            // - SCTP fixes in rustrtc 0.3.98 (no abandon of reliable chunks,
            //   burst retransmit, RTO snap-back, cwnd halve-not-collapse) make
            //   loss recovery fast and never deadlock.
            // - ICE stays Connected through transient jitter; a sustained loss
            //   surfaces as PeerConnectionState::Disconnected (rustrtc keeps
            //   SCTP alive). rport's grace timer then waits briefly for the
            //   path to recover before giving up, instead of hanging for the
            //   full ice_connection_timeout.
            ice_connection_timeout: Duration::from_secs(300),
            ice_disconnect_threshold: Duration::from_secs(30),
            enable_upnp: self.enable_upnp,
            prefer_srflx_over_natted_host: true,
            ..Default::default()
        };
        let peer_connection = Arc::new(PeerConnection::new(config));
        Ok(peer_connection)
    }
}

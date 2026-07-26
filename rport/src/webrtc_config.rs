use anyhow::Result;
use rustrtc::{IceServer, PeerConnection, RtcConfiguration};
use std::{sync::Arc, time::Duration};

use crate::config::IceServerConfig;

use crate::config::RportConfig;

#[derive(Clone)]
pub struct WebRTCConfig {
    pub ice_servers: Vec<IceServerConfig>,
    pub enable_upnp: bool,
    pub ice_disconnect_threshold: Option<Duration>,
    pub ice_disconnect_grace: Option<Duration>,
    pub ice_connection_timeout: Option<Duration>,
    pub sctp_rto_initial: Option<Duration>,
    pub sctp_rto_min: Option<Duration>,
    pub sctp_rto_max: Option<Duration>,
    pub sctp_max_association_retransmits: Option<u32>,
    pub sctp_receive_window: Option<usize>,
}

impl WebRTCConfig {
    pub fn new(
        _server: String,
        _token: String,
        ice_servers: Vec<IceServerConfig>,
        enable_upnp: bool,
        cfg: &RportConfig,
    ) -> Self {
        Self {
            ice_servers,
            enable_upnp,
            ice_disconnect_threshold: cfg.ice_disconnect_threshold.map(Duration::from_secs),
            ice_disconnect_grace: cfg.ice_disconnect_grace.map(Duration::from_secs),
            ice_connection_timeout: cfg.ice_connection_timeout.map(Duration::from_secs),
            sctp_rto_initial: cfg.sctp_rto_initial_ms.map(Duration::from_millis),
            sctp_rto_min: cfg.sctp_rto_min_ms.map(Duration::from_millis),
            sctp_rto_max: cfg.sctp_rto_max_sec.map(Duration::from_secs),
            sctp_max_association_retransmits: cfg.sctp_max_association_retransmits,
            sctp_receive_window: cfg.sctp_receive_window_kb.map(|kb| kb * 1024),
        }
    }

    fn build_rtc_config(&self, extra_ice_servers: Vec<IceServer>) -> RtcConfiguration {
        let mut ice_servers = self.get_ice_servers();
        ice_servers.extend(extra_ice_servers);
        RtcConfiguration {
            ice_servers,
            sctp_rto_initial: self.sctp_rto_initial.unwrap_or_else(|| Duration::from_millis(400)),
            sctp_rto_min: self.sctp_rto_min.unwrap_or_else(|| Duration::from_millis(200)),
            sctp_rto_max: self.sctp_rto_max.unwrap_or_else(|| Duration::from_secs(30)),
            sctp_max_association_retransmits: self.sctp_max_association_retransmits.unwrap_or(20),
            sctp_receive_window: self.sctp_receive_window.unwrap_or(2 * 1024 * 1024),
            ice_connection_timeout: self.ice_connection_timeout.unwrap_or_else(|| Duration::from_secs(300)),
            ice_disconnect_threshold: self.ice_disconnect_threshold.unwrap_or_else(|| Duration::from_secs(30)),
            ice_disconnect_grace: self.ice_disconnect_grace.unwrap_or_else(|| Duration::from_secs(15)),
            enable_upnp: self.enable_upnp,
            prefer_srflx_over_natted_host: true,
            ..Default::default()
        }
    }

    pub fn get_ice_servers(&self) -> Vec<IceServer> {
        if !self.ice_servers.is_empty() {
            return self.ice_servers.clone().into_iter().map(|c| c.into()).collect();
        }
        vec![IceServerConfig::default().into()]
    }

    pub async fn create_peer_connection(&self) -> Result<Arc<PeerConnection>> {
        self.create_peer_connection_with([].as_slice()).await
    }

    pub async fn create_peer_connection_with(&self, extra_ice_servers: &[IceServerConfig]) -> Result<Arc<PeerConnection>> {
        let extra = extra_ice_servers.iter().map(|c| IceServer::from(c.clone())).collect();
        let config = self.build_rtc_config(extra);
        let peer_connection = Arc::new(PeerConnection::new(config));
        Ok(peer_connection)
    }
}

use crate::{
    acl::Acl,
    config::{IceServerConfig, RportConfig},
    dtls_signaling::{DtlsClient, SignalingMessage, Target, send_message, recv_message},
    webrtc_config::WebRTCConfig,
};
use anyhow::{anyhow, Result};
use bytes::Bytes;
use rustrtc::{
    transports::{
        dtls::DtlsTransport,
        sctp::{DataChannelConfig, DataChannelEvent},
    },
    IceCandidate, IceGatheringState, PeerConnection, SdpType, SessionDescription,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub const RECONNECT_INTERVAL: u64 = 5;

/// Owns a WebRTC peer connection and all background tasks spawned for it.
/// On drop, closes the PC (which unblocks tasks waiting on its channels)
/// and aborts any tasks that are still alive.
struct ActiveSession {
    pc: Arc<PeerConnection>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ActiveSession {
    fn close(&mut self) {
        self.pc.close();
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        self.close();
    }
}

pub struct Agent {
    server_url: String,
    token: String,
    id: String,
    webrtc_config: WebRTCConfig,
    acl: Option<Acl>,
}

impl Agent {
    pub fn new(
        server_url: &str,
        token: &str,
        id: &str,
        ice_servers: Option<Vec<IceServerConfig>>,
        enable_upnp: bool,
        acl: Option<Acl>,
        cfg: &RportConfig,
    ) -> Self {
        let webrtc_config = WebRTCConfig::new(
            server_url.to_string(),
            token.to_string(),
            ice_servers.unwrap_or_default(),
            enable_upnp,
            cfg,
        );
        Self {
            server_url: server_url.to_string(),
            token: token.to_string(),
            id: id.to_string(),
            webrtc_config,
            acl,
        }
    }

    pub async fn run(&self) -> Result<()> {
        info!("Starting DTLS agent '{}' connecting to server {}", self.id, self.server_url);
        loop {
            match self.dtls_server_connect().await {
                Ok(_) => info!("DTLS server connection ended normally"),
                Err(e) => error!("DTLS server connection failed: {}", e),
            }
            info!("Reconnecting in {} seconds...", RECONNECT_INTERVAL);
            tokio::time::sleep(Duration::from_secs(RECONNECT_INTERVAL)).await;
        }
    }

    async fn dtls_server_connect(&self) -> Result<()> {
        let mut client = DtlsClient::connect(&self.server_url, None).await?;
        info!("DTLS connected to server {}", self.server_url);

        // Request ICE server configuration from signaling server
        info!("Requesting ICE server config from signaling server");
        let _ = client.send(&SignalingMessage::GetIceServers).await;

        let extra_ice_servers: Vec<IceServerConfig> = match tokio::time::timeout(
            Duration::from_secs(5),
            client.recv(),
        ).await {
            Ok(Ok(SignalingMessage::IceServers { ice_servers })) => {
                info!("Received ICE server config ({} servers) from signaling server", ice_servers.len());
                ice_servers.into_iter().map(|s| IceServerConfig {
                    urls: s.urls,
                    username: s.username,
                    credential: s.credential,
                }).collect()
            }
            Ok(Ok(other)) => {
                warn!("Expected IceServers after GetIceServers, got {:?}, using defaults", other);
                vec![]
            }
            Ok(Err(e)) => {
                warn!("Error receiving ICE servers: {}, using defaults", e);
                vec![]
            }
            Err(_) => {
                info!("No ICE server config from server (timeout), using defaults");
                vec![]
            }
        };

        send_message(&client.dtls, &SignalingMessage::Register {
            token: self.token.clone(),
            id: self.id.clone(),
        }).await?;
        info!("Registered with server as '{}'", self.id);

        // Keepalive
        let dtls = client.dtls.clone();
        let keepalive = async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if send_message(&dtls, &SignalingMessage::Ping).await.is_err() {
                    warn!("DTLS keepalive send failed");
                    break;
                }
            }
        };

        let message_loop = async {
            let mut active_session: Option<ActiveSession> = None;

            loop {
                let msg = match tokio::time::timeout(
                    Duration::from_secs(45),
                    recv_message(&mut client.data_rx),
                ).await {
                    Ok(Ok(m)) => m,
                    Ok(Err(e)) => {
                        error!("DTLS recv error: {}", e);
                        break;
                    }
                    Err(_) => {
                        warn!("DTLS recv timeout (45s)");
                        break;
                    }
                };

                match msg {
                    SignalingMessage::Offer { session_id, offer_sdp, targets, .. } => {
                        info!("Received offer from server: session={}", session_id);
                        let dtls = client.dtls.clone();
                        let wc = self.webrtc_config.clone();
                        let ac = self.acl.clone();

                        // Drop previous session (close PC + abort tasks)
                        if let Some(mut session) = active_session.take() {
                            session.close();
                        }

                        match handle_offer(
                            dtls, &session_id, &offer_sdp, targets,
                            ac, wc, &extra_ice_servers,
                        ).await {
                            Ok(session) => {
                                active_session = Some(session);
                            }
                            Err(e) => {
                                error!("Failed to handle offer {}: {}", session_id, e);
                            }
                        }
                    }
                    SignalingMessage::Candidate { candidate, .. } => {
                        if let Some(ref session) = active_session {
                            if let Ok(c) = IceCandidate::from_sdp(&candidate) {
                                session.pc.add_ice_candidate(c).ok();
                            }
                        }
                    }
                    SignalingMessage::EndOfCandidates { .. } => {}
                    SignalingMessage::Ping => {
                        send_message(&client.dtls, &SignalingMessage::Pong).await.ok();
                    }
                    SignalingMessage::Pong => {}
                    SignalingMessage::Error { reason, .. } => warn!("Server error: {}", reason),
                    other => warn!("Unexpected message from server: {:?}", other),
                }
            }

            // Cleanup active session on exit
            if let Some(mut session) = active_session.take() {
                session.close();
            }
        };

        tokio::select! {
            _ = keepalive => {}
            _ = message_loop => {}
        }

        Err(anyhow!("DTLS server connection lost"))
    }
}

async fn handle_offer(
    dtls: Arc<DtlsTransport>,
    session_id: &str,
    offer_sdp: &str,
    targets: Option<Vec<Target>>,
    acl: Option<Acl>,
    webrtc_config: WebRTCConfig,
    extra_ice_servers: &[IceServerConfig],
) -> Result<ActiveSession> {
    // Resolve targets
    let targets: Vec<(String, u16)> = if let Some(tgts) = targets {
        tgts.iter().map(|t| {
            let host = t.host.clone().unwrap_or_else(|| "127.0.0.1".to_string());
            (host, t.port)
        }).collect()
    } else {
        vec![("127.0.0.1".to_string(), 22)]
    };

    // ACL check
    if let Some(ref acl) = acl {
        for (host, port) in &targets {
            let ip = match tokio::net::lookup_host(format!("{}:{}", host, port)).await {
                Ok(mut addrs) => addrs.next().map(|a| a.ip()).unwrap_or_else(|| {
                    use std::net::IpAddr;
                    "0.0.0.0".parse::<IpAddr>().unwrap()
                }),
                Err(_) => {
                    send_message(&dtls, &SignalingMessage::Error {
                        session_id: session_id.to_string(),
                        reason: format!("Cannot resolve target: {}", host),
                    }).await.ok();
                    return Err(anyhow!("Cannot resolve target: {}", host));
                }
            };
            if !acl.is_allowed(&ip, *port) {
                send_message(&dtls, &SignalingMessage::Error {
                    session_id: session_id.to_string(),
                    reason: format!("Access denied: {}:{}", host, port),
                }).await.ok();
                return Err(anyhow!("ACL denied: {}:{}", host, port));
            }
        }
    }

    // Create WebRTC peer connection
    let peer_connection = webrtc_config.create_peer_connection_with(extra_ice_servers).await?;

    // Tasks spawned for this session — tracked for cleanup
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // Create data channels for each target
    struct FwdTarget {
        dc_label: String,
        host: String,
        port: u16,
    }

    let fwd_targets: Vec<FwdTarget> = targets.into_iter().map(|(host, port)| {
        let label = format!("fwd:{}:{}", host, port);
        FwdTarget { dc_label: label, host, port }
    }).collect();

    let mut data_channels = Vec::new();
    for ft in &fwd_targets {
        let dc_config = DataChannelConfig {
            ordered: true,
            label: ft.dc_label.clone(),
            ..Default::default()
        };
        let dc = peer_connection.create_data_channel(&ft.dc_label, Some(dc_config))?;
        data_channels.push((dc, ft.host.clone(), ft.port));
    }

    // Set remote description
    let offer = SessionDescription::parse(SdpType::Offer, &offer_sdp)?;
    peer_connection.set_remote_description(offer).await?;

    // Spawn forwarding handlers per data channel
    for (dc, host, port) in data_channels {
        let pc = peer_connection.clone();
        let h = host.clone();

        tasks.push(tokio::spawn(async move {
            let dc_id = dc.id;
            let (tcp_msg_tx, tcp_msg_rx) = mpsc::unbounded_channel::<Bytes>();
            let tcp_msg_rx = std::sync::Arc::new(std::sync::Mutex::new(Some(tcp_msg_rx)));

            loop {
                let event = match dc.recv().await {
                    Some(e) => e,
                    None => break,
                };

                match event {
                    DataChannelEvent::Open => {
                        info!("Data channel opened, forwarding to {}:{}", h, port);
                        let pc2 = pc.clone();
                        let h2 = h.clone();
                        let rx_arc = tcp_msg_rx.clone();

                        tokio::spawn(async move {
                            let rx_opt = rx_arc.lock().unwrap().take();
                            let mut rx = match rx_opt {
                                Some(r) => r,
                                None => return,
                            };

                            match TcpStream::connect(format!("{}:{}", h2, port)).await {
                                Ok(tcp) => {
                                    info!("TCP connected to {}:{}, forwarding...", h2, port);
                                    let (mut tcp_read, mut tcp_write) = tcp.into_split();
                                    let pc3 = pc2.clone();
                                    tokio::spawn(async move {
                                        let mut buf = [0u8; 1024];
                                        loop {
                                            match tcp_read.read(&mut buf).await {
                                                Ok(0) | Err(_) => break,
                                                Ok(n) => {
                                                    if pc3.send_data(dc_id, &buf[..n]).await.is_err() { break; }
                                                }
                                            }
                                        }
                                    });
                                    while let Some(data) = rx.recv().await {
                                        if tcp_write.write_all(&data).await.is_err() { break; }
                                        let _ = tcp_write.flush().await;
                                    }
                                }
                                Err(e) => error!("Failed to connect to {}:{}: {}", h2, port, e),
                            }
                        });
                    }
                    DataChannelEvent::Message(data) => {
                        let _ = tcp_msg_tx.send(Bytes::from(data));
                    }
                    DataChannelEvent::Close => {
                        info!("Data channel closed for {}:{}", h, port);
                        break;
                    }
                }
            }
        }));
    }

    // Drain PeerConnection events
    {
        let pc_drain = peer_connection.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(_) = pc_drain.recv().await {}
        }));
    }

    // Send answer — if this fails, clean up spawned tasks
    let answer_result: Result<(), anyhow::Error> = async {
        let answer = peer_connection.create_answer().await?;
        peer_connection.set_local_description(answer)?;
        let answer_sdp = peer_connection.local_description()
            .ok_or_else(|| anyhow!("No local description"))?
            .to_sdp_string();
        send_message(&dtls, &SignalingMessage::Answer {
            session_id: session_id.to_string(),
            answer_sdp,
        }).await?;
        Ok(())
    }.await;

    if let Err(e) = answer_result {
        for t in &tasks { t.abort(); }
        peer_connection.close();
        return Err(e);
    }

    info!("WebRTC answer sent for session {}", session_id);

    // Trickle ICE: forward gathered candidates as they arrive
    {
        let mut candidate_rx = peer_connection.subscribe_ice_candidates();
        let mut gathering_state_rx = peer_connection.subscribe_ice_gathering_state();
        let dtls_c = dtls.clone();
        let sid = session_id.to_string();
        tasks.push(tokio::spawn(async move {
            if *gathering_state_rx.borrow() == IceGatheringState::Complete {
                let _ = send_message(&dtls_c, &SignalingMessage::EndOfCandidates {
                    session_id: sid.clone(),
                }).await;
                return;
            }
            loop {
                tokio::select! {
                    result = candidate_rx.recv() => {
                        match result {
                            Ok(candidate) => {
                                let _ = send_message(&dtls_c, &SignalingMessage::Candidate {
                                    session_id: sid.clone(),
                                    candidate: candidate.to_sdp(),
                                }).await;
                                if *gathering_state_rx.borrow() == IceGatheringState::Complete {
                                    let _ = send_message(&dtls_c, &SignalingMessage::EndOfCandidates {
                                        session_id: sid.clone(),
                                    }).await;
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    _ = gathering_state_rx.changed() => {
                        if *gathering_state_rx.borrow() == IceGatheringState::Complete {
                            let _ = send_message(&dtls_c, &SignalingMessage::EndOfCandidates {
                                session_id: sid.clone(),
                            }).await;
                            break;
                        }
                    }
                }
            }
        }));
    }

    Ok(ActiveSession { pc: peer_connection, tasks })
}

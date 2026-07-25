use crate::{
    acl::Acl,
    config::IceServerConfig,
    dtls_signaling::{self, DtlsAgent, DtlsAgentSession, DtlsClient, SignalingMessage, send_message, recv_message},
    webrtc_config::WebRTCConfig,
    AnswerMessage, ServerMessage,
};
use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::Client;
use rustrtc::{
    transports::{
        dtls::{Certificate, DtlsTransport},
        sctp::DataChannelEvent,
    },
    PeerConnection, SdpType, SessionDescription,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub const RECONNECT_INTERVAL: u64 = 5; // seconds

#[allow(dead_code)]
struct ConnectionSession {
    session_id: Uuid,
    client_ip: String,
    peer_connection: Arc<PeerConnection>,
}

pub struct Agent {
    server_url: Option<String>,
    token: Option<String>,
    id: Option<String>,
    default_target_host: String,
    default_target_port: u16,
    client: Client,
    webrtc_config: WebRTCConfig,
    // DTLS fields
    dtls_listen: Option<String>,
    dtls_cert: Option<Certificate>,
    acl: Option<Acl>,
}

impl Agent {
    pub fn new(
        server_url: Option<String>,
        token: Option<String>,
        id: Option<String>,
        target_host: String,
        target_port: u16,
        ice_servers: Option<Vec<IceServerConfig>>,
        enable_upnp: bool,
        dtls_listen: Option<String>,
        dtls_cert: Option<Certificate>,
        acl: Option<Acl>,
    ) -> Self {
        let server = server_url.clone().unwrap_or_default();
        let tok = token.clone().unwrap_or_default();
        let webrtc_config = WebRTCConfig::new(
            server,
            tok,
            ice_servers.unwrap_or_default(),
            enable_upnp,
        );
        Self {
            server_url,
            token,
            id,
            default_target_host: target_host,
            default_target_port: target_port,
            client: Client::new(),
            webrtc_config,
            dtls_listen,
            dtls_cert,
            acl,
        }
    }

    //=== HTTP/SSE mode (existing) ===

    pub async fn run(&self) -> Result<()> {
        info!(
            server = ?self.server_url,
            "Starting agent: {} on {}:{}", self.id.as_deref().unwrap_or("?"), self.default_target_host, self.default_target_port
        );
        loop {
            match self.register_and_listen().await {
                Ok(_) => info!("SSE connection ended normally"),
                Err(e) => error!("SSE connection failed: {}", e),
            }
            info!("Reconnecting in {} seconds...", RECONNECT_INTERVAL);
            tokio::time::sleep(Duration::from_secs(RECONNECT_INTERVAL)).await;
        }
    }

    async fn register_and_listen(&self) -> Result<()> {
        let server = self.server_url.as_deref().unwrap_or("");
        let token = self.token.as_deref().unwrap_or("");
        let id = self.id.as_deref().unwrap_or("");
        let url = format!("{}/rport/connect?token={}&id={}", server, token, id);
        info!("Connecting to: {}", server);
        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(anyhow!("Failed to connect: {}", response.status()));
        }
        let mut stream = response.bytes_stream();
        let last_ping = tokio::sync::Mutex::new(tokio::time::Instant::now());
        let handle_stream = async {
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        *last_ping.lock().await = tokio::time::Instant::now();
                        let text = String::from_utf8_lossy(&bytes);
                        for line in text.lines() {
                            if line.starts_with("data: ") {
                                let data = &line[6..];
                                if let Ok(server_msg) = serde_json::from_str::<ServerMessage>(data) {
                                    if let Err(e) = self.handle_server_message(server_msg).await {
                                        error!("Failed to handle server message: {}", e);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => { error!("SSE stream error: {}", e); break; }
                }
            }
        };
        let check_has_ping_loop = async {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if last_ping.lock().await.elapsed() > Duration::from_secs(40) {
                    warn!("No ping received in the last 40 seconds, reconnecting...");
                    break;
                }
            }
        };
        tokio::select! {
            _ = check_has_ping_loop => {},
            _ = handle_stream => {}
        }
        Ok(())
    }

    async fn handle_server_message(&self, message: ServerMessage) -> Result<()> {
        match message.message_type.as_str() {
            "offer" => {
                let data = &message.data;
                let uuid = data["uuid"].as_str().unwrap_or("unknown");
                let offer = data["offer"].as_str().unwrap_or("");
                let client_ip = data["client_ip"].as_str().unwrap_or("unknown");
                info!("Received offer from client {} (IP: {})", uuid, client_ip);
                let answer = self
                    .handle_offer(uuid, offer, client_ip, &self.default_target_host, self.default_target_port)
                    .await?;
                let answer_msg = AnswerMessage { answer };
                let server = self.server_url.as_deref().unwrap_or("");
                let url = format!("{}/rport/answer/{}", server, uuid);
                let response = self.client.post(&url).json(&answer_msg).send().await?;
                if response.status().is_success() {
                    info!("Answer sent successfully");
                } else {
                    error!("Failed to send answer: {}", response.status());
                }
            }
            "ping" => debug!("Received ping from server"),
            _ => warn!("Unknown message type: {}", message.message_type),
        }
        Ok(())
    }

    async fn handle_offer(
        &self,
        session_id: &str,
        offer_sdp: &str,
        client_ip: &str,
        target_host: &str,
        target_port: u16,
    ) -> Result<String> {
        info!("Creating WebRTC peer connection for session: {}", session_id);
        let peer_connection = self.webrtc_config.create_peer_connection().await?;

        let dc_config = rustrtc::transports::sctp::DataChannelConfig {
            ordered: true,
            ..Default::default()
        };
        let data_channel = peer_connection.create_data_channel("port-forward", Some(dc_config))?;

        let offer = SessionDescription::parse(SdpType::Offer, &offer_sdp)?;
        peer_connection.set_remote_description(offer).await?;

        let target_host = target_host.to_string();
        let target_port = target_port;
        let client_ip = client_ip.to_string();
        let pc_clone = peer_connection.clone();
        let dc_clone = data_channel.clone();
        let dc_id = data_channel.id;

        let session_id_str = session_id.to_string();
        let setup_cancel = tokio_util::sync::CancellationToken::new();
        let task_cancel = setup_cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = task_cancel.cancelled() => {
                    debug!(session = session_id_str, "Setup failed, cleaning up peer connection");
                    pc_clone.close();
                }
                _ = async {
                    let inner_cancel = tokio_util::sync::CancellationToken::new();
                    let (tcp_write_tx, tcp_write_rx) = mpsc::unbounded_channel();
                    let mut tcp_write_rx = Some(tcp_write_rx);
                    let session_start = tokio::time::Instant::now();
                    let mut dc_msg_count: u64 = 0;
                    let mut dc_bytes_recv: u64 = 0;

                    let pc_monitor = pc_clone.clone();
                    let sid = session_id_str.clone();
                    tokio::spawn(async move {
                        let mut state_rx = pc_monitor.subscribe_peer_state();
                        while let Ok(()) = state_rx.changed().await {
                            let state = *state_rx.borrow();
                            match state {
                                rustrtc::PeerConnectionState::Connected => {
                                    info!(session = sid, "WebRTC connected");
                                }
                                rustrtc::PeerConnectionState::Disconnected
                                | rustrtc::PeerConnectionState::Failed
                                | rustrtc::PeerConnectionState::Closed => {
                                    if let Some(reason) = pc_monitor.disconnect_reason() {
                                        warn!(session = sid, "WebRTC connection ended: {} (state: {:?})", reason, state);
                                    } else {
                                        warn!(session = sid, "WebRTC connection ended: state {:?}", state);
                                    }
                                    break;
                                }
                                _ => debug!(session = sid, "Peer connection state: {:?}", state),
                            }
                        }
                    });

                    while let Some(dc_event) = dc_clone.recv().await {
                        match dc_event {
                            DataChannelEvent::Open => {
                                if let Some(rx) = tcp_write_rx.take() {
                                    let target_host = target_host.clone();
                                    let client_ip = client_ip.clone();
                                    let pc = pc_clone.clone();
                                    let cancel_token = task_cancel.clone();
                                    let sid = session_id_str.clone();
                                    info!(
                                        session = session_id_str, client_ip,
                                        "Data channel opened, starting TCP-WebRTC forwarding to {}:{}",
                                        target_host, target_port
                                    );
                                    tokio::spawn(async move {
                                        tcp_webrtc_forwarding(
                                            cancel_token, rx, sid, client_ip, pc, dc_id,
                                            &target_host, target_port,
                                        ).await.ok();
                                    });
                                }
                            }
                            DataChannelEvent::Message(data) => {
                                if !data.is_empty() {
                                    dc_msg_count += 1;
                                    dc_bytes_recv += data.len() as u64;
                                }
                                let _ = tcp_write_tx.send(Bytes::from(data));
                            }
                            DataChannelEvent::Close => {
                                let elapsed = session_start.elapsed();
                                let reason_str = pc_clone.disconnect_reason()
                                    .map(|r| format!("{}", r))
                                    .unwrap_or_else(|| "unknown".to_string());
                                info!(
                                    session = session_id_str,
                                    "Data channel closed: reason={}, duration={:.1}s, msgs={}, bytes_recv={}",
                                    reason_str, elapsed.as_secs_f64(), dc_msg_count, dc_bytes_recv
                                );
                                inner_cancel.cancel();
                                pc_clone.close();
                                break;
                            }
                        }
                    }
                    let elapsed = session_start.elapsed();
                    info!(
                        session = session_id_str,
                        "DataChannel event loop ended after {:.1}s, ensuring peer connection is closed",
                        elapsed.as_secs_f64()
                    );
                    pc_clone.close();
                } => {}
            }
        });

        let pc_clone_drain = peer_connection.clone();
        tokio::spawn(async move { while let Some(_) = pc_clone_drain.recv().await {} });

        let answer = match peer_connection.create_answer().await {
            Ok(a) => a,
            Err(e) => { setup_cancel.cancel(); return Err(e.into()); }
        };
        if let Err(e) = peer_connection.set_local_description(answer.clone()) {
            setup_cancel.cancel();
            return Err(e.into());
        }
        peer_connection.wait_for_gathering_complete().await;
        let answer_sdp = match peer_connection.local_description() {
            Some(d) => d.to_sdp_string(),
            None => { setup_cancel.cancel(); return Err(anyhow!("Failed to get local description")); }
        };
        Ok(answer_sdp)
    }

    //=== DTLS Agent via Server mode ===

    pub async fn run_via_dtls_server(&self) -> Result<()> {
        let server = self.server_url.as_deref().ok_or_else(|| anyhow!("--dtls-server address required"))?;
        let token = self.token.as_deref().ok_or_else(|| anyhow!("--token required"))?;
        let id = self.id.as_deref().unwrap_or("default");
        info!("Starting DTLS agent connecting to server {} as '{}'", server, id);

        loop {
            match self.dtls_server_connect(server, token, id).await {
                Ok(_) => info!("DTLS server connection ended normally"),
                Err(e) => error!("DTLS server connection failed: {}", e),
            }
            info!("Reconnecting in {} seconds...", RECONNECT_INTERVAL);
            tokio::time::sleep(Duration::from_secs(RECONNECT_INTERVAL)).await;
        }
    }

    async fn dtls_server_connect(&self, server: &str, token: &str, id: &str) -> Result<()> {
        let mut client = DtlsClient::connect(server, None).await?;
        info!("DTLS connected to server {}", server);

        send_message(&client.dtls, &SignalingMessage::Register {
            token: token.to_string(), id: id.to_string(),
            fingerprint: "_".to_string(),
        }).await?;
        info!("Registered with server as '{}'", id);

        // Keepalive: send ping every 10s, detect stale connection
        let keepalive = async {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if send_message(&client.dtls, &SignalingMessage::Ping).await.is_err() {
                    warn!("DTLS keepalive send failed");
                    break;
                }
            }
        };

        let message_loop = async {
            loop {
                let msg = match tokio::time::timeout(
                    Duration::from_secs(25),
                    recv_message(&mut client.data_rx),
                ).await {
                    Ok(Ok(m)) => m,
                    Ok(Err(e)) => {
                        error!("DTLS recv error: {}", e);
                        break;
                    }
                    Err(_) => {
                        warn!("DTLS recv timeout (25s) — no message from server");
                        break;
                    }
                };
                match msg {
                    SignalingMessage::Offer { session_id, offer_sdp, targets, .. } => {
                        info!("Received offer from server: session={}", session_id);
                        let dtls = client.dtls.clone();
                        let wc = self.webrtc_config.clone();
                        let dh = self.default_target_host.clone();
                        let dp = self.default_target_port;
                        let ac = self.acl.clone();

                        let result = handle_dtls_offer_v2(
                            dtls, &session_id, &offer_sdp, targets,
                            ac, wc, &dh, dp,
                        ).await;
                        if let Err(e) = result {
                            error!("Failed to handle offer {}: {}", session_id, e);
                        }
                    }
                    SignalingMessage::Registered {} => info!("Registered with server"),
                    SignalingMessage::Ping => {
                        send_message(&client.dtls, &SignalingMessage::Pong).await.ok();
                    }
                    SignalingMessage::Pong => {}
                    SignalingMessage::Error { reason, .. } => warn!("Server error: {}", reason),
                    other => warn!("Unexpected: {:?}", other),
                }
            }
        };

        tokio::select! {
            _ = keepalive => {}
            _ = message_loop => {}
        }

        Err(anyhow!("DTLS server connection lost"))
    }

    //=== DTLS mode (direct) ===

    pub async fn run_dtls(&self) -> Result<()> {
        let listen_addr = self.dtls_listen.as_deref().unwrap_or("0.0.0.0:4443");
        info!("Starting DTLS agent on {}", listen_addr);

        let mut agent = DtlsAgent::bind(listen_addr, self.dtls_cert.clone()).await?;
        info!("Agent fingerprint: {}", agent.fingerprint());

        loop {
            let mut session = match agent.accept().await {
                Some(s) => s,
                None => {
                    error!("Failed to accept DTLS connection, retrying...");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            let acl = self.acl.clone();
            let webrtc_config = self.webrtc_config.clone();
            let default_host = self.default_target_host.clone();
            let default_port = self.default_target_port;
            let agent_token = self.token.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_dtls_session(&mut session, acl, webrtc_config, agent_token, &default_host, default_port).await {
                    error!("DTLS session error: {}", e);
                }
                session.dtls.close();
            });
        }
    }
}

// For DTLS server mode: agent receives offer via server, answers back through DTLS
async fn handle_dtls_offer_v2(
    dtls: Arc<DtlsTransport>,
    session_id: &str,
    offer_sdp: &str,
    targets: Option<Vec<dtls_signaling::Target>>,
    _acl: Option<Acl>,
    webrtc_config: WebRTCConfig,
    default_host: &str,
    default_port: u16,
) -> Result<()> {
    let resolved: Vec<(String, u16)> = if let Some(tgts) = targets {
        tgts.iter().map(|t| {
            (t.host.clone().unwrap_or_else(|| default_host.to_string()), t.port)
        }).collect()
    } else {
        vec![(default_host.to_string(), default_port)]
    };

    // Create WebRTC peer connection and data channels
    let peer_connection = webrtc_config.create_peer_connection().await?;
    for (h, p) in &resolved {
        let label = format!("fwd:{}:{}", h, p);
        let dc_config = rustrtc::transports::sctp::DataChannelConfig {
            ordered: true, label: label.clone(), ..Default::default()
        };
        peer_connection.create_data_channel(&label, Some(dc_config))?;
    }

    let offer = SessionDescription::parse(SdpType::Offer, &offer_sdp)?;
    peer_connection.set_remote_description(offer).await?;

    let answer = peer_connection.create_answer().await?;
    peer_connection.set_local_description(answer.clone())?;
    peer_connection.wait_for_gathering_complete().await;
    let answer_sdp = peer_connection.local_description()
        .ok_or_else(|| anyhow!("No local description"))?
        .to_sdp_string();

    dtls_signaling::send_message(&dtls, &SignalingMessage::Answer {
        session_id: session_id.to_string(),
        answer_sdp,
    }).await?;

    info!("DTLS server-mode answer sent for session {}", session_id);

    // Spawn forwarding handlers (same as direct mode)
    let cancel = tokio_util::sync::CancellationToken::new();
    drain_pc_events(peer_connection, cancel);

    Ok(())
}

fn drain_pc_events(pc: Arc<PeerConnection>, cancel: tokio_util::sync::CancellationToken) {
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = async { while let Some(_) = pc.recv().await {} } => {}
        }
    });
}

async fn handle_dtls_session(
    session: &mut DtlsAgentSession,
    acl: Option<Acl>,
    webrtc_config: WebRTCConfig,
    expected_token: Option<String>,
    default_host: &str,
    default_port: u16,
) -> Result<()> {
    let msg = dtls_signaling::recv_message(&mut session.data_rx).await?;

    let (offer_sdp, targets) = match msg {
        SignalingMessage::Offer { offer_sdp, targets, token, .. } => {
            // Token validation for direct mode
            if let Some(ref expected) = expected_token {
                if let Some(ref provided) = token {
                    if provided != expected {
                        let _ = dtls_signaling::send_message(&session.dtls, &SignalingMessage::Error {
                            session_id: "".to_string(),
                            reason: "Invalid token".to_string(),
                        }).await;
                        return Err(anyhow!("Token mismatch: expected '{}', got '{}'", expected, provided));
                    }
                } else {
                    let _ = dtls_signaling::send_message(&session.dtls, &SignalingMessage::Error {
                        session_id: "".to_string(),
                        reason: "Token required".to_string(),
                    }).await;
                    return Err(anyhow!("Token required but not provided by client"));
                }
            }
            (offer_sdp, targets)
        }
        SignalingMessage::Error { reason, .. } => {
            warn!("Received error from client: {}", reason);
            return Err(anyhow!("Client error: {}", reason));
        }
        other => {
            warn!("Unexpected first message: {:?}", other);
            let _ = dtls_signaling::send_message(&session.dtls, &SignalingMessage::Error {
                session_id: "".to_string(),
                reason: "Expected offer as first message".to_string(),
            }).await;
            return Err(anyhow!("Unexpected message"));
        }
    };

    // Resolve targets
    let targets: Vec<(String, u16)> = if let Some(tgts) = targets {
        tgts.iter().map(|t| {
            let host = t.host.clone().unwrap_or_else(|| default_host.to_string());
            (host, t.port)
        }).collect()
    } else {
        vec![(default_host.to_string(), default_port)]
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
                    let _ = dtls_signaling::send_message(&session.dtls, &SignalingMessage::Error {
                        session_id: "".to_string(),
                        reason: format!("Cannot resolve target host: {}", host),
                    }).await;
                    return Err(anyhow!("Cannot resolve target: {}", host));
                }
            };
            if !acl.is_allowed(&ip, *port) {
                let _ = dtls_signaling::send_message(&session.dtls, &SignalingMessage::Error {
                    session_id: "".to_string(),
                    reason: format!("Access denied: {}:{}", host, port),
                }).await;
                return Err(anyhow!("ACL denied: {}:{}", host, port));
            }
        }
    }

    let peer_connection = webrtc_config.create_peer_connection().await?;

    // Create a data channel for each target
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
        let dc_config = rustrtc::transports::sctp::DataChannelConfig {
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

        tokio::spawn(async move {
            let dc_id = dc.id;
            // TCP write receiver — shared between Open handler and DC event loop
            let (tcp_msg_tx, tcp_msg_rx) = mpsc::unbounded_channel::<Bytes>();
            let tcp_msg_rx = std::sync::Arc::new(std::sync::Mutex::new(Some(tcp_msg_rx)));

            loop {
                let event = match tokio::time::timeout(Duration::from_secs(5), dc.recv()).await {
                    Ok(Some(e)) => e,
                    Ok(None) | Err(_) => break,
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
        });
    }

    // Drain PeerConnection events
    let pc_drain = peer_connection.clone();
    tokio::spawn(async move {
        while let Some(_) = pc_drain.recv().await {}
    });

    // Create answer
    let answer = peer_connection.create_answer().await?;
    peer_connection.set_local_description(answer.clone())?;
    peer_connection.wait_for_gathering_complete().await;

    let answer_sdp = peer_connection.local_description()
        .ok_or_else(|| anyhow!("No local description"))?
        .to_sdp_string();

    // Send answer over DTLS
    dtls_signaling::send_message(&session.dtls, &SignalingMessage::Answer {
        session_id: "".to_string(),
        answer_sdp,
    }).await?;

    info!("DTLS signaling complete, WebRTC connecting...");
    Ok(())
}

//=== TCP-WebRTC forwarding (shared) ===

async fn tcp_webrtc_forwarding(
    cancel_token: tokio_util::sync::CancellationToken,
    mut tcp_write_rx: mpsc::UnboundedReceiver<Bytes>,
    session_id: String,
    client_ip: String,
    peer_connection: Arc<PeerConnection>,
    channel_id: u16,
    target_host: &str,
    target_port: u16,
) -> Result<()> {
    let tcp_stream = match TcpStream::connect(format!("{}:{}", target_host, target_port)).await {
        Ok(stream) => stream,
        Err(e) => {
            error!(session = session_id, client_ip, "Failed to connect to {}:{}: {}", target_host, target_port, e);
            peer_connection.close();
            return Err(anyhow!("Failed to connect to {}:{}", target_host, target_port));
        }
    };

    info!(session = session_id, client_ip, "Setting up bidirectional forwarding for {}:{}", target_host, target_port);
    let fwd_start = tokio::time::Instant::now();
    let tcp_to_dc_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dc_to_tcp_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (mut tcp_read, mut tcp_write) = tcp_stream.into_split();

    let tcp_to_dc_counter = tcp_to_dc_bytes.clone();
    let recv_from_tcp = async {
        let mut buffer = [0u8; 1024];
        loop {
            match tcp_read.read(&mut buffer).await {
                Ok(0) => { debug!(session = session_id, "TCP connection closed (EOF)"); break; }
                Ok(n) => {
                    tcp_to_dc_counter.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                    if let Err(e) = peer_connection.send_data(channel_id, &buffer[..n]).await {
                        error!(session = session_id, "Failed to send data through WebRTC: {}", e);
                        break;
                    }
                }
                Err(e) => { error!(session = session_id, "TCP read error: {}", e); break; }
            }
        }
    };

    let dc_to_tcp_counter = dc_to_tcp_bytes.clone();
    let sid2 = session_id.clone();
    let recv_from_data_channel = async move {
        while let Some(msg) = tcp_write_rx.recv().await {
            if msg.is_empty() { continue; }
            dc_to_tcp_counter.fetch_add(msg.len() as u64, std::sync::atomic::Ordering::Relaxed);
            if let Err(e) = tcp_write.write_all(&msg).await {
                error!(session = sid2, "Failed to write to TCP stream: {}", e);
                break;
            }
            if let Err(e) = tcp_write.flush().await {
                error!(session = sid2, "Failed to flush TCP stream: {}", e);
                break;
            }
        }
        let _ = tcp_write.shutdown().await;
    };

    let exit_reason;
    tokio::select! {
        _ = cancel_token.cancelled() => { exit_reason = "cancel"; }
        _ = recv_from_data_channel => { exit_reason = "dc_closed"; }
        _ = recv_from_tcp => { exit_reason = "tcp_closed"; }
    }

    let elapsed = fwd_start.elapsed();
    let t2d = tcp_to_dc_bytes.load(std::sync::atomic::Ordering::Relaxed);
    let d2t = dc_to_tcp_bytes.load(std::sync::atomic::Ordering::Relaxed);
    info!(
        session = session_id, client_ip,
        "Forwarding ended: reason={}, duration={:.1}s, tcp->dc={}B, dc->tcp={}B",
        exit_reason, elapsed.as_secs_f64(), t2d, d2t
    );
    drop(tcp_read);
    Ok(())
}

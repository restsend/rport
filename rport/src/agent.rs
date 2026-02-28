use crate::{AnswerMessage, ServerMessage};
use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::Client;
use rustrtc::{transports::sctp::DataChannelEvent, PeerConnection, SdpType, SessionDescription};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::IceServerConfig;
use crate::webrtc_config::WebRTCConfig;

pub const RECONNECT_INTERVAL: u64 = 5; // seconds

#[allow(dead_code)]
struct ConnectionSession {
    session_id: Uuid,
    client_ip: String,
    peer_connection: Arc<PeerConnection>,
}

pub struct Agent {
    server_url: String,
    token: String,
    id: String,
    target_host: String,
    target_port: u16,
    client: Client,
    webrtc_config: WebRTCConfig,
}

impl Agent {
    pub fn new(
        server_url: String,
        token: String,
        id: String,
        target_host: String,
        target_port: u16,
        ice_servers: Option<Vec<IceServerConfig>>,
    ) -> Self {
        let webrtc_config = WebRTCConfig::new(
            server_url.clone(),
            token.clone(),
            ice_servers.unwrap_or_default(),
        );
        Self {
            server_url,
            token,
            id,
            target_host,
            target_port,
            client: Client::new(),
            webrtc_config,
        }
    }

    pub async fn run(&self) -> Result<()> {
        info!(
            server = self.server_url,
            "Starting agent: {} on {}:{}", self.id, self.target_host, self.target_port
        );

        loop {
            match self.register_and_listen().await {
                Ok(_) => {
                    info!("SSE connection ended normally");
                }
                Err(e) => {
                    error!("SSE connection failed: {}", e);
                }
            }

            info!("Reconnecting in {} seconds...", RECONNECT_INTERVAL);
            tokio::time::sleep(Duration::from_secs(RECONNECT_INTERVAL)).await;
        }
    }

    async fn register_and_listen(&self) -> Result<()> {
        let url = format!(
            "{}/rport/connect?token={}&id={}",
            self.server_url, self.token, self.id
        );
        info!("Connecting to: {}", self.server_url);
        // Use SSE connection instead of WebSocket
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
                        // Parse SSE events
                        for line in text.lines() {
                            if line.starts_with("data: ") {
                                let data = &line[6..]; // Remove "data: " prefix
                                if let Ok(server_msg) = serde_json::from_str::<ServerMessage>(data)
                                {
                                    if let Err(e) = self.handle_server_message(server_msg).await {
                                        error!("Failed to handle server message: {}", e);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("SSE stream error: {}", e);
                        break;
                    }
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
                let answer = self.handle_offer(uuid, offer, client_ip).await?;

                // Send answer back via HTTP POST
                let answer_msg = AnswerMessage { answer };
                let url = format!("{}/rport/answer/{}", self.server_url, uuid);

                let response = self.client.post(&url).json(&answer_msg).send().await?;
                if response.status().is_success() {
                    info!("Answer sent successfully");
                } else {
                    error!("Failed to send answer: {}", response.status());
                }
            }
            "ping" => {
                debug!("Received ping from server");
            }
            _ => {
                warn!("Unknown message type: {}", message.message_type);
            }
        }
        Ok(())
    }

    async fn handle_offer(
        &self,
        session_id: &str,
        offer_sdp: &str,
        client_ip: &str,
    ) -> Result<String> {
        info!(
            "Creating WebRTC peer connection for session: {}",
            session_id
        );

        let peer_connection = self.webrtc_config.create_peer_connection().await?;

        // Create negotiated data channel
        use rustrtc::transports::sctp::DataChannelConfig;
        let dc_config = DataChannelConfig {
            ordered: true,
            ..Default::default()
        };
        let data_channel = peer_connection.create_data_channel("port-forward", Some(dc_config))?;

        // Set remote description first
        let offer = SessionDescription::parse(SdpType::Offer, &offer_sdp)?;
        peer_connection.set_remote_description(offer).await?;

        // Set up data channel handler
        let target_host = self.target_host.clone();
        let target_port = self.target_port;
        let client_ip = client_ip.to_string();
        let pc_clone = peer_connection.clone();
        let dc_clone = data_channel.clone();
        let dc_id = data_channel.id;

        // Handle DataChannel events
        let session_id_str = session_id.to_string();
        tokio::spawn(async move {
            let cancel_token = tokio_util::sync::CancellationToken::new();
            let (tcp_write_tx, tcp_write_rx) = mpsc::unbounded_channel();
            let mut tcp_write_rx = Some(tcp_write_rx);
            let session_start = tokio::time::Instant::now();
            let mut dc_msg_count: u64 = 0;
            let mut dc_bytes_recv: u64 = 0;
            let mut dc_keepalive_count: u64 = 0;

            // Monitor peer connection state for disconnect reasons
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
                                warn!(
                                    session = sid,
                                    "WebRTC connection ended: {} (state: {:?})", reason, state
                                );
                            } else {
                                warn!(session = sid, "WebRTC connection ended: state {:?}", state);
                            }
                            break;
                        }
                        _ => {
                            debug!(session = sid, "Peer connection state: {:?}", state);
                        }
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
                            let cancel_token = cancel_token.clone();
                            let sid = session_id_str.clone();
                            info!(
                                session = session_id_str,
                                client_ip,
                                "Data channel opened, starting TCP-WebRTC forwarding to {}:{}",
                                target_host,
                                target_port
                            );
                            tokio::spawn(async move {
                                tcp_webrtc_forwarding(
                                    cancel_token,
                                    rx,
                                    sid,
                                    client_ip,
                                    pc,
                                    dc_id,
                                    &target_host,
                                    target_port,
                                )
                                .await
                                .ok();
                            });
                        }
                    }
                    DataChannelEvent::Message(data) => {
                        if data.is_empty() {
                            dc_keepalive_count += 1;
                        } else {
                            dc_msg_count += 1;
                            dc_bytes_recv += data.len() as u64;
                        }
                        let _ = tcp_write_tx.send(Bytes::from(data));
                    }
                    DataChannelEvent::Close => {
                        let elapsed = session_start.elapsed();
                        let reason_str = pc_clone
                            .disconnect_reason()
                            .map(|r| format!("{}", r))
                            .unwrap_or_else(|| "unknown".to_string());
                        info!(
                            session = session_id_str,
                            "Data channel closed: reason={}, duration={:.1}s, msgs={}, bytes_recv={}, keepalives={}",
                            reason_str,
                            elapsed.as_secs_f64(),
                            dc_msg_count,
                            dc_bytes_recv,
                            dc_keepalive_count
                        );
                        cancel_token.cancel();
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
        });

        // Drain PeerConnection events
        let pc_clone_drain = peer_connection.clone();
        tokio::spawn(async move {
            while let Some(_) = pc_clone_drain.recv().await {
                // drain
            }
        });

        // Create answer
        let answer = peer_connection.create_answer().await?;
        peer_connection.set_local_description(answer.clone())?;

        // Wait for ICE gathering to complete
        peer_connection.wait_for_gathering_complete().await;

        let answer_sdp = peer_connection
            .local_description()
            .ok_or_else(|| anyhow!("Failed to get local description"))?
            .to_sdp_string();
        Ok(answer_sdp)
    }
}

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
            error!(
                session = session_id,
                client_ip, "Failed to connect to {}:{}: {}", target_host, target_port, e
            );
            let _ = peer_connection.close();
            return Err(anyhow!(
                "Failed to connect to {}:{}",
                target_host,
                target_port
            ));
        }
    };

    info!(
        session = session_id,
        client_ip, "Setting up bidirectional forwarding for {}:{}", target_host, target_port
    );

    let fwd_start = tokio::time::Instant::now();
    let tcp_to_dc_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dc_to_tcp_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dc_to_tcp_keepalives = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let (mut tcp_read, mut tcp_write) = tcp_stream.into_split();
    let max_read_timeout = Duration::from_secs(1800); // 30 minutes

    let tcp_to_dc_counter = tcp_to_dc_bytes.clone();
    let recv_from_tcp = async {
        let mut buffer = [0u8; 1024];
        loop {
            let r = tokio::time::timeout(max_read_timeout, tcp_read.read(&mut buffer)).await;
            match r {
                Ok(Ok(0)) => {
                    debug!(session = session_id, "TCP connection closed (EOF)");
                    break;
                }
                Ok(Ok(n)) => {
                    tcp_to_dc_counter.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                    let data = &buffer[..n];
                    if let Err(e) = peer_connection.send_data(channel_id, data).await {
                        error!(
                            session = session_id,
                            "Failed to send data through WebRTC: {}", e
                        );
                        break;
                    }
                }
                Ok(Err(e)) => {
                    error!(session = session_id, "TCP read error: {}", e);
                    break;
                }
                Err(_) => {
                    warn!(
                        session = session_id,
                        "TCP read timeout ({}s), closing",
                        max_read_timeout.as_secs()
                    );
                    break;
                }
            }
        }
    };

    let dc_to_tcp_counter = dc_to_tcp_bytes.clone();
    let dc_keepalive_counter = dc_to_tcp_keepalives.clone();
    let sid2 = session_id.clone();
    let recv_from_data_channel = async move {
        while let Some(msg) = tcp_write_rx.recv().await {
            // Ignore zero-length messages (keepalive pings from client)
            if msg.is_empty() {
                dc_keepalive_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                continue;
            }
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
        // Shutdown TCP write side when data channel closes
        let _ = tcp_write.shutdown().await;
    };

    let exit_reason;
    tokio::select! {
        _ = cancel_token.cancelled() => {
            exit_reason = "cancel";
        }
        _ = recv_from_data_channel => {
            exit_reason = "dc_closed";
        }
        _ = recv_from_tcp => {
            exit_reason = "tcp_closed";
        }
    }

    let elapsed = fwd_start.elapsed();
    let t2d = tcp_to_dc_bytes.load(std::sync::atomic::Ordering::Relaxed);
    let d2t = dc_to_tcp_bytes.load(std::sync::atomic::Ordering::Relaxed);
    let ka = dc_to_tcp_keepalives.load(std::sync::atomic::Ordering::Relaxed);
    info!(
        session = session_id,
        client_ip,
        "Forwarding ended: reason={}, duration={:.1}s, tcp->dc={}B, dc->tcp={}B, keepalives={}",
        exit_reason,
        elapsed.as_secs_f64(),
        t2d,
        d2t,
        ka
    );

    drop(tcp_read);

    // Note: PeerConnection will be closed by the DataChannel event handler
    Ok(())
}

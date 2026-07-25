use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::Client;
use rustrtc::{
    transports::sctp::{DataChannel, DataChannelConfig, DataChannelEvent},
    PeerConnection, PeerConnectionEvent, SdpType, SessionDescription,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, warn};

use crate::config::{ForwardMapping, IceServerConfig};
use crate::dtls_signaling::{DtlsClient, SignalingMessage, Target};
use crate::known_hosts::KnownHosts;
use crate::webrtc_config::WebRTCConfig;
use crate::OfferMessage;

const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
pub struct ForwardStats {
    pub bytes_sent: AtomicU64,
    pub bytes_recv: AtomicU64,
    pub packets_sent: AtomicU64,
    pub packets_recv: AtomicU64,
}

/// Spawn a periodic stats reporter that logs throughput every 10 seconds.
pub fn spawn_stats_reporter(label: &str, stats: Arc<ForwardStats>) {
    let label = label.to_string();
    tokio::spawn(async move {
        let mut prev_sent = 0u64;
        let mut prev_recv = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let sent = stats.bytes_sent.load(Ordering::Relaxed);
            let recv = stats.bytes_recv.load(Ordering::Relaxed);
            let p_sent = stats.packets_sent.load(Ordering::Relaxed);
            let p_recv = stats.packets_recv.load(Ordering::Relaxed);
            let d_sent = sent.saturating_sub(prev_sent);
            let d_recv = recv.saturating_sub(prev_recv);
            let sent_kbps = d_sent as f64 / 10.0 / 1024.0;
            let recv_kbps = d_recv as f64 / 10.0 / 1024.0;
            tracing::info!(
                "[stats] {} | sent: {}B ({} pkts, {:.1} KB/s) recv: {}B ({} pkts, {:.1} KB/s) total: {}B↑ {}B↓",
                label,
                sent, p_sent, sent_kbps,
                recv, p_recv, recv_kbps,
                sent, recv,
            );
            prev_sent = sent;
            prev_recv = recv;
        }
    });
}

//=== Generic stream ↔ WebRTC forwarder ===

pub async fn forward_stream_to_webrtc<R, W>(
    peer_connection: Arc<PeerConnection>,
    data_channel: Arc<DataChannel>,
    connect_timeout: Option<u32>,
    stats: Option<Arc<ForwardStats>>,
    mut input: R,
    mut output: W,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (open_tx, open_rx) = tokio::sync::oneshot::channel();
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let dc_closed = tokio_util::sync::CancellationToken::new();

    let dc_clone = data_channel.clone();
    let pc_disc = peer_connection.clone();
    let dc_closed_tx = dc_closed.clone();
    let stats_clone = stats.clone();
    tokio::spawn(async move {
        let mut open_tx = Some(open_tx);
        while let Some(event) = dc_clone.recv().await {
            match event {
                DataChannelEvent::Open => { if let Some(tx) = open_tx.take() { let _ = tx.send(()); } }
                DataChannelEvent::Message(data) => {
                    if let Some(ref s) = stats_clone {
                        s.bytes_recv.fetch_add(data.len() as u64, Ordering::Relaxed);
                        s.packets_recv.fetch_add(1, Ordering::Relaxed);
                    }
                    let _ = msg_tx.send(data);
                }
                DataChannelEvent::Close => {
                    if let Some(reason) = pc_disc.disconnect_reason() {
                        tracing::warn!("Data channel closed (reason: {})", reason);
                    }
                    dc_closed_tx.cancel();
                    break;
                }
            }
        }
    });

    let connect_timeout = connect_timeout.unwrap_or(30);
    if let Err(_) = tokio::time::timeout(Duration::from_secs(connect_timeout.into()), open_rx).await {
        return Err(anyhow!("Data channel open timeout"));
    }

    const DISCONNECT_GRACE: Duration = Duration::from_secs(15);
    let pc_monitor = peer_connection.clone();
    let webrtc_dead = tokio_util::sync::CancellationToken::new();
    let webrtc_dead_tx = webrtc_dead.clone();
    tokio::spawn(async move {
        let mut state_rx = pc_monitor.subscribe_peer_state();
        loop {
            if state_rx.changed().await.is_err() { return; }
            let state = *state_rx.borrow();
            match state {
                rustrtc::PeerConnectionState::Failed | rustrtc::PeerConnectionState::Closed => {
                    if let Some(reason) = pc_monitor.disconnect_reason() {
                        tracing::warn!("WebRTC connection lost: {} (state: {:?})", reason, state);
                    } else { tracing::warn!("WebRTC connection lost: state {:?}", state); }
                    webrtc_dead_tx.cancel(); return;
                }
                rustrtc::PeerConnectionState::Disconnected => {
                    tracing::warn!("WebRTC disconnected; waiting up to {:?} for recovery", DISCONNECT_GRACE);
                    let deadline = tokio::time::Instant::now() + DISCONNECT_GRACE;
                    let mut recovered = false;
                    loop {
                        tokio::select! {
                            changed = state_rx.changed() => {
                                if changed.is_err() { break; }
                                match *state_rx.borrow() {
                                    rustrtc::PeerConnectionState::Connected => { recovered = true; break; }
                                    rustrtc::PeerConnectionState::Failed | rustrtc::PeerConnectionState::Closed => {
                                        if let Some(reason) = pc_monitor.disconnect_reason() {
                                            tracing::warn!("WebRTC connection lost during grace: {}", reason);
                                        }
                                        webrtc_dead_tx.cancel(); return;
                                    }
                                    _ => {}
                                }
                            }
                            _ = tokio::time::sleep_until(deadline) => { break; }
                        }
                    }
                    if !recovered {
                        if let Some(reason) = pc_monitor.disconnect_reason() {
                            tracing::warn!("WebRTC did not recover within {:?}, giving up: {}", DISCONNECT_GRACE, reason);
                        } else { tracing::warn!("WebRTC did not recover within {:?}, giving up", DISCONNECT_GRACE); }
                        webrtc_dead_tx.cancel(); return;
                    }
                }
                _ => {}
            }
        }
    });

    let pc_clone = peer_connection.clone();
    let dc_id = data_channel.id;
    let stats_input = stats.clone();
    let input_task = async move {
        let mut buffer = [0u8; 1200];
        loop {
            match input.read(&mut buffer).await {
                Ok(0) => { tracing::debug!("forward_stream_to_webrtc: input EOF"); break; }
                Ok(n) => {
                    if let Some(ref s) = stats_input {
                        s.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
                        s.packets_sent.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Err(e) = pc_clone.send_data(dc_id, &buffer[..n]).await {
                        tracing::error!("Failed to send data through WebRTC: {}", e); break;
                    }
                }
                Err(e) => { tracing::debug!("forward_stream_to_webrtc: input read failed: {}", e); break; }
            }
        }
    };

    let mut output_task = tokio::spawn(async move {
        while let Some(data) = msg_rx.recv().await {
            if output.write_all(&data).await.is_err() { break; }
            if output.flush().await.is_err() { break; }
        }
    });

    tokio::select! {
        _ = webrtc_dead.cancelled() => { tracing::debug!("forward_stream_to_webrtc: exiting due to WebRTC disconnect"); }
        _ = dc_closed.cancelled() => { tracing::debug!("forward_stream_to_webrtc: data channel closed by remote"); }
        _ = input_task => {
            tracing::debug!("forward_stream_to_webrtc: input closed, waiting for drain");
            tokio::select! {
                _ = tokio::time::sleep(DRAIN_TIMEOUT) => {}
                _ = dc_closed.cancelled() => {}
                _ = &mut output_task => {}
            }
        }
        _ = &mut output_task => { tracing::debug!("forward_stream_to_webrtc: output closed"); }
    }
    Ok(())
}

//=== CliClient struct ===

pub struct CliClient {
    server_url: Option<String>,
    pub token: Option<String>,
    client: Client,
    webrtc_config: WebRTCConfig,
    // DTLS fields
    dtls_connect_addr: Option<String>,
    no_known_hosts_check: bool,
}

impl CliClient {
    pub fn new(
        server_url: Option<String>,
        token: Option<String>,
        ice_servers: Option<Vec<IceServerConfig>>,
        enable_upnp: bool,
        dtls_connect_addr: Option<String>,
        no_known_hosts_check: bool,
    ) -> Self {
        let srv = server_url.clone().unwrap_or_default();
        let tok = token.clone().unwrap_or_default();
        let webrtc_config = WebRTCConfig::new(
            srv, tok, ice_servers.unwrap_or_default(), enable_upnp,
        );
        Self {
            server_url,
            token,
            client: Client::new(),
            webrtc_config,
            dtls_connect_addr,
            no_known_hosts_check,
        }
    }

    //=== HTTP/SSE mode (ProxyCommand) ===

    pub async fn connect_proxy_command(
        &self,
        connect_timeout: Option<u32>,
        agent_id: String,
    ) -> Result<()> {
        let (peer_connection, data_channel) =
            self.create_webrtc_connection_silent(&agent_id).await?;
        if let Err(e) = forward_stream_to_webrtc(
            peer_connection, data_channel, connect_timeout, None,
            tokio::io::stdin(), tokio::io::stdout(),
        ).await {
            tracing::error!("forward_stream_to_webrtc failed: {}", e);
            return Err(e);
        }
        Ok(())
    }

    //=== HTTP/SSE mode (port forwarding) ===

    pub async fn connect_port_forward(&self, agent_id: String, local_port: u16) -> Result<()> {
        info!("Starting port forward from localhost:{} to agent {}", local_port, agent_id);
        let listener = TcpListener::bind(format!("127.0.0.1:{}", local_port)).await?;
        info!("Listening on localhost:{}", local_port);

        loop {
            match listener.accept().await {
                Ok((tcp_stream, addr)) => {
                    info!("New connection from {}", addr);
                    let agent_id = agent_id.clone();
                    let client = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = client.handle_tcp_connection(tcp_stream, agent_id).await {
                            error!("Failed to handle TCP connection: {}", e);
                        }
                    });
                }
                Err(e) => { error!("Failed to accept connection: {}", e); }
            }
        }
    }

    async fn handle_tcp_connection(
        &self,
        mut tcp_stream: TcpStream,
        agent_id: String,
    ) -> Result<()> {
        let (close_tx, close_rx) = tokio::sync::oneshot::channel();
        let max_read_timeout = Duration::from_secs(1800);
        let (peer_connection, data_channel) = match self.create_webrtc_connection(&agent_id).await {
            Ok(res) => res,
            Err(e) => {
                let msg = format!("RPORT_SETUP_ERROR: {}\n", e);
                error!("{}", msg);
                let _ = tcp_stream.write_all(msg.as_bytes()).await;
                let _ = tcp_stream.flush().await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                return Err(e);
            }
        };
        let pc_clone = peer_connection.clone();
        tokio::spawn(async move {
            while let Some(event) = pc_clone.recv().await {
                if let PeerConnectionEvent::DataChannel(dc) = event {
                    tracing::debug!("CliClient PC Event: DataChannel: id={}, label={}", dc.id, dc.label);
                }
            }
        });
        let pc_monitor = peer_connection.clone();
        tokio::spawn(async move {
            let mut state_rx = pc_monitor.subscribe_peer_state();
            while let Ok(()) = state_rx.changed().await {
                let state = *state_rx.borrow();
                match state {
                    rustrtc::PeerConnectionState::Disconnected
                    | rustrtc::PeerConnectionState::Failed
                    | rustrtc::PeerConnectionState::Closed => {
                        if let Some(reason) = pc_monitor.disconnect_reason() {
                            tracing::warn!("WebRTC connection ended: {} (state: {:?})", reason, state);
                        } else { tracing::warn!("WebRTC connection ended: state {:?}", state); }
                        break;
                    }
                    _ => tracing::debug!("Peer connection state: {:?}", state),
                }
            }
        });
        if let Err(_) = tokio::time::timeout(Duration::from_secs(30), peer_connection.wait_for_connected()).await {
            peer_connection.close();
            return Err(anyhow!("WebRTC connection timeout"));
        }
        peer_connection.wait_for_connected().await?;

        let (open_tx, open_rx) = tokio::sync::oneshot::channel();
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel();
        let dc_clone = data_channel.clone();
        tokio::spawn(async move {
            let mut open_tx = Some(open_tx);
            while let Some(event) = dc_clone.recv().await {
                match event {
                    DataChannelEvent::Open => { if let Some(tx) = open_tx.take() { let _ = tx.send(()); } }
                    DataChannelEvent::Message(data) => { let _ = msg_tx.send(data); }
                    DataChannelEvent::Close => { let _ = close_tx.send(()); break; }
                }
            }
        });
        if let Err(_) = tokio::time::timeout(Duration::from_secs(10), open_rx).await {
            peer_connection.close(); let msg = "RPORT_SETUP_ERROR: Data channel open timeout\n".to_string();
            error!("{}", msg); let _ = tcp_stream.write_all(msg.as_bytes()).await;
            let _ = tcp_stream.flush().await; return Err(anyhow!("Data channel open timeout"));
        }
        let (mut tcp_read, mut tcp_write) = tcp_stream.into_split();
        let pc_clone = peer_connection.clone();
        let dc_id = data_channel.id;
        let tcp_to_webrtc = async move {
            let mut buffer = [0u8; 1024];
            loop {
                let r = tokio::time::timeout(max_read_timeout, tcp_read.read(&mut buffer)).await?;
                match r {
                    Ok(0) => { info!("TCP connection closed by client"); break; }
                    Ok(n) => {
                        if let Err(e) = pc_clone.send_data(dc_id, &buffer[..n]).await { error!("Failed to send data through WebRTC: {}", e); break; }
                    }
                    Err(e) => { error!("Failed to read from TCP: {}", e); break; }
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        let webrtc_to_tcp = async move {
            while let Some(data) = msg_rx.recv().await {
                if let Err(e) = tcp_write.write_all(&data).await { error!("Failed to write to TCP: {}", e); break; }
                if let Err(e) = tcp_write.flush().await { error!("Failed to flush TCP: {}", e); break; }
            }
        };
        tokio::select! {
            _ = close_rx => {}
            _ = tcp_to_webrtc => { info!("TCP to WebRTC forwarding ended"); }
            _ = webrtc_to_tcp => { info!("WebRTC to TCP forwarding ended"); }
        }
        peer_connection.close();
        Ok(())
    }

    //=== HTTP/SSE: create WebRTC connection ===

    async fn create_webrtc_connection(
        &self,
        agent_id: &str,
    ) -> Result<(Arc<PeerConnection>, Arc<DataChannel>)> {
        info!("Creating WebRTC peer connection for agent: {}", agent_id);
        let peer_connection = self.create_peer_connection().await?;
        let data_channel_config = DataChannelConfig { ordered: true, ..Default::default() };
        let data_channel = peer_connection.create_data_channel("port-forward", Some(data_channel_config))?;
        let offer = peer_connection.create_offer().await?;
        peer_connection.set_local_description(offer.clone())?;
        if let Err(_) = tokio::time::timeout(Duration::from_secs(3), peer_connection.wait_for_gathering_complete()).await {
            info!("ICE gathering timed out, proceeding with gathered candidates");
        }
        let offer = peer_connection.local_description().ok_or_else(|| anyhow!("Failed to get local description"))?;
        let sdp = offer.to_sdp_string();
        let offer_sdp = sdp.lines().filter(|l| !l.contains("IP6") && !l.contains("::")).collect::<Vec<_>>().join("\r\n");

        let server = self.server_url.as_deref().unwrap_or("");
        let token = self.token.as_deref().unwrap_or("");
        let offer_msg = OfferMessage { id: agent_id.to_string(), offer: offer_sdp };
        info!("Sending offer to signaling server...");
        let url = format!("{}/rport/offer?token={}", server, token);
        let response = self.client.post(&url).json(&offer_msg).send().await?;
        if !response.status().is_success() {
            return Err(anyhow!("Failed to send offer: {}", response.status()));
        }
        let response_body: Value = response.json().await?;
        let answer_sdp = response_body["answer"].as_str().ok_or_else(|| anyhow!("Missing answer in response"))?;
        let answer = SessionDescription::parse(SdpType::Answer, &answer_sdp)?;
        peer_connection.set_remote_description(answer).await?;
        info!("WebRTC handshake completed successfully");
        Ok((peer_connection, data_channel))
    }

    async fn create_webrtc_connection_silent(
        &self,
        agent_id: &str,
    ) -> Result<(Arc<PeerConnection>, Arc<DataChannel>)> {
        let peer_connection = self.create_peer_connection().await?;
        let pc_clone = peer_connection.clone();
        tokio::spawn(async move {
            while let Some(_) = pc_clone.recv().await {}
        });
        let data_channel_config = DataChannelConfig { ordered: true, ..Default::default() };
        let data_channel = peer_connection.create_data_channel("port-forward", Some(data_channel_config))?;
        let offer = peer_connection.create_offer().await?;
        peer_connection.set_local_description(offer.clone())?;
        if let Err(_) = tokio::time::timeout(Duration::from_secs(3), peer_connection.wait_for_gathering_complete()).await {
            info!("ICE gathering timed out, proceeding with gathered candidates");
        }
        let offer = peer_connection.local_description().ok_or_else(|| anyhow!("Failed to get local description"))?;
        let server = self.server_url.as_deref().unwrap_or("");
        let token = self.token.as_deref().unwrap_or("");
        let offer_sdp = offer.to_sdp_string();
        let url = format!("{}/rport/offer?token={}", server, token);
        tracing::debug!("create_webrtc_connection_silent: sending offer to {} \n {}", url, offer_sdp);
        let offer_msg = OfferMessage { id: agent_id.to_string(), offer: offer_sdp };
        let response = self.client.post(&url).timeout(Duration::from_secs(10)).json(&offer_msg).send().await?;
        if !response.status().is_success() {
            return Err(anyhow!("Failed to send offer: {}", response.status()));
        }
        let response_body: Value = response.json().await?;
        let answer_sdp = response_body["answer"].as_str().ok_or_else(|| anyhow!("Missing answer in response"))?;
        let answer = SessionDescription::parse(SdpType::Answer, &answer_sdp)?;
        peer_connection.set_remote_description(answer).await?;
        Ok((peer_connection, data_channel))
    }

    async fn create_peer_connection(&self) -> Result<Arc<PeerConnection>> {
        self.webrtc_config.create_peer_connection().await
    }

    //=== DTLS mode ===

    /// Connect to agent via DTLS, exchange WebRTC offer/answer,
    /// then set up local port forwarding for all targets.
    pub async fn connect_via_dtls(
        &self,
        forward_mappings: &[ForwardMapping],
    ) -> Result<()> {
        let addr = self.dtls_connect_addr.as_deref()
            .ok_or_else(|| anyhow!("DTLS connect address not set"))?;

        // known-hosts check (skip for ProxyCommand)
        if !self.no_known_hosts_check {
            // Connect first to get the fingerprint
            let temp_dtls = DtlsClient::connect(addr, None).await?;
            temp_dtls.close();
            // Simplified: just check known hosts
            let kh = KnownHosts::load();
            match kh.check(addr, "") {
                crate::known_hosts::CheckResult::Match => {}
                crate::known_hosts::CheckResult::Mismatch { expected, actual } => {
                    return Err(anyhow!(
                        "REMOTE HOST IDENTIFICATION HAS CHANGED for {}!\n\
                         Expected fingerprint: {}\nActual fingerprint: {}",
                        addr, expected, actual
                    ));
                }
                crate::known_hosts::CheckResult::Unknown => {
                    // Will be prompted after first handshake
                }
            }
        }

        // Connect DTLS
        let mut dtls_client = DtlsClient::connect(addr, None).await?;

        info!("DTLS session connected");

        // known-hosts: prompt on first connection
        if !self.no_known_hosts_check {
            // For now, we store based on the connect address
            // We'll generate a placeholder fingerprint for known-hosts
            let fp = "verified"; // Placeholder - in real impl, extract from cert
            let mut kh = KnownHosts::load();
            match kh.check(addr, fp) {
                crate::known_hosts::CheckResult::Unknown => {
                    if KnownHosts::prompt_and_confirm(addr, fp) {
                        kh.add(addr, fp);
                        if let Err(e) = kh.save() {
                            warn!("Failed to save known-hosts: {}", e);
                        }
                    } else {
                        dtls_client.close();
                        return Err(anyhow!("Host key verification failed"));
                    }
                }
                crate::known_hosts::CheckResult::Mismatch { expected, actual } => {
                    dtls_client.close();
                    return Err(anyhow!(
                        "REMOTE HOST IDENTIFICATION HAS CHANGED for {}!\n\
                         Expected fingerprint: {}\nActual fingerprint: {}",
                        addr, expected, actual
                    ));
                }
                crate::known_hosts::CheckResult::Match => {}
            }
        }

        // Prepare targets from forward mappings
        let targets: Vec<Target> = forward_mappings.iter().map(|f| Target {
            host: f.remote_host.clone(),
            port: f.remote_port,
        }).collect();

        // Create WebRTC peer connection
        let peer_connection = self.create_peer_connection().await?;

        // Create data channels for each target
        struct DcMapping {
            dc: Arc<DataChannel>,
            local_port: u16,
        }
        let mut dc_mappings = Vec::new();
        for ft in forward_mappings {
            let label = format!("fwd:{}:{}", ft.remote_host.as_deref().unwrap_or("127.0.0.1"), ft.remote_port);
            let dc_config = DataChannelConfig {
                ordered: true,
                label: label.clone(),
                ..Default::default()
            };
            let dc = peer_connection.create_data_channel(&label, Some(dc_config))?;
            dc_mappings.push(DcMapping {
                local_port: ft.local_port,
                dc,
            });
        }

        // Create SDP offer
        let offer = peer_connection.create_offer().await?;
        peer_connection.set_local_description(offer.clone())?;
        if let Err(_) = tokio::time::timeout(Duration::from_secs(3), peer_connection.wait_for_gathering_complete()).await {
            info!("ICE gathering timed out, proceeding with gathered candidates");
        }
        let offer_sdp = peer_connection.local_description()
            .ok_or_else(|| anyhow!("Failed to get local description"))?
            .to_sdp_string();

        // Send offer over DTLS (include token for agent verification)
        dtls_client.send(&SignalingMessage::Offer {
            session_id: "client-1".to_string(),
            token: self.token.clone(),
            offer_sdp,
            targets: Some(targets),
        }).await?;

        // Wait for answer
        loop {
            let msg = dtls_client.recv().await?;
            match msg {
                SignalingMessage::Answer { answer_sdp, .. } => {
                    let answer = SessionDescription::parse(SdpType::Answer, &answer_sdp)?;
                    peer_connection.set_remote_description(answer).await?;
                    info!("WebRTC handshake via DTLS completed");
                    break;
                }
                SignalingMessage::Error { reason, .. } => {
                    dtls_client.close();
                    return Err(anyhow!("Agent rejected offer: {}", reason));
                }
                other => {
                    warn!("Unexpected message during signaling: {:?}", other);
                    continue;
                }
            }
        }

        // WebRTC connected. Start local port forward listeners.
        let peer_connection = peer_connection;
        let all_stats = Arc::new(ForwardStats::default());
        let reporter_label = forward_mappings.iter()
            .map(|f| format!("{}:{}", f.local_port, f.remote_port))
            .collect::<Vec<_>>()
            .join(",");
        spawn_stats_reporter(&reporter_label, all_stats.clone());

        for dm in dc_mappings {
            let pc = peer_connection.clone();
            let dc = dm.dc;
            let local_port = dm.local_port;
            let stats = all_stats.clone();

            tokio::spawn(async move {
                let listener = match TcpListener::bind(format!("127.0.0.1:{}", local_port)).await {
                    Ok(l) => l,
                    Err(e) => {
                        error!("Failed to bind local port {}: {}", local_port, e);
                        return;
                    }
                };
                info!("DTLS forwarding: listening on localhost:{}", local_port);

                loop {
                    match listener.accept().await {
                        Ok((tcp_stream, _addr)) => {
                            let pc = pc.clone();
                            let dc = dc.clone();
                    let (reader, writer) = tcp_stream.into_split();
                    let stats = Some(stats.clone());
                    tokio::spawn(async move {
                            if let Err(e) = forward_stream_to_webrtc(
                                pc.clone(), dc.clone(), Some(30), stats,
                                reader, writer,
                            ).await {
                                tracing::error!("Forwarding error: {}", e);
                            }
                        });
                        }
                        Err(e) => tracing::error!("Accept error: {}", e),
                    }
                }
            });
        }

        // Keep DTLS connection alive for potential future use (candidates, etc.)
        let mut dtls_state = dtls_client.dtls.subscribe_state();
        tokio::spawn(async move {
            while dtls_state.changed().await.is_ok() {
                if matches!(*dtls_state.borrow(), rustrtc::transports::dtls::DtlsState::Closed) {
                    break;
                }
            }
        });

        // Wait forever (or until error)
        std::future::pending::<()>().await;
        Ok(())
    }
}

impl Clone for CliClient {
    fn clone(&self) -> Self {
        Self {
            server_url: self.server_url.clone(),
            token: self.token.clone(),
            client: Client::new(),
            webrtc_config: self.webrtc_config.clone(),
            dtls_connect_addr: self.dtls_connect_addr.clone(),
            no_known_hosts_check: self.no_known_hosts_check,
        }
    }
}

//=== Tests ===

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::OfferMessage;
    use rustrtc::{PeerConnection, RtcConfiguration};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_connect_port_forward_integration() -> Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .try_init();

        let config = RtcConfiguration::default();
        let agent_pc = Arc::new(PeerConnection::new(config));
        agent_pc.add_transceiver(
            rustrtc::MediaKind::Application,
            rustrtc::TransceiverDirection::SendRecv,
        );

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let local_addr = listener.local_addr()?;
        let server_url = format!("http://{}", local_addr);
        let agent_pc_clone = agent_pc.clone();

        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };
                let agent_pc = agent_pc_clone.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let n = match socket.read(&mut buf).await { Ok(n) if n > 0 => n, _ => return };
                    let req = String::from_utf8_lossy(&buf[..n]);

                    if req.contains("GET /rport/iceservers") {
                        let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]";
                        socket.write_all(response.as_bytes()).await.unwrap();
                        return;
                    }
                    if req.contains("POST /rport/offer") {
                        if let Some(idx) = req.find("\r\n\r\n") {
                            let body = &req[idx + 4..];
                            if let Ok(offer_msg) = serde_json::from_str::<OfferMessage>(body) {
                                let offer = SessionDescription::parse(SdpType::Offer, &offer_msg.offer).unwrap();
                                agent_pc.set_remote_description(offer).await.unwrap();
                                let answer = agent_pc.create_answer().await.unwrap();
                                agent_pc.set_local_description(answer.clone()).unwrap();
                                agent_pc.wait_for_gathering_complete().await;
                                let answer = agent_pc.local_description().unwrap();
                                let answer_sdp = answer.to_sdp_string();
                                let response_json = serde_json::json!({
                                    "uuid": uuid::Uuid::new_v4(),
                                    "offer": offer_msg.offer,
                                    "answer": answer_sdp
                                });
                                let response_body = response_json.to_string();
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                    response_body.len(), response_body
                                );
                                socket.write_all(response.as_bytes()).await.unwrap();
                            }
                        }
                    }
                });
            }
        });

        let client = CliClient::new(Some(server_url), Some("test-token".to_string()), None, false, None, true);

        let client_clone = client.clone();
        tokio::spawn(async move {
            if let Err(e) = client_clone.connect_port_forward("gpu03".to_string(), 4023).await {
                eprintln!("connect_port_forward failed: {}", e);
            }
        });

        tokio::time::sleep(Duration::from_secs(1)).await;
        let _stream = TcpStream::connect("127.0.0.1:4023").await?;
        let (dc_tx, dc_rx) = tokio::sync::oneshot::channel();

        let agent_pc_clone = agent_pc.clone();
        tokio::spawn(async move {
            let mut dc_tx = Some(dc_tx);
            while let Some(event) = agent_pc_clone.recv().await {
                if let PeerConnectionEvent::DataChannel(dc) = event {
                    if let Some(tx) = dc_tx.take() { let _ = tx.send(dc); }
                }
            }
        });

        agent_pc.wait_for_connected().await.unwrap();
        let dc = tokio::time::timeout(Duration::from_secs(5), dc_rx).await??;
        let (open_tx, open_rx) = tokio::sync::oneshot::channel();
        let dc_clone = dc.clone();
        tokio::spawn(async move {
            let mut open_tx = Some(open_tx);
            while let Some(event) = dc_clone.recv().await {
                if let DataChannelEvent::Open = event {
                    if let Some(tx) = open_tx.take() { let _ = tx.send(()); }
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(5), open_rx).await??;
        Ok(())
    }
}

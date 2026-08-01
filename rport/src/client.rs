use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use rustrtc::{
    transports::sctp::{DataChannel, DataChannelConfig, DataChannelEvent},
    IceCandidate, IceGatheringState, PeerConnection, PeerConnectionEvent, SdpType, SessionDescription,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};

use crate::config::{ForwardMapping, IceServerConfig, RportConfig};
use crate::dtls_signaling::{send_message, DtlsClient, SignalingMessage, Target};
use crate::reliable::ReliableSession;
use crate::webrtc_config::WebRTCConfig;
use uuid::Uuid;

const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
pub struct ForwardStats {
    pub bytes_sent: AtomicU64,
    pub bytes_recv: AtomicU64,
    pub packets_sent: AtomicU64,
    pub packets_recv: AtomicU64,
}

#[allow(dead_code)]
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
                label, sent, p_sent, sent_kbps, recv, p_recv, recv_kbps, sent, recv,
            );
            prev_sent = sent;
            prev_recv = recv;
        }
    });
}

//=== Generic stream ↔ WebRTC forwarder ===

#[allow(clippy::too_many_arguments)]
pub async fn forward_stream_to_webrtc<R, W>(
    peer_connection: Arc<PeerConnection>,
    data_channel: Arc<DataChannel>,
    connect_timeout: Option<u32>,
    stats: Option<Arc<ForwardStats>>,
    label: String,
    mut input: R,
    mut output: W,
    mut remote_msg_rx: tokio::sync::mpsc::UnboundedReceiver<Bytes>,
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
                DataChannelEvent::Open => {
                    debug!("Local data channel Open event");
                    if let Some(pair) = pc_disc.ice_transport().get_selected_pair() {
                        info!(
                            "ICE selected pair: local {} {:?} {} -> remote {} {:?} {} [nominated={}]",
                            pair.local.address, pair.local.typ, pair.local.transport,
                            pair.remote.address, pair.remote.typ, pair.remote.transport,
                            pair.nominated,
                        );
                    }
                    if let Some(tx) = open_tx.take() { let _ = tx.send(()); }
                }
                DataChannelEvent::Message(data) => {
                    if data.is_empty() {
                        // Zero-length message = EOF marker from agent (remote TCP closed)
                        debug!("dc→client: EOF marker, closing local connection");
                        dc_closed_tx.cancel();
                        break;
                    }
                    let len = data.len();
                    if let Some(ref s) = stats_clone {
                        s.bytes_recv.fetch_add(len as u64, Ordering::Relaxed);
                        s.packets_recv.fetch_add(1, Ordering::Relaxed);
                    }
                    debug!("dc→client: {} bytes", len);
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
    debug!("Waiting for data channel to open (timeout: {}s)...", connect_timeout);
    if let Err(_) = tokio::time::timeout(Duration::from_secs(connect_timeout.into()), open_rx).await {
        return Err(anyhow!("Data channel open timeout"));
    }
    debug!("Data channel is open, starting forwarding");

    // Periodic link-stats logger: every 10s prints SCTP-level throughput,
    // smoothed RTT, retransmission timeout and retransmit count for this
    // connection, plus app-level bytes when a ForwardStats is attached.
    {
        let pc_stats = peer_connection.clone();
        let app_stats = stats.clone();
        let stats_label = label.clone();
        tokio::spawn(async move {
            let mut prev_sent = 0u64;
            let mut prev_recv = 0u64;
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let s = match pc_stats.sctp_link_stats() {
                    Some(s) => s,
                    None => continue,
                };
                let d_sent = s.bytes_sent.saturating_sub(prev_sent);
                let d_recv = s.bytes_received.saturating_sub(prev_recv);
                prev_sent = s.bytes_sent;
                prev_recv = s.bytes_received;
                let up_kbps = d_sent as f64 / 10.0 / 1024.0;
                let dn_kbps = d_recv as f64 / 10.0 / 1024.0;
                let srtt_ms = s.srtt.as_secs_f64() * 1000.0;
                let rto_ms = s.rto.as_secs_f64() * 1000.0;
                let app = app_stats.as_ref().map(|a| {
                    let bs = a.bytes_sent.load(Ordering::Relaxed);
                    let br = a.bytes_recv.load(Ordering::Relaxed);
                    format!(" | app: {}B↑ {}B↓", bs, br)
                }).unwrap_or_default();
                info!(
                    "[link] {} | sctp sent {:.1}KB ({:.1}KB/s) recv {:.1}KB ({:.1}KB/s) | srtt {:.1}ms rto {:.0}ms retrans {} dur {:.0}s{}",
                    stats_label,
                    s.bytes_sent as f64 / 1024.0, up_kbps,
                    s.bytes_received as f64 / 1024.0, dn_kbps,
                    srtt_ms, rto_ms, s.retransmissions, s.duration.as_secs(),
                    app,
                );
            }
        });
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
                Ok(0) => { debug!("forward_stream_to_webrtc: input EOF"); break; }
                Ok(n) => {
                    if let Some(ref s) = stats_input {
                        s.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
                        s.packets_sent.fetch_add(1, Ordering::Relaxed);
                    }
                    debug!("client→dc: {} bytes", n);
                    if let Err(e) = pc_clone.send_data(dc_id, &buffer[..n]).await {
                        error!("Failed to send data through WebRTC: {}", e); break;
                    }
                }
                Err(e) => { debug!("forward_stream_to_webrtc: input read failed: {}", e); break; }
            }
        }
    };

    let mut output_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                data = msg_rx.recv() => {
                    match data {
                        Some(data) => {
                            debug!("output_write: {} bytes from local dc", data.len());
                            if output.write_all(&data).await.is_err() { break; }
                            if output.flush().await.is_err() { break; }
                        }
                        None => break,
                    }
                }
                data = remote_msg_rx.recv() => {
                    match data {
                        Some(data) => {
                            debug!("output_write: {} bytes from remote dc", data.len());
                            if output.write_all(&data).await.is_err() { break; }
                            if output.flush().await.is_err() { break; }
                        }
                        None => break,
                    }
                }
            }
        }
    });

    tokio::select! {
        _ = webrtc_dead.cancelled() => { tracing::debug!("forward_stream_to_webrtc: exiting due to WebRTC disconnect"); }
        _ = dc_closed.cancelled() => { tracing::debug!("forward_stream_to_webrtc: data channel closed by remote"); }
        _ = input_task => {
            // Check if WebRTC is already dead — distinguish clean local EOF
            // from a remote disconnect that caused stdin to close.
            let pc_state = *peer_connection.subscribe_peer_state().borrow();
            if matches!(pc_state, rustrtc::PeerConnectionState::Failed | rustrtc::PeerConnectionState::Closed)
                || dc_closed.is_cancelled()
            {
                let reason = peer_connection.disconnect_reason()
                    .map(|r| format!(" ({})", r)).unwrap_or_default();
                tracing::warn!("Connection lost: remote WebRTC disconnected{} — session terminated", reason);
            } else {
                tracing::debug!("forward_stream_to_webrtc: input closed, waiting for drain");
            }
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

//=== CliClient ===

pub struct CliClient {
    server_url: String,
    token: String,
    agent_id: String,
    webrtc_config: WebRTCConfig,
    wait_candidates: bool,
}

impl CliClient {
    pub fn new(
        server_url: &str,
        token: &str,
        agent_id: &str,
        ice_servers: Option<Vec<IceServerConfig>>,
        enable_upnp: bool,
        wait_candidates: bool,
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
            agent_id: agent_id.to_string(),
            webrtc_config,
            wait_candidates,
        }
    }

    //=== ProxyCommand mode ===

    pub async fn connect_proxy_command(
        &self,
        connect_timeout: Option<u32>,
        target_host: &str,
        target_port: u16,
    ) -> Result<()> {
        info!("ProxyCommand: agent '{}' target {}:{}", self.agent_id, target_host, target_port);
        let (pc, dc, remote_rx) = self.establish_webrtc(target_host, target_port).await?;
        forward_stream_to_webrtc(
            pc, dc, connect_timeout, None,
            format!("proxy -> {}:{}", target_host, target_port),
            tokio::io::stdin(), tokio::io::stdout(), remote_rx,
        ).await
    }

    //=== Port forward mode ===

    pub async fn connect_port_forwards(
        &self,
        connect_timeout: Option<u32>,
        forwards: &[ForwardMapping],
    ) -> Result<()> {
        let all_stats = Arc::new(ForwardStats::default());

        for fwd in forwards {
            let local_port = fwd.local_port.ok_or_else(|| {
                anyhow!("Port forward requires local port in -L spec")
            })?;
            let host = fwd.host.clone();
            let port = fwd.port;
            let webrtc_config = self.webrtc_config.clone();
            let srv = self.server_url.clone();
            let tok = self.token.clone();
            let agent_id = self.agent_id.clone();
            let stats = all_stats.clone();
            let timeout = connect_timeout;
            let wait_candidates = self.wait_candidates;

            tokio::spawn(async move {
                let listener = match TcpListener::bind(format!("127.0.0.1:{}", local_port)).await {
                    Ok(l) => l,
                    Err(e) => {
                        error!("Failed to bind local port {}: {}", local_port, e);
                        return;
                    }
                };
                info!("Port forward: listening on localhost:{} -> agent '{}' -> {}:{}",
                      local_port, agent_id, host, port);

                loop {
                    match listener.accept().await {
                        Ok((tcp_stream, addr)) => {
                            info!("New connection from {}", addr);
                            // Disable Nagle: interactive protocols (SSH) send
                            // small packets that must not be delayed by Nagle's
                            // algorithm + delayed-ACK, which can stall auth.
                            let _ = tcp_stream.set_nodelay(true);
                            let (reader, writer) = tcp_stream.into_split();
                            let client = CliClient {
                                server_url: srv.clone(),
                                token: tok.clone(),
                                agent_id: agent_id.clone(),
                                webrtc_config: webrtc_config.clone(),
                                wait_candidates,
                            };
                            let stats = stats.clone();
                            let host_clone = host.clone();
                            tokio::spawn(async move {
                                match client.establish_webrtc(&host_clone, port).await {
                                    Ok((pc, dc, remote_rx)) => {
                                        if let Err(e) = forward_stream_to_webrtc(
                                            pc, dc, timeout, Some(stats),
                                            format!("localhost:{} -> {}:{}", local_port, host_clone, port),
                                            reader, writer, remote_rx,
                                        ).await {
                                            error!("Forwarding error: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to establish WebRTC: {}", e);
                                    }
                                }
                            });
                        }
                        Err(e) => error!("Accept error: {}", e),
                    }
                }
            });
        }

        // Wait forever
        std::future::pending::<()>().await;
        Ok(())
    }

    /// Connect to DTLS, send GetIceServers, receive response.
    /// Returns `Ok(Some((client, servers)))` on success.
    /// Returns `Ok(None)` on timeout/old server — client is **dead**.
    /// The caller should close it and reconnect without GetIceServers.
    pub async fn connect_dtls_and_get_ice_servers(
        server_url: &str,
    ) -> Result<Option<(DtlsClient, Vec<IceServerConfig>)>> {
        let mut client = DtlsClient::connect(server_url, None).await?;
        let _ = client.send(&SignalingMessage::GetIceServers).await;

        match tokio::time::timeout(Duration::from_secs(3), client.recv()).await {
            Ok(Ok(SignalingMessage::IceServers { ice_servers })) => {
                info!("Received ICE server config ({} servers) from signaling server", ice_servers.len());
                let servers = ice_servers.into_iter().map(|s| IceServerConfig {
                    urls: s.urls,
                    username: s.username,
                    credential: s.credential,
                }).collect();
                Ok(Some((client, servers)))
            }
            Ok(Ok(other)) => {
                warn!("Unexpected response to GetIceServers: {:?}, using defaults", other);
                client.close();
                Ok(None)
            }
            Ok(Err(e)) => {
                warn!("Error receiving ICE servers: {}, using defaults", e);
                client.close();
                Ok(None)
            }
            Err(_) => {
                info!("GetIceServers timed out (old server), will reconnect");
                client.close();
                Ok(None)
            }
        }
    }

    /// Connect to DTLS server, create WebRTC offer, exchange signaling, return PC + DC
    pub async fn establish_webrtc(
        &self,
        target_host: &str,
        target_port: u16,
    ) -> Result<(Arc<PeerConnection>, Arc<DataChannel>, tokio::sync::mpsc::UnboundedReceiver<Bytes>)> {
        // Try GetIceServers. On timeout/error the DTLS connection is dead
        // (old server dropped us), so reconnect without it.
        let (mut dtls_client, extra_ice_servers) =
            match Self::connect_dtls_and_get_ice_servers(&self.server_url).await {
                Ok(Some((client, servers))) => {
                    info!("Connected with ICE server config from signaling server");
                    (client, servers)
                }
                _ => {
                    info!("Connecting without ICE server config (old server or fallback)");
                    let client = DtlsClient::connect(&self.server_url, None).await?;
                    info!("DTLS connected (fallback)");
                    (client, vec![])
                }
            };

        let peer_connection = self.webrtc_config.create_peer_connection_with(&extra_ice_servers).await?;

        let label = format!("fwd:{}:{}", target_host, target_port);
        let dc_config = DataChannelConfig {
            ordered: true,
            label: label.clone(),
            ..Default::default()
        };
        let data_channel = peer_connection.create_data_channel(&label, Some(dc_config))?;

        let (remote_msg_tx, remote_msg_rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();

        let pc_drain = peer_connection.clone();
        let rt = remote_msg_tx.clone();
        let drain_label = label.clone();
        tokio::spawn(async move {
            while let Some(event) = pc_drain.recv().await {
                if let PeerConnectionEvent::DataChannel(dc) = event {
                    let label = dc.label.clone();
                    info!("Received remote data channel from agent: {}", label);
                    let tx = rt.clone();
                    tokio::spawn(async move {
                        while let Some(event) = dc.recv().await {
                            match event {
                                DataChannelEvent::Message(data) => {
                                    let _ = tx.send(data);
                                }
                                DataChannelEvent::Close => {
                                    debug!("Remote DC '{}' closed by agent", label);
                                    break;
                                }
                                other => {
                                    debug!("Remote DC '{}' event: {:?}", label, other);
                                }
                            }
                        }
                    });
                } else {
                    debug!("Peer connection event (non-DC) for '{}'", drain_label);
                }
            }
        });

        let session_id = Uuid::new_v4().to_string();

        // Reliable session for seq/ack exchange with agent
        let reliable = Arc::new(Mutex::new(ReliableSession::new(
            Duration::from_millis(2000),
            3,
        )));

        // Subscribe to ICE candidate / gathering events BEFORE
        // set_local_description. Host candidates are gathered almost
        // instantly once gathering begins; the broadcast channel does not
        // replay candidates emitted before a subscription exists, so
        // subscribing after set_local_description would silently drop host
        // candidates and break same-LAN / localhost connectivity. Candidates
        // are buffered in the receiver until the forwarding task polls them.
        let mut candidate_rx = peer_connection.subscribe_ice_candidates();
        let mut gathering_state_rx = peer_connection.subscribe_ice_gathering_state();
        let dtls = dtls_client.dtls.clone();
        let rel = reliable.clone();
        let sid = session_id.clone();

        let offer = peer_connection.create_offer().await?;
        peer_connection.set_local_description(offer)?;

        // Wait for all ICE candidates before sending offer so that
        // old HTTP/SSE agents (which ignore trickle ICE) get all candidates in the SDP
        if self.wait_candidates {
            info!("Waiting for ICE gathering to complete (--wait-candidates)");
            peer_connection.wait_for_gathering_complete().await;
        }

        let offer_sdp = peer_connection.local_description()
            .ok_or_else(|| anyhow!("Failed to get local description"))?
            .to_sdp_string();

        info!("Sending offer for agent '{}' session {}", self.agent_id, session_id);
        dtls_client.send(&SignalingMessage::new_offer(
            session_id.clone(),
            self.agent_id.clone(),
            offer_sdp,
            Some(vec![Target {
                host: Some(target_host.to_string()),
                port: target_port,
            }]),
        )).await?;

        // Trickle ICE: forward gathered candidates as they arrive
        tokio::spawn(async move {
            if *gathering_state_rx.borrow() == IceGatheringState::Complete {
                let mut msg = SignalingMessage::new_end_of_candidates(sid.clone());
                rel.lock().unwrap().prepare_send(&mut msg, false);
                let _ = send_message(&dtls, &msg).await;
                return;
            }
            loop {
                tokio::select! {
                    result = candidate_rx.recv() => {
                        match result {
                            Ok(candidate) => {
                                info!(
                                    "Local ICE candidate: {} {} {} {:?}",
                                    candidate.address, candidate.transport, candidate.priority, candidate.typ,
                                );
                                let mut msg = SignalingMessage::new_candidate(
                                    sid.clone(),
                                    candidate.to_sdp(),
                                );
                                rel.lock().unwrap().prepare_send(&mut msg, false);
                                let _ = send_message(&dtls, &msg).await;
                                if *gathering_state_rx.borrow() == IceGatheringState::Complete {
                                    let mut msg = SignalingMessage::new_end_of_candidates(sid.clone());
                                    rel.lock().unwrap().prepare_send(&mut msg, false);
                                    let _ = send_message(&dtls, &msg).await;
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    _ = gathering_state_rx.changed() => {
                        if *gathering_state_rx.borrow() == IceGatheringState::Complete {
                            let mut msg = SignalingMessage::new_end_of_candidates(sid.clone());
                            rel.lock().unwrap().prepare_send(&mut msg, false);
                            let _ = send_message(&dtls, &msg).await;
                            break;
                        }
                    }
                }
            }
        });

        // Monitor selected ICE candidate pair — fires when a pair is nominated.
        {
            let mut pair_rx = peer_connection.ice_transport().subscribe_selected_pair();
            tokio::spawn(async move {
                while pair_rx.changed().await.is_ok() {
                    if let Some(ref pair) = *pair_rx.borrow() {
                        info!(
                            "ICE pair nominated: local {} {:?} {} -> remote {} {:?} {} [nominated={}]",
                            pair.local.address, pair.local.typ, pair.local.transport,
                            pair.remote.address, pair.remote.typ, pair.remote.transport,
                            pair.nominated,
                        );
                    }
                }
            });
        }

        // Subscribe to PeerConnection state so we can detect Connected and
        // break the answer loop as soon as ICE completes (instead of waiting
        // for EndOfCandidates from the agent).  Combined with the rustrtc fix
        // (no premature IceTransportState::Failed when the first batch of
        // connectivity checks has no successful pairs), trickle candidates
        // arriving after the answer will trigger new checks and ICE will
        // eventually connect.
        let mut pc_state_rx = peer_connection.subscribe_peer_state();
        let mut answer_received = false;
        let mut received_end_of_candidates = false;

        loop {
            tokio::select! {
                msg = dtls_client.recv() => {
                    let msg = match msg {
                        Ok(m) => m,
                        Err(e) => {
                            debug!("Signaling connection closed during candidate gathering: {}", e);
                            break;
                        }
                    };
                    reliable.lock().unwrap().process_recv(&msg);
                    match msg {
                        SignalingMessage::Answer { answer_sdp, .. } => {
                            if answer_received {
                                debug!("Duplicate answer (agent retransmit), ignoring");
                            } else {
                                info!("Received answer from agent, setting remote description");
                                let answer = SessionDescription::parse(SdpType::Answer, &answer_sdp)?;
                                peer_connection.set_remote_description(answer).await?;
                                info!("WebRTC handshake completed for session {}", session_id);
                                answer_received = true;
                                // ACK the answer promptly so the agent stops
                                // retransmitting it while we wait for ICE to
                                // connect.  Lock is dropped before .await.
                                let ack_seq = reliable.lock().unwrap().last_recv_seq();
                                if let Some(seq) = ack_seq {
                                    let mut ack_msg = SignalingMessage::new_ack(session_id.clone(), seq);
                                    {
                                        let mut guard = reliable.lock().unwrap();
                                        guard.prepare_send(&mut ack_msg, false);
                                    }
                                    let _ = dtls_client.send(&ack_msg).await;
                                }
                            }
                        }
                        SignalingMessage::Candidate { candidate, .. } => {
                            if candidate.len() > 80 {
                                debug!("Received ICE candidate (truncated): {}...", &candidate[..80]);
                            } else {
                                debug!("Received ICE candidate: {}", candidate);
                            }
                            if let Ok(c) = IceCandidate::from_sdp(&candidate) {
                                peer_connection.add_ice_candidate(c).ok();
                            }
                        }
                        SignalingMessage::EndOfCandidates { .. } => {
                            debug!("Received end-of-candidates from agent");
                            if answer_received {
                                break;
                            }
                            received_end_of_candidates = true;
                        }
                        SignalingMessage::Error { reason, .. } => {
                            dtls_client.close();
                            return Err(anyhow!("Agent rejected offer: {}", reason));
                        }
                        other => {
                            debug!("Unexpected message during signaling: {:?}", other);
                            continue;
                        }
                    }
                    if answer_received && received_end_of_candidates {
                        break;
                    }
                }
                // ICE has connected — data channel is about to open
                _ = pc_state_rx.changed() => {
                    let state = *pc_state_rx.borrow_and_update();
                    match state {
                        rustrtc::PeerConnectionState::Connected => {
                            info!("WebRTC connected");
                            break;
                        }
                        rustrtc::PeerConnectionState::Failed => {
                            let reason = peer_connection.disconnect_reason()
                                .map(|r| r.to_string()).unwrap_or_default();
                            return Err(anyhow!("WebRTC failed: {}", reason));
                        }
                        rustrtc::PeerConnectionState::Closed => {
                            return Err(anyhow!("WebRTC closed"));
                        }
                        _ => {
                            debug!("PeerConnection state: {:?}", state);
                        }
                    }
                }
                // Long timeout: break even if ICE never connects
                _ = tokio::time::sleep(Duration::from_secs(30)) => {
                    if answer_received {
                        warn!("Timeout waiting for ICE to connect, proceeding");
                        break;
                    }
                    if received_end_of_candidates {
                        return Err(anyhow!("Timeout waiting for answer from agent"));
                    }
                }
            }
        }

        // Send ack for last received seq from agent so it stops retransmitting.
        // Lock is dropped before .await to avoid MutexGuard !Send issues.
        let ack_seq = reliable.lock().unwrap().last_recv_seq();
        if let Some(seq) = ack_seq {
            let mut ack_msg = SignalingMessage::new_ack(session_id.clone(), seq);
            {
                let mut guard = reliable.lock().unwrap();
                guard.prepare_send(&mut ack_msg, false);
            }
            let _ = dtls_client.send(&ack_msg).await;
        }

        Ok((peer_connection, data_channel, remote_msg_rx))
    }
}

impl Clone for CliClient {
    fn clone(&self) -> Self {
        Self {
            server_url: self.server_url.clone(),
            token: self.token.clone(),
            agent_id: self.agent_id.clone(),
            webrtc_config: self.webrtc_config.clone(),
            wait_candidates: self.wait_candidates,
        }
    }
}

use std::collections::HashMap;
use std::net::SocketAddr;
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
use tokio::net::{TcpListener, UdpSocket};
use tracing::{debug, error, info, warn};

use crate::config::{ForwardMapping, IceServerConfig, Protocol, RportConfig};
use crate::dtls_signaling::{send_message, DtlsClient, SignalingMessage, Target};
use crate::reliable::ReliableSession;
use crate::webrtc_config::WebRTCConfig;
use uuid::Uuid;

const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Idle time before an inactive UDP session is torn down (and its WebRTC
/// peer connection released). A datagram arriving after the timeout simply
/// establishes a fresh session, mirroring NAT UDP mapping timeouts.
const UDP_SESSION_IDLE: Duration = Duration::from_secs(60);

/// Build the data-channel config for a forwarding session. UDP sessions use
/// an unordered, zero-retransmit (PR-SCTP) channel so a lost packet behaves
/// like real UDP (dropped) instead of stalling every later datagram behind
/// it (head-of-line blocking). SCTP preserves message boundaries, so each
/// datagram maps to exactly one DataChannel message.
pub fn data_channel_config_for(label: &str, proto: Protocol) -> DataChannelConfig {
    let mut config = DataChannelConfig {
        ordered: true,
        label: label.to_string(),
        ..Default::default()
    };
    if proto == Protocol::Udp {
        config.ordered = false;
        config.max_retransmits = Some(0);
    }
    config
}

pub fn dc_label_for(host: &str, port: u16, proto: Protocol) -> String {
    match proto {
        Protocol::Tcp => format!("fwd:{}:{}", host, port),
        Protocol::Udp => format!("fwd:udp:{}:{}", host, port),
    }
}

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
    _label: String,
    mut input: R,
    mut output: W,
    mut remote_msg_rx: tokio::sync::mpsc::UnboundedReceiver<Bytes>,
    signaling_done: tokio_util::sync::CancellationToken,
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
                    let _ = msg_tx.send(data);
                }
                DataChannelEvent::BufferedAmountLow(_) => {
                    // Flow-control hint (W3C bufferedamountlow); nothing to do here.
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
        signaling_done.cancel();
        return Err(anyhow!("Data channel open timeout"));
    }
    debug!("Data channel is open, starting forwarding");

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
                Ok(0) => {
                    debug!("forward_stream_to_webrtc: input EOF");
                    // Symmetric EOF marker (mirror of the agent's): tells the
                    // agent to close the target connection and reap the
                    // session immediately instead of waiting for the ICE
                    // disconnect threshold (~45s) to notice we are gone.
                    let _ = pc_clone.send_data(dc_id, &[]).await;
                    break;
                }
                Ok(n) => {
                    if let Some(ref s) = stats_input {
                        s.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
                        s.packets_sent.fetch_add(1, Ordering::Relaxed);
                    }
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
                            if output.write_all(&data).await.is_err() { break; }
                            if output.flush().await.is_err() { break; }
                        }
                        None => break,
                    }
                }
                data = remote_msg_rx.recv() => {
                    match data {
                        Some(data) => {
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
    // Session is over — stop answering signaling keepalives so the server
    // reclaims the DTLS session instead of keeping it alive forever.
    signaling_done.cancel();
    // The event-drain and state-monitor tasks hold Arc clones of the peer
    // connection; without an explicit close they would keep its ICE sockets
    // (and their file descriptors) alive forever in a long-running client.
    peer_connection.close();
    Ok(())
}

//=== UDP session: datagrams ↔ WebRTC data channel ===

/// Forward one UDP "session": datagrams arriving on `input_rx` (from the
/// local UDP socket, all from one source address) go out over the data
/// channel; data-channel messages are delivered back to the same source
/// address. Each datagram maps to exactly one data-channel message.
///
/// The session ends on: idle timeout (no traffic in either direction for
/// `UDP_SESSION_IDLE`), data-channel close (or a zero-length EOF marker from
/// the agent, e.g. when it could not reach the UDP target), or WebRTC
/// failure. `#[allow(clippy::too_many_arguments)]` mirrors the TCP variant.
#[allow(clippy::too_many_arguments)]
pub async fn forward_udp_session(
    peer_connection: Arc<PeerConnection>,
    data_channel: Arc<DataChannel>,
    connect_timeout: Option<u32>,
    stats: Option<Arc<ForwardStats>>,
    label: String,
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<Bytes>,
    signaling_done: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let (open_tx, open_rx) = tokio::sync::oneshot::channel();
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    let dc_closed = tokio_util::sync::CancellationToken::new();

    // Drain data-channel events: replies from the agent, EOF markers and
    // channel close.
    let dc_clone = data_channel.clone();
    let dc_closed_tx = dc_closed.clone();
    let stats_dc = stats.clone();
    tokio::spawn(async move {
        let mut open_tx = Some(open_tx);
        while let Some(event) = dc_clone.recv().await {
            match event {
                DataChannelEvent::Open => {
                    debug!("UDP data channel Open event");
                    if let Some(tx) = open_tx.take() { let _ = tx.send(()); }
                }
                DataChannelEvent::Message(data) => {
                    if let Some(ref s) = stats_dc {
                        s.bytes_recv.fetch_add(data.len() as u64, Ordering::Relaxed);
                        s.packets_recv.fetch_add(1, Ordering::Relaxed);
                    }
                    let _ = msg_tx.send(data);
                }
                DataChannelEvent::BufferedAmountLow(_) => {}
                DataChannelEvent::Close => {
                    dc_closed_tx.cancel();
                    break;
                }
            }
        }
    });

    let connect_timeout = connect_timeout.unwrap_or(30);
    if let Err(_) = tokio::time::timeout(
        Duration::from_secs(connect_timeout.into()),
        open_rx,
    ).await {
        signaling_done.cancel();
        peer_connection.close();
        return Err(anyhow!("Data channel open timeout"));
    }
    debug!("UDP data channel is open, starting forwarding ({})", label);

    // WebRTC failure monitor (same policy as the TCP forwarder).
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
                    webrtc_dead_tx.cancel(); return;
                }
                rustrtc::PeerConnectionState::Disconnected => {
                    let deadline = tokio::time::Instant::now() + DISCONNECT_GRACE;
                    let mut recovered = false;
                    loop {
                        tokio::select! {
                            changed = state_rx.changed() => {
                                if changed.is_err() { break; }
                                if matches!(*state_rx.borrow(), rustrtc::PeerConnectionState::Connected) {
                                    recovered = true; break;
                                }
                            }
                            _ = tokio::time::sleep_until(deadline) => { break; }
                        }
                    }
                    if !recovered {
                        webrtc_dead_tx.cancel(); return;
                    }
                }
                _ => {}
            }
        }
    });

    let pc_send = peer_connection.clone();
    let dc_id = data_channel.id;
    let stats_out = stats.clone();
    let mut closed_by_remote = false;
    let result = loop {
        tokio::select! {
            // Session idle: nothing flowed in either direction recently.
            _ = tokio::time::sleep(UDP_SESSION_IDLE) => {
                debug!("UDP session idle ({}), closing: {}", UDP_SESSION_IDLE.as_secs(), label);
                break Ok(());
            }
            _ = webrtc_dead.cancelled() => {
                debug!("forward_udp_session: WebRTC disconnected");
                break Ok(());
            }
            _ = dc_closed.cancelled() => {
                debug!("forward_udp_session: data channel closed");
                break Ok(());
            }
            datagram = input_rx.recv() => {
                match datagram {
                    Some(data) => {
                        // Empty datagrams carry no meaning on this path (the
                        // zero-length message is reserved for EOF).
                        if data.is_empty() { continue; }
                        if let Some(ref s) = stats_out {
                            s.bytes_sent.fetch_add(data.len() as u64, Ordering::Relaxed);
                            s.packets_sent.fetch_add(1, Ordering::Relaxed);
                        }
                        if let Err(e) = pc_send.send_data(dc_id, &data).await {
                            debug!("forward_udp_session: send failed: {}", e);
                            break Ok(());
                        }
                    }
                    // Local listener shut down.
                    None => break Ok(()),
                }
            }
            data = msg_rx.recv() => {
                match data {
                    Some(data) => {
                        // Zero-length message = EOF marker from the agent
                        // (e.g. UDP target unreachable).
                        if data.is_empty() {
                            debug!("UDP EOF marker from agent, closing session");
                            closed_by_remote = true;
                            break Ok(());
                        }
                        if let Err(e) = sock.send_to(&data, peer).await {
                            debug!("UDP send_to {} failed: {}", peer, e);
                            break Ok(());
                        }
                    }
                    None => break Ok(()),
                }
            }
        }
    };

    if closed_by_remote {
        debug!("UDP session ended (agent closed): {}", label);
    }
    signaling_done.cancel();
    peer_connection.close();
    result
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
        let (pc, dc, remote_rx, signaling_done) = self
            .establish_webrtc(target_host, target_port, Protocol::Tcp)
            .await?;
        forward_stream_to_webrtc(
            pc, dc, connect_timeout, None,
            format!("proxy -> {}:{}", target_host, target_port),
            tokio::io::stdin(), tokio::io::stdout(), remote_rx, signaling_done,
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
            match fwd.proto {
                Protocol::Tcp => {
                    self.spawn_tcp_forward(fwd, connect_timeout, all_stats.clone());
                }
                Protocol::Udp => {
                    self.spawn_udp_forward(fwd, connect_timeout, all_stats.clone());
                }
            }
        }

        // Wait forever
        std::future::pending::<()>().await;
        Ok(())
    }

    /// TCP port forward: one WebRTC session per accepted TCP connection.
    fn spawn_tcp_forward(
        &self,
        fwd: &ForwardMapping,
        connect_timeout: Option<u32>,
        all_stats: Arc<ForwardStats>,
    ) {
        let local_port = fwd.local_port.unwrap_or_else(|| {
            // spawn_* helpers are only called with port-forward mappings
            unreachable!("TCP port forward requires a local port")
        });
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
            // Bind loopback on both address families when available — SSH's
            // `-L` listens on 127.0.0.1 AND ::1, and tools that resolve
            // `localhost` (spice-gtk on macOS prefers ::1) must reach us on
            // either stack.
            let mut listeners = Vec::new();
            for addr in [("127.0.0.1", local_port), ("::1", local_port)] {
                match TcpListener::bind(addr).await {
                    Ok(l) => listeners.push(l),
                    Err(e) => {
                        // IPv4 failure is fatal; IPv6 may simply be unavailable.
                        if addr.0 == "127.0.0.1" {
                            error!("Failed to bind local port {}: {}", local_port, e);
                            return;
                        }
                        debug!("IPv6 loopback unavailable on port {}: {}", local_port, e);
                    }
                }
            }
            info!("TCP forward: listening on [localhost]:{} ({} stack(s)) -> agent '{}' -> {}:{}",
                  local_port, listeners.len(), agent_id, host, port);

            for listener in listeners {
                let webrtc_config = webrtc_config.clone();
                let srv = srv.clone();
                let tok = tok.clone();
                let agent_id = agent_id.clone();
                let stats = stats.clone();
                let host = host.clone();
                tokio::spawn(async move {
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
                                    match client.establish_webrtc(&host_clone, port, Protocol::Tcp).await {
                                        Ok((pc, dc, remote_rx, signaling_done)) => {
                                            if let Err(e) = forward_stream_to_webrtc(
                                                pc, dc, timeout, Some(stats),
                                                format!("localhost:{} -> {}:{}", local_port, host_clone, port),
                                                reader, writer, remote_rx, signaling_done,
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
        });
    }

    /// UDP port forward: datagrams from each source address are multiplexed
    /// onto its own WebRTC session (NAT-like per-peer mapping). Sessions are
    /// created on the first datagram from a source and torn down after an
    /// idle period, so quiet clients do not hold ICE sockets forever.
    fn spawn_udp_forward(
        &self,
        fwd: &ForwardMapping,
        connect_timeout: Option<u32>,
        all_stats: Arc<ForwardStats>,
    ) {
        let local_port = fwd.local_port.unwrap_or_else(|| {
            unreachable!("UDP port forward requires a local port")
        });
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
            // Mirror spawn_tcp_forward: bind loopback on both address
            // families when available so `localhost` resolves either way.
            let mut socks = Vec::new();
            for addr in [("127.0.0.1", local_port), ("::1", local_port)] {
                match UdpSocket::bind(addr).await {
                    Ok(s) => socks.push(Arc::new(s)),
                    Err(e) => {
                        if addr.0 == "127.0.0.1" {
                            error!("Failed to bind local UDP port {}: {}", local_port, e);
                            return;
                        }
                        debug!("IPv6 loopback unavailable for UDP port {}: {}", local_port, e);
                    }
                }
            }
            info!("UDP forward: listening on [localhost]:{} ({} stack(s)) -> agent '{}' -> udp {}:{}",
                  local_port, socks.len(), agent_id, host, port);
            for sock in socks {
                let webrtc_config = webrtc_config.clone();
                let srv = srv.clone();
                let tok = tok.clone();
                let agent_id = agent_id.clone();
                let stats = stats.clone();
                let host = host.clone();
                tokio::spawn(async move {
                    Self::run_udp_listener(
                        sock, local_port, host, port, agent_id,
                        webrtc_config, srv, tok, wait_candidates, timeout, stats,
                    ).await;
                });
            }
        });
    }

    /// Accept loop for one UDP listen socket: datagrams from each source
    /// address are multiplexed onto their own WebRTC session (NAT-like
    /// per-peer mapping). Sessions are created on the first datagram from a
    /// source and torn down after an idle period, so quiet clients do not
    /// hold ICE sockets forever.
    #[allow(clippy::too_many_arguments)]
    async fn run_udp_listener(
        sock: Arc<UdpSocket>,
        local_port: u16,
        host: String,
        port: u16,
        agent_id: String,
        webrtc_config: WebRTCConfig,
        srv: String,
        tok: String,
        wait_candidates: bool,
        timeout: Option<u32>,
        stats: Arc<ForwardStats>,
    ) {
        // src address -> outbound datagram queue for its session
        let mut sessions: HashMap<SocketAddr, tokio::sync::mpsc::UnboundedSender<Bytes>> =
            HashMap::new();
        let mut buf = vec![0u8; 65535];

            loop {
                let (n, src) = match sock.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(e) => {
                        error!("UDP accept error: {}", e);
                        continue;
                    }
                };
                let datagram = Bytes::copy_from_slice(&buf[..n]);

                // Reuse the live session for this source; a send error means
                // the session task has exited (idle/remote close/WebRTC
                // failure) and a fresh one must be established.
                let reuse = match sessions.get(&src) {
                    Some(tx) if !tx.is_closed() => tx.send(datagram.clone()).is_ok(),
                    _ => false,
                };
                if reuse {
                    continue;
                }
                sessions.remove(&src);

                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
                let _ = tx.send(datagram);
                sessions.insert(src, tx.clone());

                let client = CliClient {
                    server_url: srv.clone(),
                    token: tok.clone(),
                    agent_id: agent_id.clone(),
                    webrtc_config: webrtc_config.clone(),
                    wait_candidates,
                };
                let stats = stats.clone();
                let sock = sock.clone();
                let host = host.clone();
                let label = format!("localhost:{} -> udp {}:{} [{}]",
                                    local_port, host, port, src);
                tokio::spawn(async move {
                    match client.establish_webrtc(&host, port, Protocol::Udp).await {
                        Ok((pc, dc, remote_rx, signaling_done)) => {
                            // Merge datagrams queued before the session was
                            // ready with any that arrive while it is up, plus
                            // messages from remote-created channels.
                            let (merged_tx, merged_rx) =
                                tokio::sync::mpsc::unbounded_channel::<Bytes>();
                            let pump_in = tokio::spawn(async move {
                                let mut rx = rx;
                                let mut remote_rx = remote_rx;
                                loop {
                                    tokio::select! {
                                        d = rx.recv() => match d {
                                            Some(d) => { if merged_tx.send(d).is_err() { break; } }
                                            None => { merged_tx.send(Bytes::new()).ok(); break; }
                                        },
                                        d = remote_rx.recv() => match d {
                                            Some(d) => { if merged_tx.send(d).is_err() { break; } }
                                            None => break,
                                        },
                                    }
                                }
                            });
                            if let Err(e) = forward_udp_session(
                                pc, dc, timeout, Some(stats), label.clone(),
                                sock, src, merged_rx, signaling_done,
                            ).await {
                                debug!("UDP session error ({}): {}", label, e);
                            }
                            pump_in.abort();
                        }
                        Err(e) => {
                            error!("Failed to establish UDP WebRTC session ({}): {}", label, e);
                        }
                    }
                });
            }
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
        proto: Protocol,
    ) -> Result<(
        Arc<PeerConnection>,
        Arc<DataChannel>,
        tokio::sync::mpsc::UnboundedReceiver<Bytes>,
        tokio_util::sync::CancellationToken,
    )> {
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

        let label = dc_label_for(target_host, target_port, proto);
        let dc_config = data_channel_config_for(&label, proto);
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
        // old HTTP/SSE agents (which ignore trickle ICE) get all candidates in the SDP.
        // Bounded: a slow/hung UPnP or STUN gather must not block the offer
        // forever — the trickle loop below still forwards candidates as they
        // arrive, so ICE can complete after the offer goes out.
        if self.wait_candidates {
            info!("Waiting for ICE gathering to complete (--wait-candidates)");
            if tokio::time::timeout(Duration::from_secs(5), peer_connection.wait_for_gathering_complete()).await.is_err() {
                warn!("ICE gathering not complete after 5s, sending offer anyway (trickle will deliver remaining candidates)");
            }
        } else if self.webrtc_config.enable_upnp {
            // UPnP gathering runs after STUN/TURN in rustrtc's gather(), so
            // without waiting the relay candidate is ready first and the
            // direct UPnP srflx path loses the ICE race. Give the UPnP
            // candidate a short bounded head-start (300ms) so the direct
            // path can win when the router responds promptly.
            info!("Waiting briefly for UPnP candidates (max 300ms) before sending offer");
            if tokio::time::timeout(Duration::from_millis(300), peer_connection.wait_for_gathering_complete()).await.is_err() {
                debug!("UPnP gathering not complete after 300ms, sending offer anyway (trickle will deliver remaining candidates)");
            }
        }

        let offer_sdp = peer_connection.local_description()
            .ok_or_else(|| anyhow!("Failed to get local description"))?
            .to_sdp_string();

        info!("Sending offer for agent '{}' session {}", self.agent_id, session_id);
        let target = match proto {
            Protocol::Tcp => Target::tcp(Some(target_host.to_string()), target_port),
            Protocol::Udp => Target::udp(Some(target_host.to_string()), target_port),
        };
        dtls_client.send(&SignalingMessage::new_offer(
            session_id.clone(),
            self.agent_id.clone(),
            offer_sdp,
            Some(vec![target]),
        )).await?;

        // Trickle ICE: forward gathered candidates as they arrive
        {
            let pc_candidates = peer_connection.clone();
            // With --wait-candidates the offer SDP already carries every
            // candidate; flushing again would send duplicates.
            let sdp_has_all_candidates = self.wait_candidates;
            tokio::spawn(async move {
                // Candidates already delivered live (keyed by SDP line) so the
                // Complete-time snapshot flush does not duplicate them.
                let mut sent: std::collections::HashSet<String> = std::collections::HashSet::new();
                // Never short-circuit on an already-Complete flag: when
                // gathering finishes with a SINGLE candidate — relay-only
                // ICE policy, where the TURN allocation completes a moment
                // after the answer went out — Complete is observable before
                // any candidate is drained, and dropping straight to
                // end-of-candidates silently discards that candidate. ICE
                // then has no remote candidate to check and the session
                // stalls until timeout (~60% of relay-path sessions). On
                // Complete, flush the authoritative `local_candidates()`
                // snapshot first, then send end-of-candidates.
                let mut gathering_done =
                    sdp_has_all_candidates
                        || *gathering_state_rx.borrow() == IceGatheringState::Complete;
                while !gathering_done {
                    tokio::select! {
                        result = candidate_rx.recv() => {
                            match result {
                                Ok(candidate) => {
                                    let sdp = candidate.to_sdp();
                                    if sent.insert(sdp.clone()) {
                                        info!(
                                            "Local ICE candidate: {} {} {} {:?}",
                                            candidate.address, candidate.transport, candidate.priority, candidate.typ,
                                        );
                                        let mut msg = SignalingMessage::new_candidate(
                                            sid.clone(),
                                            sdp,
                                        );
                                        rel.lock().unwrap().prepare_send(&mut msg, false);
                                        let _ = send_message(&dtls, &msg).await;
                                    }
                                    gathering_done = *gathering_state_rx.borrow() == IceGatheringState::Complete;
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                            }
                        }
                        _ = gathering_state_rx.changed() => {
                            gathering_done = *gathering_state_rx.borrow() == IceGatheringState::Complete;
                        }
                    }
                }
                if !sdp_has_all_candidates {
                    for candidate in pc_candidates.ice_transport().local_candidates() {
                        let sdp = candidate.to_sdp();
                        if sent.insert(sdp.clone()) {
                            info!(
                                "Local ICE candidate (flush): {} {} {} {:?}",
                                candidate.address, candidate.transport, candidate.priority, candidate.typ,
                            );
                            let mut msg = SignalingMessage::new_candidate(
                                sid.clone(),
                                sdp,
                            );
                            rel.lock().unwrap().prepare_send(&mut msg, false);
                            let _ = send_message(&dtls, &msg).await;
                        }
                    }
                }
                let mut msg = SignalingMessage::new_end_of_candidates(sid.clone());
                rel.lock().unwrap().prepare_send(&mut msg, false);
                let _ = send_message(&dtls, &msg).await;
            });
        }

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
                        SignalingMessage::Ping => {
                            let _ = dtls_client.send(&SignalingMessage::Pong).await;
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

        // Keep the signaling DTLS connection to the server alive for the
        // lifetime of this session. The server pings clients every 15s and
        // idle-times-out sessions that stop responding; during the
        // data-channel-open wait the client sends no signaling of its own, so
        // without a responder the server would tear the session down before
        // WebRTC has finished connecting.
        let signaling_done = tokio_util::sync::CancellationToken::new();
        {
            let mut dtls_client = dtls_client;
            let done = signaling_done.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = done.cancelled() => {
                            dtls_client.close();
                            break;
                        }
                        msg = dtls_client.recv() => {
                            match msg {
                                Ok(SignalingMessage::Ping) => {
                                    let _ = dtls_client.send(&SignalingMessage::Pong).await;
                                }
                                Ok(_) => {}
                                Err(_) => break,
                            }
                        }
                    }
                }
            });
        }

        Ok((peer_connection, data_channel, remote_msg_rx, signaling_done))
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

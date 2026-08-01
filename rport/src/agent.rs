use crate::{
    acl::Acl,
    config::{IceServerConfig, RportConfig},
    dtls_signaling::{DtlsClient, SignalingMessage, Target, send_message, recv_message},
    reliable::ReliableSession,
    webrtc_config::WebRTCConfig,
};
use anyhow::{anyhow, Result};
use bytes::Bytes;
use rustrtc::{
    transports::{
        dtls::DtlsTransport,
        sctp::{DataChannelConfig, DataChannelEvent},
    },
    IceCandidate, IceGatheringState, PeerConnection, PeerConnectionState, SdpType,
    SessionDescription,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

pub const RECONNECT_INTERVAL: u64 = 5;

/// Owns a WebRTC peer connection and all background tasks spawned for it.
/// On drop, closes the PC (which unblocks tasks waiting on its channels)
/// and aborts any tasks that are still alive.
struct ActiveSession {
    pc: Arc<PeerConnection>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    reliable: Arc<Mutex<ReliableSession>>,
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
            // One agent can serve multiple concurrent client sessions, each
            // keyed by its server-assigned session_id. A session reaps itself
            // when its peer connection reaches a terminal state (Failed/Closed)
            // by reporting back on `done_tx`.
            let mut active_sessions: HashMap<String, ActiveSession> = HashMap::new();
            let (done_tx, mut done_rx) = mpsc::unbounded_channel::<String>();

            loop {
                let msg = tokio::select! {
                    biased;
                    // Self-reap: a session's peer connection ended.
                    sid = done_rx.recv() => {
                        if let Some(sid) = sid {
                            if let Some(mut session) = active_sessions.remove(&sid) {
                                info!("Session {} ended ({} active session(s) remain)",
                                      sid, active_sessions.len());
                                session.close();
                            }
                        }
                        continue;
                    }
                    msg_result = tokio::time::timeout(
                        Duration::from_secs(45),
                        recv_message(&mut client.data_rx),
                    ) => {
                        match msg_result {
                            Ok(Ok(m)) => m,
                            Ok(Err(e)) => {
                                error!("DTLS recv error: {}", e);
                                break;
                            }
                            Err(_) => {
                                warn!("DTLS recv timeout (45s)");
                                break;
                            }
                        }
                    }
                };

                match msg {
                    SignalingMessage::Offer { session_id, offer_sdp, targets, .. } => {
                        info!("Received offer from server: session={}", session_id);
                        let dtls = client.dtls.clone();
                        let wc = self.webrtc_config.clone();
                        let ac = self.acl.clone();

                        // Replace a prior session with the same id (client reconnect).
                        if let Some(mut old) = active_sessions.remove(&session_id) {
                            info!("Replacing existing session {}", session_id);
                            old.close();
                        }

                        match handle_offer(
                            dtls, &session_id, &offer_sdp, targets,
                            ac, wc, &extra_ice_servers,
                            done_tx.clone(),
                        ).await {
                            Ok(session) => {
                                active_sessions.insert(session_id.clone(), session);
                                info!("Session {} active ({} total)", session_id, active_sessions.len());
                            }
                            Err(e) => {
                                error!("Failed to handle offer {}: {}", session_id, e);
                            }
                        }
                    }
                    SignalingMessage::Candidate { ref session_id, ref candidate, .. } => {
                        if let Some(session) = active_sessions.get(session_id) {
                            if let Ok(c) = IceCandidate::from_sdp(candidate) {
                                session.pc.add_ice_candidate(c).ok();
                            }
                            session.reliable.lock().unwrap().process_recv(&msg);
                        } else {
                            debug!("Candidate for unknown session {}, ignoring", session_id);
                        }
                    }
                    SignalingMessage::EndOfCandidates { ref session_id, .. } => {
                        if let Some(session) = active_sessions.get(session_id) {
                            session.reliable.lock().unwrap().process_recv(&msg);
                        }
                        debug!("End-of-candidates for session {}", session_id);
                    }
                    SignalingMessage::Ack { ref session_id, .. } => {
                        if let Some(session) = active_sessions.get(session_id) {
                            session.reliable.lock().unwrap().process_recv(&msg);
                        }
                    }
                    SignalingMessage::Ping => {
                        send_message(&client.dtls, &SignalingMessage::Pong).await.ok();
                    }
                    SignalingMessage::Pong => {}
                    SignalingMessage::Error { reason, .. } => warn!("Server error: {}", reason),
                    other => warn!("Unexpected message from server: {:?}", other),
                }
            }

            // Cleanup all active sessions on exit
            for (_, mut session) in active_sessions.drain() {
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

#[allow(clippy::too_many_arguments)]
async fn handle_offer(
    dtls: Arc<DtlsTransport>,
    session_id: &str,
    offer_sdp: &str,
    targets: Option<Vec<Target>>,
    acl: Option<Acl>,
    webrtc_config: WebRTCConfig,
    extra_ice_servers: &[IceServerConfig],
    done_tx: mpsc::UnboundedSender<String>,
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
                    send_message(&dtls, &SignalingMessage::new_error(
                        session_id.to_string(),
                        format!("Cannot resolve target: {}", host),
                    )).await.ok();
                    return Err(anyhow!("Cannot resolve target: {}", host));
                }
            };
            if !acl.is_allowed(&ip, *port) {
                send_message(&dtls, &SignalingMessage::new_error(
                    session_id.to_string(),
                    format!("Access denied: {}:{}", host, port),
                )).await.ok();
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
                                    // Disable Nagle for interactive protocols (SSH).
                                    let _ = tcp.set_nodelay(true);
                                    let (mut tcp_read, mut tcp_write) = tcp.into_split();
                                    let pc3 = pc2.clone();
                                    tokio::spawn(async move {
                                        let mut buf = [0u8; 1024];
                                        let mut total: u64 = 0;
                                        loop {
                                            match tcp_read.read(&mut buf).await {
                                                Ok(0) | Err(_) => {
                                                    debug!("agent tcp_read EOF (sent {} bytes total)", total);
                                                    break;
                                                }
                                                Ok(n) => {
                                                    total += n as u64;
                                                    match pc3.send_data(dc_id, &buf[..n]).await {
                                                        Ok(_) => debug!("agent→dc: {} bytes (total {})", n, total),
                                                        Err(e) => { debug!("agent send_data err: {}", e); break; }
                                                    }
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

    // Session lifecycle monitor: reap this session from the map when its peer
    // connection reaches a terminal state. Disconnected is intentionally
    // ignored (may recover within the ICE grace period); the rustrtc disconnect
    // monitor promotes long-lived Disconnected -> Failed/Closed.
    {
        let pc_watch = peer_connection.clone();
        let sid = session_id.to_string();
        let done = done_tx.clone();
        tasks.push(tokio::spawn(async move {
            let mut state_rx = pc_watch.subscribe_peer_state();
            while state_rx.changed().await.is_ok() {
                let state = *state_rx.borrow();
                match state {
                    PeerConnectionState::Failed | PeerConnectionState::Closed => {
                        debug!("Peer connection for session {} -> {:?}, reaping", sid, state);
                        let _ = done.send(sid);
                        return;
                    }
                    PeerConnectionState::Connected => {
                        if let Some(pair) = pc_watch.ice_transport().get_selected_pair() {
                            info!(
                                "ICE selected pair (agent): local {} {:?} {} -> remote {} {:?} {} [nominated={}]",
                                pair.local.address, pair.local.typ, pair.local.transport,
                                pair.remote.address, pair.remote.typ, pair.remote.transport,
                                pair.nominated,
                            );
                        }
                    }
                    _ => {}
                }
            }
        }));
    }

    // Monitor selected ICE candidate pair — fires when a pair is nominated.
    {
        let mut pair_rx = peer_connection.ice_transport().subscribe_selected_pair();
        tasks.push(tokio::spawn(async move {
            while pair_rx.changed().await.is_ok() {
                if let Some(ref pair) = *pair_rx.borrow() {
                    info!(
                        "ICE pair nominated (agent): local {} {:?} {} -> remote {} {:?} {} [nominated={}]",
                        pair.local.address, pair.local.typ, pair.local.transport,
                        pair.remote.address, pair.remote.typ, pair.remote.transport,
                        pair.nominated,
                    );
                }
            }
        }));
    }

    // Reliable session for Answer retransmission
    let reliable = Arc::new(Mutex::new(ReliableSession::new(
        Duration::from_millis(2000),
        3,
    )));

    // Subscribe to ICE candidate / gathering events BEFORE
    // set_local_description, because host candidates are gathered almost
    // instantly once gathering starts and the broadcast channel does not
    // replay candidates emitted before the subscription exists. Buffering
    // them here avoids losing host candidates (needed for same-LAN links).
    let mut candidate_rx = peer_connection.subscribe_ice_candidates();
    let mut gathering_state_rx = peer_connection.subscribe_ice_gathering_state();
    let dtls_c = dtls.clone();
    let sid = session_id.to_string();
    let rel = reliable.clone();

    // Send answer with seq=1 for retransmission tracking
    let answer_result: Result<(), anyhow::Error> = async {
        let answer = peer_connection.create_answer().await?;
        peer_connection.set_local_description(answer)?;
        let answer_sdp = peer_connection.local_description()
            .ok_or_else(|| anyhow!("No local description"))?
            .to_sdp_string();
        let mut msg = SignalingMessage::new_answer(
            session_id.to_string(),
            answer_sdp,
        );
        {
            let mut r = reliable.lock().unwrap();
            r.prepare_send(&mut msg, true);
        }
        send_message(&dtls, &msg).await?;
        Ok(())
    }.await;

    if let Err(e) = answer_result {
        for t in &tasks { t.abort(); }
        peer_connection.close();
        return Err(e);
    }

    info!("WebRTC answer sent for session {} (with reliability)", session_id);

    // Retransmission timer for Answer/EndOfCandidates
    {
        let dtls_c = dtls.clone();
        let rel = reliable.clone();
        tasks.push(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                let pending = {
                    let mut r = rel.lock().unwrap();
                    r.due_retransmits(Instant::now())
                };
                if pending.is_empty() {
                    let r = rel.lock().unwrap();
                    if r.is_exhausted() || !r.has_pending() {
                        break;
                    }
                    continue;
                }
                for p in pending {
                    debug!("Retransmitting seq={}", p.seq);
                    let _ = send_message(&dtls_c, &p.msg).await;
                }
            }
        }));
    }

    // Trickle ICE: forward gathered candidates as they arrive
    {
        tasks.push(tokio::spawn(async move {
            if *gathering_state_rx.borrow() == IceGatheringState::Complete {
                let mut msg = SignalingMessage::new_end_of_candidates(sid.clone());
                {
                    let mut r = rel.lock().unwrap();
                    r.prepare_send(&mut msg, true);
                }
                let _ = send_message(&dtls_c, &msg).await;
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
                                {
                                    let mut r = rel.lock().unwrap();
                                    r.prepare_send(&mut msg, false);
                                }
                                let _ = send_message(&dtls_c, &msg).await;
                                if *gathering_state_rx.borrow() == IceGatheringState::Complete {
                                    let mut msg = SignalingMessage::new_end_of_candidates(sid.clone());
                                    {
                                        let mut r = rel.lock().unwrap();
                                        r.prepare_send(&mut msg, true);
                                    }
                                    let _ = send_message(&dtls_c, &msg).await;
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
                            {
                                let mut r = rel.lock().unwrap();
                                r.prepare_send(&mut msg, true);
                            }
                            let _ = send_message(&dtls_c, &msg).await;
                            break;
                        }
                    }
                }
            }
        }));
    }

    Ok(ActiveSession { pc: peer_connection, tasks, reliable })
}

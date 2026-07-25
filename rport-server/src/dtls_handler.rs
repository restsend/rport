use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use rustrtc::transports::dtls::{self, DtlsState, DtlsTransport, Certificate};
use rustrtc::transports::ice::conn::IceConn;
use rustrtc::transports::ice::IceSocketWrapper;
use rustrtc::transports::PacketReceiver;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch, RwLock};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use rport_server::handler::PendingOffer;
use rport_server::{AppState, ServerMessage};

//=== Unified signaling message types (compatible with rport crate) ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub host: Option<String>,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    #[serde(rename = "register")]
    Register { token: String, id: String },
    #[serde(rename = "offer")]
    Offer { session_id: String, agent_id: String, offer_sdp: String, targets: Option<Vec<Target>> },
    #[serde(rename = "answer")]
    Answer { session_id: String, answer_sdp: String },
    #[serde(rename = "candidate")]
    Candidate { session_id: String, candidate: String },
    #[serde(rename = "end-of-candidates")]
    EndOfCandidates { session_id: String },
    #[serde(rename = "error")]
    Error { session_id: String, reason: String },
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "pong")]
    Pong,
}

fn encode_msg(msg: &SignalingMessage) -> Result<Vec<u8>> {
    let json = serde_json::to_string(msg)?;
    let len = json.len();
    let mut buf = Vec::with_capacity(4 + len);
    buf.extend_from_slice(&(len as u32).to_be_bytes());
    buf.extend_from_slice(json.as_bytes());
    Ok(buf)
}

async fn recv_msg(rx: &mut mpsc::UnboundedReceiver<Bytes>) -> Result<SignalingMessage> {
    let data = rx.recv().await.ok_or_else(|| anyhow!("DTLS channel closed"))?;
    if data.len() < 4 {
        return Err(anyhow!("Frame too short"));
    }
    let msg_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + msg_len {
        return Err(anyhow!("Incomplete frame"));
    }
    Ok(serde_json::from_slice(&data[4..4 + msg_len])?)
}

async fn send_msg(dtls: &DtlsTransport, msg: &SignalingMessage) -> Result<()> {
    let data = encode_msg(msg)?;
    dtls.send(Bytes::from(data)).await?;
    Ok(())
}

//=== Session types ===

#[derive(Clone)]
struct ClientSession {
    dtls: Arc<DtlsTransport>,
    agent_id: String,
}

pub struct DtlsHandler;

pub fn load_certificate(cert_path: &Path, key_path: &Path) -> Result<Certificate> {
    let cert_pem = std::fs::read_to_string(cert_path)
        .with_context(|| format!("Failed to read DTLS cert from {}", cert_path.display()))?;
    let key_pem = std::fs::read_to_string(key_path)
        .with_context(|| format!("Failed to read DTLS key from {}", key_path.display()))?;

    let pem_data = pem::parse(&cert_pem)
        .with_context(|| format!("Failed to parse DTLS cert PEM from {}", cert_path.display()))?;
    let der_bytes = pem_data.contents().to_vec();

    let mut cert = Certificate::default();
    cert.certificate = vec![der_bytes];
    cert.private_key = key_pem;
    Ok(cert)
}

impl DtlsHandler {
    /// Start DTLS signaling server. Returns a JoinHandle.
    pub async fn listen(addr: String, cert: Option<Certificate>, state: AppState) -> Result<tokio::task::JoinHandle<()>> {
        let socket = Arc::new(UdpSocket::bind(&addr).await?);
        let cert = cert.unwrap_or_else(|| dtls::generate_certificate().expect("gen cert"));
        let fingerprint = dtls::fingerprint(&cert);
        info!("DTLS signaling server listening on {}, fingerprint: {}", addr, fingerprint);

        let agents: Arc<RwLock<HashMap<String, Arc<DtlsTransport>>>> = Arc::new(RwLock::new(HashMap::new()));
        let sessions: Arc<RwLock<HashMap<String, ClientSession>>> = Arc::new(RwLock::new(HashMap::new()));
        let connect_counts: Arc<RwLock<HashMap<String, usize>>> = Arc::new(RwLock::new(HashMap::new()));

        let handle = tokio::spawn(async move {
            if let Err(e) = Self::run_loop(socket, cert, agents, sessions, connect_counts, state).await {
                error!("DTLS handler error: {}", e);
            }
        });

        Ok(handle)
    }

    async fn run_loop(
        socket: Arc<UdpSocket>,
        cert: Certificate,
        agents: Arc<RwLock<HashMap<String, Arc<DtlsTransport>>>>,
        sessions: Arc<RwLock<HashMap<String, ClientSession>>>,
        connect_counts: Arc<RwLock<HashMap<String, usize>>>,
        state: AppState,
    ) -> Result<()> {
        let mut peers: HashMap<SocketAddr, (Arc<IceConn>, tokio::task::JoinHandle<()>)> = HashMap::new();
        let mut buf = [0u8; 2000];

        loop {
            let (len, peer_addr) = match socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => { warn!("DTLS recv error: {}", e); continue; }
            };
            let packet = Bytes::copy_from_slice(&buf[..len]);

            if let Some((conn, _)) = peers.get(&peer_addr) {
                let mut mb = Vec::new();
                PacketReceiver::receive(conn.as_ref(), packet, peer_addr, &mut mb).await;
                continue;
            }

            // New peer
            let (tx, rx) = watch::channel(Some(IceSocketWrapper::Udp(socket.clone())));
            let conn = IceConn::new(rx, peer_addr, None);
            drop(tx);

            let (dtls, data_rx, runner) = match DtlsTransport::new(
                conn.clone(), cert.clone(), false, 4096, None,
            ).await {
                Ok(v) => v,
                Err(e) => { warn!("Failed to create DTLS: {}", e); continue; }
            };
            conn.set_dtls_receiver(dtls.clone());

            let mut mb = Vec::new();
            PacketReceiver::receive(conn.as_ref(), packet, peer_addr, &mut mb).await;

            tokio::spawn(runner);

            let addr = peer_addr;
            let agents_c = agents.clone();
            let sessions_c = sessions.clone();
            let counts_c = connect_counts.clone();
            let dtls_c = dtls.clone();
            let state_c = state.clone();

            let feed_handle = tokio::spawn(async move {
                let mut state_rx = dtls_c.subscribe_state();
                loop {
                    if let DtlsState::Connected(_, _) = *state_rx.borrow() {
                        info!("DTLS handshake succeeded: {}", addr);
                        break;
                    }
                    if state_rx.changed().await.is_err() {
                        warn!("DTLS handshake failed for {}", addr);
                        return;
                    }
                }

                let mut data_rx = data_rx;
                let first_msg = match recv_msg(&mut data_rx).await {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("Failed to read first message from {}: {}", addr, e);
                        return;
                    }
                };

                match first_msg {
                    SignalingMessage::Register { token: _, id } => {
                        let count = {
                            let mut cc = counts_c.write().await;
                            let c = cc.entry(id.clone()).or_insert(0);
                            *c += 1;
                            *c
                        };
                        let start = tokio::time::Instant::now();
                        info!("Agent '{}' registered from {} (connection #{})", id, addr, count);
                        let agents_for_loop = agents_c.clone();
                        agents_c.write().await.insert(id.clone(), dtls_c.clone());
                        Self::agent_loop(dtls_c, &mut data_rx, agents_for_loop, sessions_c, id.clone()).await;
                        agents_c.write().await.remove(&id);
                        info!("Agent '{}' disconnected (lifetime: {:.1}s, total connections: {})",
                              id, start.elapsed().as_secs_f64(), count);
                    }
                    SignalingMessage::Offer { session_id, agent_id, offer_sdp, targets } => {
                        info!("Offer from client for agent '{}' (session: {}) from {}", agent_id, session_id, addr);
                        let sid = session_id.clone();
                        let aid = agent_id.clone();
                        sessions_c.write().await.insert(session_id.clone(), ClientSession {
                            dtls: dtls_c.clone(),
                            agent_id: agent_id.clone(),
                        });
                        let agent_dtls = agents_c.read().await.get(&agent_id).cloned();
                        if let Some(agent_dtls) = agent_dtls {
                            info!("Agent '{}' found via DTLS registry", agent_id);

                            let offer_msg = SignalingMessage::Offer {
                                session_id: session_id.clone(),
                                agent_id: agent_id.clone(),
                                offer_sdp,
                                targets,
                            };
                            if let Err(e) = send_msg(&agent_dtls, &offer_msg).await {
                                warn!("Failed to forward offer to agent '{}': {}", agent_id, e);
                                let _ = send_msg(&dtls_c, &SignalingMessage::Error {
                                    session_id,
                                    reason: format!("Agent '{}' unavailable", agent_id),
                                }).await;
                                sessions_c.write().await.remove(&sid);
                                return;
                            }
                            Self::client_loop(dtls_c, &mut data_rx, sessions_c, agents_c, sid, aid).await;
                        } else if let Some(http_agent) = Self::find_http_agent(&state_c, &agent_id).await {
                            info!("Agent '{}' found in HTTP/SSE registry (sid={}), bridging signaling", agent_id, session_id);
                            let uuid = Uuid::new_v4();
                            let (answer_tx, answer_rx) = oneshot::channel();
                            state_c.pending_offers.write().await.insert(uuid, PendingOffer {
                                offer: offer_sdp.clone(),
                                client_ip: addr.to_string(),
                                sender: answer_tx,
                            });
                            let (candidate_tx, candidate_rx) = mpsc::unbounded_channel::<String>();
                            state_c.pending_candidates.write().await.insert(uuid, candidate_tx);
                            let server_message = ServerMessage {
                                message_type: "offer".to_string(),
                                data: serde_json::json!({
                                    "uuid": uuid,
                                    "offer": offer_sdp,
                                    "client_ip": addr.to_string(),
                                }),
                            };
                            if http_agent.sender.send(server_message).is_err() {
                                warn!("Failed to send offer via SSE to agent '{}'", agent_id);
                                state_c.pending_offers.write().await.remove(&uuid);
                                state_c.pending_candidates.write().await.remove(&uuid);
                                let _ = send_msg(&dtls_c, &SignalingMessage::Error {
                                    session_id: session_id.clone(),
                                    reason: format!("Agent '{}' unavailable", agent_id),
                                }).await;
                                sessions_c.write().await.remove(&sid);
                                return;
                            }
                            match tokio::time::timeout(Duration::from_secs(30), answer_rx).await {
                                Ok(Ok(answer)) => {
                                    let _ = send_msg(&dtls_c, &SignalingMessage::Answer {
                                        session_id: session_id.clone(),
                                        answer_sdp: answer,
                                    }).await;
                                    info!("Offer/answer bridge complete for agent '{}'", agent_id);
                                    Self::client_loop_bridge(dtls_c, &mut data_rx, state_c, sid.clone(), aid, uuid, candidate_rx).await;
                                    sessions_c.write().await.remove(&sid);
                                }
                                _ => {
                                    warn!("Answer timeout for bridged agent '{}'", agent_id);
                                    state_c.pending_offers.write().await.remove(&uuid);
                                    state_c.pending_candidates.write().await.remove(&uuid);
                                    let _ = send_msg(&dtls_c, &SignalingMessage::Error {
                                        session_id: session_id.clone(),
                                        reason: format!("Agent '{}' answer timeout", agent_id),
                                    }).await;
                                    sessions_c.write().await.remove(&sid);
                                }
                            }
                        } else {
                            warn!("Agent '{}' not found in any registry for session {}", agent_id, session_id);
                            let _ = send_msg(&dtls_c, &SignalingMessage::Error {
                                session_id,
                                reason: format!("Agent '{}' not found", agent_id),
                            }).await;
                            sessions_c.write().await.remove(&sid);
                        }
                    }
                    other => {
                        warn!("Unexpected first message from {}: {:?}", addr, other);
                    }
                }
            });

            peers.insert(peer_addr, (conn, feed_handle));
        }
    }

    async fn agent_loop(
        dtls: Arc<DtlsTransport>,
        data_rx: &mut mpsc::UnboundedReceiver<Bytes>,
        _agents: Arc<RwLock<HashMap<String, Arc<DtlsTransport>>>>,
        sessions: Arc<RwLock<HashMap<String, ClientSession>>>,
        agent_id: String,
    ) {
        // Keepalive: send Ping to agents periodically
        let ping_handle = {
            let dtls = dtls.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;
                    if send_msg(&dtls, &SignalingMessage::Ping).await.is_err() {
                        break;
                    }
                }
            })
        };

        let msg_loop = async {
            loop {
                let msg = match tokio::time::timeout(
                    tokio::time::Duration::from_secs(30),
                    recv_msg(data_rx),
                ).await {
                    Ok(Ok(m)) => m,
                    Ok(Err(e)) => {
                        warn!("Agent '{}' recv error: {}", agent_id, e);
                        break;
                    }
                    Err(_) => {
                        warn!("Agent '{}' recv timeout", agent_id);
                        break;
                    }
                };

                match msg {
                    SignalingMessage::Answer { session_id, answer_sdp } => {
                        let client = sessions.read().await.get(&session_id).cloned();
                        if let Some(client) = client {
                            if let Err(e) = send_msg(&client.dtls, &SignalingMessage::Answer {
                                session_id, answer_sdp,
                            }).await {
                                warn!("Failed to forward answer to client: {}", e);
                            }
                        }
                    }
                    SignalingMessage::Candidate { session_id, candidate } => {
                        let client = sessions.read().await.get(&session_id).cloned();
                        if let Some(client) = client {
                            let _ = send_msg(&client.dtls, &SignalingMessage::Candidate {
                                session_id, candidate,
                            }).await;
                        }
                    }
                    SignalingMessage::EndOfCandidates { session_id } => {
                        let client = sessions.read().await.get(&session_id).cloned();
                        if let Some(client) = client {
                            let _ = send_msg(&client.dtls, &SignalingMessage::EndOfCandidates {
                                session_id,
                            }).await;
                        }
                    }
                    SignalingMessage::Ping => {
                        let _ = send_msg(&dtls, &SignalingMessage::Pong).await;
                    }
                    SignalingMessage::Pong => {}
                    SignalingMessage::Error { reason, .. } => {
                        warn!("Agent '{}' error: {}", agent_id, reason);
                    }
                    other => {
                        debug!("Agent '{}' unexpected message: {:?}", agent_id, other);
                    }
                }
            }
        };

        tokio::select! {
            _ = msg_loop => {}
            _ = ping_handle => {}
        }

        // Cleanup: remove all sessions for this agent
        let mut sessions_w = sessions.write().await;
        sessions_w.retain(|_, s| s.agent_id != agent_id);
    }

    async fn client_loop(
        dtls: Arc<DtlsTransport>,
        data_rx: &mut mpsc::UnboundedReceiver<Bytes>,
        sessions: Arc<RwLock<HashMap<String, ClientSession>>>,
        agents: Arc<RwLock<HashMap<String, Arc<DtlsTransport>>>>,
        session_id: String,
        agent_id: String,
    ) {
        loop {
            let msg = match tokio::time::timeout(
                tokio::time::Duration::from_secs(60),
                recv_msg(data_rx),
            ).await {
                Ok(Ok(m)) => m,
                Ok(Err(e)) => {
                    warn!("Client session {} recv error: {}", session_id, e);
                    break;
                }
                Err(_) => {
                    info!("Client session {} idle timeout", session_id);
                    break;
                }
            };

            match msg {
                SignalingMessage::Candidate { session_id: sid, candidate } => {
                    if let Some(agent_dtls) = agents.read().await.get(&agent_id) {
                        let _ = send_msg(agent_dtls, &SignalingMessage::Candidate {
                            session_id: sid, candidate,
                        }).await;
                    }
                }
                SignalingMessage::EndOfCandidates { session_id: sid } => {
                    if let Some(agent_dtls) = agents.read().await.get(&agent_id) {
                        let _ = send_msg(agent_dtls, &SignalingMessage::EndOfCandidates {
                            session_id: sid,
                        }).await;
                    }
                }
                SignalingMessage::Ping => {
                    let _ = send_msg(&dtls, &SignalingMessage::Pong).await;
                }
                SignalingMessage::Pong => {}
                SignalingMessage::Error { reason, .. } => {
                    warn!("Client session {} error: {}", session_id, reason);
                    break;
                }
                other => {
                    debug!("Client session {} unexpected message: {:?}", session_id, other);
                }
            }
        }

        sessions.write().await.remove(&session_id);
        info!("Client session {} ended", session_id);
    }

    async fn find_http_agent(state: &AppState, agent_id: &str) -> Option<rport_server::handler::AgentConnection> {
        let agents = state.agents.read().await;
        let match_suffix = format!(":{}", agent_id);
        let result = agents.iter()
            .find(|(key, _)| key.ends_with(&match_suffix))
            .map(|(key, agent)| {
                info!("HTTP/SSE agent '{}' found via lookup key '{}'", agent_id, key);
                agent.clone()
            });
        if result.is_none() {
            debug!("Agent '{}' not found in HTTP/SSE registry (total {} agents)", agent_id, agents.len());
        }
        result
    }

    async fn client_loop_bridge(
        dtls: Arc<DtlsTransport>,
        data_rx: &mut mpsc::UnboundedReceiver<Bytes>,
        state: AppState,
        session_id: String,
        agent_id: String,
        uuid: Uuid,
        mut candidate_rx: mpsc::UnboundedReceiver<String>,
    ) {
        let http_agent = Self::find_http_agent(&state, &agent_id).await;

        loop {
            tokio::select! {
                msg = recv_msg(data_rx) => {
                    match msg {
                        Ok(SignalingMessage::Candidate { session_id: sid, candidate }) => {
                            if let Some(ref agent) = http_agent {
                                let msg = ServerMessage {
                                    message_type: "candidate".to_string(),
                                    data: serde_json::json!({
                                        "session_id": sid,
                                        "candidate": candidate,
                                    }),
                                };
                                let _ = agent.sender.send(msg);
                            }
                        }
                        Ok(SignalingMessage::EndOfCandidates { session_id: sid }) => {
                            if let Some(ref agent) = http_agent {
                                let msg = ServerMessage {
                                    message_type: "end-of-candidates".to_string(),
                                    data: serde_json::json!({"session_id": sid}),
                                };
                                let _ = agent.sender.send(msg);
                            }
                        }
                        Ok(SignalingMessage::Ping) => {
                            let _ = send_msg(&dtls, &SignalingMessage::Pong).await;
                        }
                        Ok(SignalingMessage::Pong) => {}
                        Ok(SignalingMessage::Error { reason, .. }) => {
                            warn!("Bridge session {} error: {}", session_id, reason);
                            break;
                        }
                        Err(_) => break,
                        _ => {}
                    }
                }
                candidate = candidate_rx.recv() => {
                    match candidate {
                        Some(candidate) => {
                            let _ = send_msg(&dtls, &SignalingMessage::Candidate {
                                session_id: session_id.clone(),
                                candidate,
                            }).await;
                        }
                        None => break,
                    }
                }
            }
        }

        state.pending_candidates.write().await.remove(&uuid);
        info!("Bridge session {} ended", session_id);
    }
}

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use rustrtc::transports::dtls::{self, DtlsState, DtlsTransport, Certificate};
use rustrtc::transports::ice::conn::IceConn;
use rustrtc::transports::ice::IceSocketWrapper;
use rustrtc::transports::PacketReceiver;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch, RwLock};
use tracing::{error, info, warn};

//=== Re-using the same signaling message types from rport crate ===
// For the server, we need compatible types.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DtlsMessage {
    #[serde(rename = "register")]
    Register { token: String, id: String, fingerprint: String },
    #[serde(rename = "offer")]
    Offer { session_id: String, agent_id: String, offer_sdp: String },
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

fn encode_msg(msg: &DtlsMessage) -> Result<Vec<u8>> {
    let json = serde_json::to_string(msg)?;
    let len = json.len();
    let mut buf = Vec::with_capacity(4 + len);
    buf.extend_from_slice(&(len as u32).to_be_bytes());
    buf.extend_from_slice(json.as_bytes());
    Ok(buf)
}

async fn recv_msg(rx: &mut mpsc::UnboundedReceiver<Bytes>) -> Result<DtlsMessage> {
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

struct AgentSession {
    _token: String,
    _id: String,
    dtls: Arc<DtlsTransport>,
    _last_ping: SystemTime,
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

    /// Start DTLS listener. Returns a JoinHandle that must be awaited.
    pub async fn listen(addr: String, cert: Option<Certificate>) -> Result<tokio::task::JoinHandle<()>> {
        let socket = Arc::new(UdpSocket::bind(&addr).await?);
        let cert = cert.unwrap_or_else(|| dtls::generate_certificate().expect("gen cert"));
        let fingerprint = dtls::fingerprint(&cert);
        info!("DTLS server listening on {}, fingerprint: {}", addr, fingerprint);

        let agents: Arc<RwLock<HashMap<String, AgentSession>>> = Arc::new(RwLock::new(HashMap::new()));
        let agents_clone = agents.clone();

        let handle = tokio::spawn(async move {
            if let Err(e) = Self::run_loop(socket, cert, agents_clone).await {
                error!("DTLS handler error: {}", e);
            }
        });

        Ok(handle)
    }

    async fn run_loop(
        socket: Arc<UdpSocket>,
        cert: Certificate,
        agents: Arc<RwLock<HashMap<String, AgentSession>>>,
    ) -> Result<()> {
        loop {
            let mut buf = [0u8; 2000];
            let (len, peer_addr) = match socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => { warn!("DTLS recv error: {}", e); continue; }
            };
            let first_packet = Bytes::copy_from_slice(&buf[..len]);

            let peer_socket = match UdpSocket::bind("127.0.0.1:0").await {
                Ok(s) => Arc::new(s),
                Err(e) => { warn!("Failed to bind socket: {}", e); continue; }
            };

            let (tx, rx) = watch::channel(Some(IceSocketWrapper::Udp(peer_socket.clone())));
            let conn = IceConn::new(rx, peer_addr, None);
            drop(tx);

            let (dtls, data_rx, runner) = match DtlsTransport::new(
                conn.clone(), cert.clone(), false, 4096, None,
            ).await {
                Ok(v) => v,
                Err(e) => { warn!("Failed to create DTLS: {}", e); continue; }
            };

            conn.set_dtls_receiver(dtls.clone());

            // Feed first packet
            let mut mb = Vec::new();
            PacketReceiver::receive(conn.as_ref(), first_packet, peer_addr, &mut mb).await;

            // Background task to feed remaining packets from this peer
            let conn_clone = conn.clone();
            let sk = socket.clone();
            let addr = peer_addr;
            tokio::spawn(async move {
                let mut buf2 = [0u8; 2000];
                loop {
                    let (len, from) = match sk.recv_from(&mut buf2).await {
                        Ok(v) => v, Err(_) => break,
                    };
                    if from != addr { continue; }
                    let pkt = Bytes::copy_from_slice(&buf2[..len]);
                    let mut mb2 = Vec::new();
                    PacketReceiver::receive(conn_clone.as_ref(), pkt, from, &mut mb2).await;
                }
            });

            tokio::spawn(runner);

            let agents_clone = agents.clone();
            tokio::spawn(async move {
                // Wait for DTLS handshake
                let mut state_rx = dtls.subscribe_state();
                loop {
                    if let DtlsState::Connected(_, _) = *state_rx.borrow() { break; }
                    if state_rx.changed().await.is_err() { return; }
                }

                info!("DTLS peer connected from {}", peer_addr);

                // Receive first message (register or offer)
                let mut data_rx = data_rx;
                let msg = match recv_msg(&mut data_rx).await {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("Failed to receive first message from {}: {}", peer_addr, e);
                        dtls.close();
                        return;
                    }
                };

                match msg {
                    DtlsMessage::Register { token, id, fingerprint: _ } => {
                        let key = format!("{}:{}", token, id);
                        let agent = AgentSession {
                            _token: token, _id: id, dtls: dtls.clone(),
                            _last_ping: SystemTime::now(),
                        };
                        agents_clone.write().await.insert(key.clone(), agent);
                        info!("Agent '{}' registered via DTLS", key);
                    }
                    DtlsMessage::Offer { session_id, agent_id, offer_sdp } => {
                        let _agent_key = format!("client:{}", agent_id);
                        info!("Offer from client for agent '{}'", _agent_key);
                        let agents = agents_clone.read().await;
                        if let Some(agent) = agents.get(&_agent_key) {
                            if let Ok(data) = encode_msg(&DtlsMessage::Offer {
                                session_id: session_id.clone(),
                                agent_id: agent_id.clone(),
                                offer_sdp,
                            }) {
                                let _ = agent.dtls.send(Bytes::from(data)).await;
                                info!("Offer forwarded to agent '{}'", _agent_key);
                            }
                        } else {
                            let err = encode_msg(&DtlsMessage::Error {
                                session_id, reason: format!("Agent '{}' not found", _agent_key),
                            }).unwrap();
                            let _ = dtls.send(Bytes::from(err)).await;
                        }
                    }
                    other => {
                        warn!("Unexpected first message from {}: {:?}", peer_addr, other);
                    }
                }

                dtls.close();
            });
        }
    }
}

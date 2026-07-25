use anyhow::{anyhow, Result};
use bytes::Bytes;
use rustrtc::transports::dtls::{DtlsState, DtlsTransport, Certificate, generate_certificate, fingerprint};
use rustrtc::transports::ice::conn::IceConn;
use rustrtc::transports::ice::IceSocketWrapper;
use rustrtc::transports::PacketReceiver;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

//=== Message types ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub host: Option<String>,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    #[serde(rename = "offer")]
    Offer { session_id: String, token: Option<String>, offer_sdp: String, targets: Option<Vec<Target>> },
    #[serde(rename = "answer")]
    Answer { session_id: String, answer_sdp: String },
    #[serde(rename = "candidate")]
    Candidate { session_id: String, candidate: String },
    #[serde(rename = "end-of-candidates")]
    EndOfCandidates { session_id: String },
    #[serde(rename = "register")]
    Register { token: String, id: String, fingerprint: String },
    #[serde(rename = "registered")]
    Registered {},
    #[serde(rename = "error")]
    Error { session_id: String, reason: String },
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "pong")]
    Pong,
}

//=== Frame codec ===

pub fn encode_message(msg: &SignalingMessage) -> Result<Vec<u8>> {
    let json = serde_json::to_string(msg)?;
    let len = json.len();
    let mut buf = Vec::with_capacity(4 + len);
    buf.extend_from_slice(&(len as u32).to_be_bytes());
    buf.extend_from_slice(json.as_bytes());
    Ok(buf)
}

pub async fn recv_message(rx: &mut mpsc::UnboundedReceiver<Bytes>) -> Result<SignalingMessage> {
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

pub async fn send_message(dtls: &DtlsTransport, msg: &SignalingMessage) -> Result<()> {
    let data = encode_message(msg)?;
    dtls.send(Bytes::from(data)).await?;
    Ok(())
}

//=== Shared: create an IceConn from a UdpSocket bound to a remote addr ===

fn create_ice_conn(
    socket: Arc<UdpSocket>,
    remote_addr: SocketAddr,
) -> (Arc<IceConn>, watch::Receiver<Option<IceSocketWrapper>>) {
    let (tx, rx) = watch::channel(Some(IceSocketWrapper::Udp(socket)));
    let conn = IceConn::new(rx.clone(), remote_addr, None);
    drop(tx);
    (conn, rx)
}

//=== DTLS Client (Client side) ===

pub struct DtlsClient {
    pub dtls: Arc<DtlsTransport>,
    pub data_rx: mpsc::UnboundedReceiver<Bytes>,
    _reader: tokio::task::JoinHandle<()>,
}

impl DtlsClient {
    pub async fn connect(
        addr: &str,
        expected_fingerprint: Option<String>,
    ) -> Result<Self> {
        let remote_addr: SocketAddr = tokio::net::lookup_host(addr)
            .await?
            .next()
            .ok_or_else(|| anyhow!("Could not resolve DTLS address '{}'", addr))?;

        let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        debug!("DTLS client bound {}, connecting to {}", socket.local_addr()?, remote_addr);

        let (conn, _rx) = create_ice_conn(socket.clone(), remote_addr);

        // Spawn read loop
        let conn_clone = conn.clone();
        let sock_clone = socket.clone();
        let reader = tokio::spawn(async move {
            let mut buf = [0u8; 2000];
            let mut marshal_buf = Vec::new();
            loop {
                let (len, addr) = match sock_clone.recv_from(&mut buf).await {
                    Ok(v) => v, Err(_) => break,
                };
                PacketReceiver::receive(conn_clone.as_ref(), Bytes::copy_from_slice(&buf[..len]), addr, &mut marshal_buf).await;
            }
        });

        let cert = generate_certificate()?;
        let (dtls, data_rx, runner) = DtlsTransport::new(
            conn.clone(), cert, true, 4096, expected_fingerprint,
        ).await?;

        conn.set_dtls_receiver(dtls.clone());
        tokio::spawn(runner);

        // Wait for handshake
        let mut state_rx = dtls.subscribe_state();
        loop {
            if let DtlsState::Connected(_, _) = *state_rx.borrow() { break; }
            if state_rx.changed().await.is_err() {
                dtls.close(); return Err(anyhow!("DTLS handshake failed to {}", addr));
            }
        }
        info!("DTLS connected to {}", remote_addr);
        Ok(Self { dtls, data_rx, _reader: reader })
    }

    pub async fn send(&self, msg: &SignalingMessage) -> Result<()> {
        send_message(&self.dtls, msg).await
    }

    pub async fn recv(&mut self) -> Result<SignalingMessage> {
        recv_message(&mut self.data_rx).await
    }

    pub fn close(&self) {
        self.dtls.close();
    }
}

//=== DTLS Agent (Server side) ===

pub struct DtlsAgent {
    socket: Arc<UdpSocket>,
    pub cert: Certificate,
    pub sessions: mpsc::UnboundedReceiver<DtlsAgentSession>,
    _driver: tokio::task::JoinHandle<()>,
}

pub struct DtlsAgentSession {
    pub dtls: Arc<DtlsTransport>,
    #[allow(dead_code)]
    pub data_rx: mpsc::UnboundedReceiver<Bytes>,
    #[allow(dead_code)]
    pub _peer_addr: SocketAddr,
}

impl DtlsAgent {
    pub async fn bind(addr: &str, user_cert: Option<Certificate>) -> Result<Self> {
        let socket = Arc::new(UdpSocket::bind(addr).await?);
        let cert = user_cert.unwrap_or_else(|| generate_certificate().expect("gen cert"));
        let fp = fingerprint(&cert);
        info!("DTLS agent listening on {}, fingerprint: {}", socket.local_addr()?, fp);

        let (tx, sessions) = mpsc::unbounded_channel();
        let d = Self::drive(socket.clone(), cert.clone(), tx);
        Ok(Self { socket, cert, sessions, _driver: tokio::spawn(d) })
    }

    #[allow(dead_code)]
    pub fn local_addr(&self) -> Result<SocketAddr> { Ok(self.socket.local_addr()?) }
    pub fn fingerprint(&self) -> String { fingerprint(&self.cert) }

    pub async fn accept(&mut self) -> Option<DtlsAgentSession> {
        self.sessions.recv().await
    }

    async fn drive(
        socket: Arc<UdpSocket>,
        cert: Certificate,
        session_tx: mpsc::UnboundedSender<DtlsAgentSession>,
    ) {
        use std::collections::HashMap;
        // Map peer_addr -> (IceConn, feed_task_handle)
        // Prevents duplicate sessions for the same peer during handshake
        let mut sessions: HashMap<SocketAddr, (Arc<IceConn>, tokio::task::JoinHandle<()>)> = HashMap::new();
        let mut buf = [0u8; 2000];

        loop {
            let (len, peer_addr) = match socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => { warn!("Agent recv error: {}", e); break; }
            };
            let packet = Bytes::copy_from_slice(&buf[..len]);

            // Check if this peer already has a pending handshake session
            if let Some((conn, _)) = sessions.get(&peer_addr) {
                let mut mb = Vec::new();
                PacketReceiver::receive(conn.as_ref(), packet, peer_addr, &mut mb).await;
                continue;
            }

            // New peer: create IceConn sharing the main socket
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

            tokio::spawn(runner);

            // Feed first packet
            let mut mb = Vec::new();
            PacketReceiver::receive(conn.as_ref(), packet, peer_addr, &mut mb).await;

            // Background: wait for handshake, then send session
            let dtls_c = dtls.clone();
            let conn_c = conn.clone();
            let sk = socket.clone();
            let tx2 = session_tx.clone();
            let addr = peer_addr;

            let feed_handle = tokio::spawn(async move {
                let mut state_rx = dtls_c.subscribe_state();
                loop {
                    if let DtlsState::Connected(_, _) = *state_rx.borrow() { break; }
                    if state_rx.changed().await.is_err() { return; }
                }
                info!("DTLS session established with {}", addr);
                let session = DtlsAgentSession {
                    dtls: dtls_c,
                    data_rx,
                    _peer_addr: addr,
                };
                let _ = tx2.send(session);

                // Keep feeding this peer's packets to the IceConn
                let mut buf2 = [0u8; 2000];
                loop {
                    let (len, from) = match sk.recv_from(&mut buf2).await {
                        Ok(v) => v, Err(_) => break,
                    };
                    if from != addr { continue; }
                    let pkt = Bytes::copy_from_slice(&buf2[..len]);
                    let mut mb2 = Vec::new();
                    PacketReceiver::receive(conn_c.as_ref(), pkt, from, &mut mb2).await;
                }
            });

            // Insert into sessions map to avoid duplicate processing
            sessions.insert(peer_addr, (conn, feed_handle));
        }
    }
}

//=== Tests ===

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_dtls_signaling_roundtrip() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("info")
            .try_init();

        // Agent: listen on a random port
        let mut agent = DtlsAgent::bind("127.0.0.1:0", None).await.unwrap();
        let agent_addr = agent.local_addr().unwrap().to_string();
        tracing::info!("DTLS agent listening on {}", agent_addr);

        // Client: connect to agent
        let mut client = DtlsClient::connect(&agent_addr, None).await.unwrap();

        // Agent: accept connection
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut agent_session = agent.accept().await.expect("Agent should accept connection");

        // Client sends offer
        let session_id = "test-session-1".to_string();
        client.send(&SignalingMessage::Offer {
            session_id: session_id.clone(),
            token: None,
            offer_sdp: "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n".to_string(),
            targets: Some(vec![Target { host: Some("127.0.0.1".to_string()), port: 22 }]),
        }).await.unwrap();

        // Agent receives offer
        let msg = tokio::time::timeout(Duration::from_secs(5), recv_message(&mut agent_session.data_rx)).await
            .expect("Timeout receiving offer").unwrap();
        match msg {
            SignalingMessage::Offer { session_id: sid, offer_sdp, targets, .. } => {
                assert_eq!(sid, "test-session-1");
                assert!(offer_sdp.contains("v=0"));
                let targets = targets.expect("targets should be present");
                assert_eq!(targets.len(), 1);
                assert_eq!(targets[0].host, Some("127.0.0.1".to_string()));
                assert_eq!(targets[0].port, 22);
            }
            other => panic!("Expected offer, got {:?}", other),
        }

        // Agent sends answer
        send_message(&agent_session.dtls, &SignalingMessage::Answer {
            session_id: session_id.clone(),
            answer_sdp: "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n".to_string(),
        }).await.unwrap();

        // Client receives answer
        let msg = tokio::time::timeout(Duration::from_secs(5), client.recv()).await
            .expect("Timeout receiving answer").unwrap();
        match msg {
            SignalingMessage::Answer { session_id: sid, answer_sdp } => {
                assert_eq!(sid, "test-session-1");
                assert!(answer_sdp.contains("v=0"));
            }
            other => panic!("Expected answer, got {:?}", other),
        }

        client.close();
        agent_session.dtls.close();
        tracing::info!("DTLS signaling roundtrip test passed!");
    }
}

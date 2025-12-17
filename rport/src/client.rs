use anyhow::{anyhow, Result};
use reqwest::Client;
use rustrtc::{
    transports::sctp::{DataChannel, DataChannelConfig, DataChannelEvent},
    PeerConnection, PeerConnectionEvent, SdpType, SessionDescription,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info};

use crate::webrtc_config::WebRTCConfig;
use crate::{config::IceServerConfig, OfferMessage};

pub async fn forward_stream_to_webrtc<R, W>(
    peer_connection: Arc<PeerConnection>,
    data_channel: Arc<DataChannel>,
    mut input: R,
    mut output: W,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Wait for data channel open
    // We need to spawn a task to consume events from data channel to detect Open
    let (open_tx, open_rx) = tokio::sync::oneshot::channel();
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel();

    let dc_clone = data_channel.clone();
    tokio::spawn(async move {
        let mut open_tx = Some(open_tx);
        while let Some(event) = dc_clone.recv().await {
            match event {
                DataChannelEvent::Open => {
                    if let Some(tx) = open_tx.take() {
                        let _ = tx.send(());
                    }
                }
                DataChannelEvent::Message(data) => {
                    let _ = msg_tx.send(data);
                }
                DataChannelEvent::Close => {
                    break;
                }
            }
        }
    });

    if let Err(_) = tokio::time::timeout(Duration::from_secs(10), open_rx).await {
        return Err(anyhow!("Data channel open timeout"));
    }
    let cancel_token = tokio_util::sync::CancellationToken::new();
    // Set up input -> WebRTC forwarding
    let pc_clone = peer_connection.clone();
    let dc_id = data_channel.id;

    let input_task = async move {
        let mut buffer = [0u8; 1024];

        loop {
            match input.read(&mut buffer).await {
                Ok(0) => {
                    // input closed - exit silently
                    break;
                }
                Ok(n) => {
                    let data = &buffer[..n];
                    if let Err(e) = pc_clone.send_data(dc_id, data).await {
                        // WebRTC send failed - exit silently
                        tracing::error!("Failed to send data through WebRTC: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    // input read failed - exit silently
                    tracing::debug!("forward_stream_to_webrtc: input read failed: {}", e);
                    break;
                }
            }
        }
    };

    // Set up WebRTC -> output forwarding
    let output_task = tokio::spawn(async move {
        while let Some(data) = msg_rx.recv().await {
            if output.write_all(&data).await.is_err() {
                break;
            }
            if output.flush().await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = cancel_token.cancelled() => {}
        _ = input_task => {}
        _ = output_task => {}
    }

    Ok(())
}

pub struct CliClient {
    server_url: String,
    token: String,
    client: Client,
    webrtc_config: WebRTCConfig,
}

impl CliClient {
    pub fn new(
        server_url: String,
        token: String,
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
            client: Client::new(),
            webrtc_config,
        }
    }

    pub async fn connect_proxy_command(&self, agent_id: String) -> Result<()> {
        // ProxyCommand mode - NO LOGGING to avoid SSH interference
        let (peer_connection, data_channel) =
            self.create_webrtc_connection_silent(&agent_id).await?;
        if let Err(e) = forward_stream_to_webrtc(
            peer_connection,
            data_channel,
            tokio::io::stdin(),
            tokio::io::stdout(),
        )
        .await
        {
            tracing::error!("forward_stream_to_webrtc failed: {}", e);
            return Err(e);
        }
        Ok(())
    }

    pub async fn connect_port_forward(&self, agent_id: String, local_port: u16) -> Result<()> {
        info!(
            "Starting port forward from localhost:{} to agent {}",
            local_port, agent_id
        );

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
                Err(e) => {
                    error!("Failed to accept connection: {}", e);
                }
            }
        }
    }

    async fn handle_tcp_connection(
        &self,
        mut tcp_stream: TcpStream,
        agent_id: String,
    ) -> Result<()> {
        let (close_tx, close_rx) = tokio::sync::oneshot::channel();
        let max_read_timeout = Duration::from_secs(1800); // 30 minutes
        let setup_result = async {
            // Create WebRTC connection for this TCP connection
            let (peer_connection, data_channel) = self.create_webrtc_connection(&agent_id).await?;

            let pc_clone = peer_connection.clone();
            tokio::spawn(async move {
                while let Some(event) = pc_clone.recv().await {
                    match event {
                        PeerConnectionEvent::DataChannel(dc) => {
                            tracing::debug!(
                                "CliClient PC Event: DataChannel: id={}, label={}",
                                dc.id,
                                dc.label
                            );
                        }
                        _ => {}
                    }
                }
            });

            // Wait for connection to be established
            if let Err(_) = tokio::time::timeout(
                Duration::from_secs(30),
                peer_connection.wait_for_connected(),
            )
            .await
            {
                return Err(anyhow!("WebRTC connection timeout"));
            }
            peer_connection.wait_for_connected().await?;

            // Wait for data channel open and handle messages
            let (open_tx, open_rx) = tokio::sync::oneshot::channel();
            let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel();

            let dc_clone = data_channel.clone();
            tokio::spawn(async move {
                let mut open_tx = Some(open_tx);
                while let Some(event) = dc_clone.recv().await {
                    match event {
                        DataChannelEvent::Open => {
                            if let Some(tx) = open_tx.take() {
                                let _ = tx.send(());
                            }
                        }
                        DataChannelEvent::Message(data) => {
                            let _ = msg_tx.send(data);
                        }
                        DataChannelEvent::Close => {
                            let _ = close_tx.send(());
                            break;
                        }
                    }
                }
            });

            if let Err(_) = tokio::time::timeout(Duration::from_secs(10), open_rx).await {
                return Err(anyhow!("Data channel open timeout"));
            }

            Ok((peer_connection, data_channel, msg_rx))
        }
        .await;

        let (peer_connection, data_channel, mut msg_rx) = match setup_result {
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

        // Split the TCP stream
        let (mut tcp_read, mut tcp_write) = tcp_stream.into_split();

        let pc_clone = peer_connection.clone();
        let dc_id = data_channel.id;

        let tcp_to_webrtc = async move {
            let mut buffer = [0u8; 1024];
            loop {
                let r = tokio::time::timeout(max_read_timeout, tcp_read.read(&mut buffer)).await?;
                match r {
                    Ok(0) => {
                        info!("TCP connection closed by client");
                        break;
                    }
                    Ok(n) => {
                        let data = &buffer[..n];
                        if let Err(e) = pc_clone.send_data(dc_id, data).await {
                            error!("Failed to send data through WebRTC: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Failed to read from TCP: {}", e);
                        break;
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        };

        // Set up WebRTC -> TCP forwarding
        let webrtc_to_tcp = async move {
            while let Some(data) = msg_rx.recv().await {
                if let Err(e) = tcp_write.write_all(&data).await {
                    error!("Failed to write to TCP: {}", e);
                    break;
                }
                if let Err(e) = tcp_write.flush().await {
                    error!("Failed to flush TCP: {}", e);
                    break;
                }
            }
        };

        // Wait for either direction to close
        tokio::select! {
            _ = close_rx => {
                info!("Data channel closed");
            }
            _ = tcp_to_webrtc => {
                info!("TCP to WebRTC forwarding ended");
            }
            _ = webrtc_to_tcp => {
                info!("WebRTC to TCP forwarding ended");
            }
        }

        peer_connection.close();
        Ok(())
    }

    async fn create_webrtc_connection(
        &self,
        agent_id: &str,
    ) -> Result<(Arc<PeerConnection>, Arc<DataChannel>)> {
        info!("Creating WebRTC peer connection for agent: {}", agent_id);

        // Create WebRTC peer connection
        let peer_connection = self.create_peer_connection().await?;

        // Create a data channel before creating the offer
        let data_channel_config = DataChannelConfig {
            ordered: true,
            ..Default::default()
        };
        let data_channel =
            peer_connection.create_data_channel("port-forward", Some(data_channel_config))?;

        // Create offer
        let offer = peer_connection.create_offer()?;
        peer_connection.set_local_description(offer.clone())?;

        // Wait for ICE gathering
        if let Err(_) = tokio::time::timeout(
            Duration::from_secs(3),
            peer_connection.wait_for_gathering_complete(),
        )
        .await
        {
            info!("ICE gathering timed out, proceeding with gathered candidates");
        }

        let offer = peer_connection
            .local_description()
            .ok_or_else(|| anyhow!("Failed to get local description after ICE gathering"))?;
        let sdp = offer.to_sdp_string();
        // Filter out IPv6 candidates for compatibility
        let offer_sdp = sdp
            .lines()
            .filter(|l| !l.contains("IP6") && !l.contains("::"))
            .collect::<Vec<_>>()
            .join("\r\n");

        // Send offer to server
        let offer_msg = OfferMessage {
            id: agent_id.to_string(),
            offer: offer_sdp,
        };

        info!("Sending offer to signaling server...");
        let url = format!("{}/rport/offer?token={}", self.server_url, self.token);
        let response = self.client.post(&url).json(&offer_msg).send().await?;

        if !response.status().is_success() {
            return Err(anyhow!("Failed to send offer: {}", response.status()));
        }

        let response_body: Value = response.json().await?;
        let answer_sdp = response_body["answer"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing answer in response"))?;

        // Set remote description from answer
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
            while let Some(event) = pc_clone.recv().await {
                match event {
                    PeerConnectionEvent::DataChannel(_) => {}
                    _ => {}
                }
            }
        });

        let data_channel_config = DataChannelConfig {
            ordered: true,
            ..Default::default()
        };
        let data_channel =
            peer_connection.create_data_channel("port-forward", Some(data_channel_config))?;
        let offer = peer_connection.create_offer()?;
        peer_connection.set_local_description(offer.clone())?;

        if let Err(_) = tokio::time::timeout(
            Duration::from_secs(3),
            peer_connection.wait_for_gathering_complete(),
        )
        .await
        {
            info!("ICE gathering timed out, proceeding with gathered candidates");
        }

        let offer = peer_connection
            .local_description()
            .ok_or_else(|| anyhow!("Failed to get local description after ICE gathering"))?;

        // Strip IPv6 candidates from offer
        let offer_sdp = offer.to_sdp_string();
        let url = format!("{}/rport/offer?token={}", self.server_url, self.token);
        tracing::debug!(
            "create_webrtc_connection_silent: sending offer to {} \n {}",
            url,
            offer_sdp
        );

        let offer_msg = OfferMessage {
            id: agent_id.to_string(),
            offer: offer_sdp,
        };
        let response = self
            .client
            .post(&url)
            .timeout(Duration::from_secs(10))
            .json(&offer_msg)
            .send()
            .await?;

        if !response.status().is_success() {
            tracing::error!("Failed to send offer: {}", response.status());
            return Err(anyhow!("Failed to send offer: {}", response.status()));
        }

        let response_body: Value = response.json().await?;
        let answer_sdp = response_body["answer"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing answer in response"))?;

        // Set remote description from answer
        let answer = SessionDescription::parse(SdpType::Answer, &answer_sdp)?;
        peer_connection.set_remote_description(answer).await?;
        Ok((peer_connection, data_channel))
    }

    async fn create_peer_connection(&self) -> Result<Arc<PeerConnection>> {
        self.webrtc_config.create_peer_connection().await
    }
}

impl Clone for CliClient {
    fn clone(&self) -> Self {
        Self {
            server_url: self.server_url.clone(),
            token: self.token.clone(),
            client: Client::new(),
            webrtc_config: self.webrtc_config.clone(),
        }
    }
}

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

        // Setup "Agent" side WebRTC
        let config = RtcConfiguration::default();
        let agent_pc = Arc::new(PeerConnection::new(config));
        agent_pc.add_transceiver(
            rustrtc::MediaKind::Application,
            rustrtc::TransceiverDirection::SendRecv,
        );

        // Setup Mock Signaling Server
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let local_addr = listener.local_addr()?;
        let server_url = format!("http://{}", local_addr);

        let agent_pc_clone = agent_pc.clone();

        // Spawn the mock server
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };

                let agent_pc = agent_pc_clone.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let n = match socket.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };

                    let req = String::from_utf8_lossy(&buf[..n]);

                    if req.contains("GET /rport/iceservers") {
                        let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]";
                        socket.write_all(response.as_bytes()).await.unwrap();
                        return;
                    }

                    if req.contains("POST /rport/offer") {
                        // Find body
                        if let Some(idx) = req.find("\r\n\r\n") {
                            let body = &req[idx + 4..];

                            if let Ok(offer_msg) = serde_json::from_str::<OfferMessage>(body) {
                                // Handle WebRTC negotiation
                                let offer =
                                    SessionDescription::parse(SdpType::Offer, &offer_msg.offer)
                                        .unwrap();
                                agent_pc.set_remote_description(offer).await.unwrap();

                                let answer = agent_pc.create_answer().unwrap();
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
                                    response_body.len(),
                                    response_body
                                );
                                socket.write_all(response.as_bytes()).await.unwrap();
                            }
                        }
                    }
                });
            }
        });

        // Setup Client
        let client = CliClient::new(server_url, "test-token".to_string(), None);

        // Run connect_port_forward in background
        let client_clone = client.clone();
        tokio::spawn(async move {
            if let Err(e) = client_clone
                .connect_port_forward("gpu03".to_string(), 4023)
                .await
            {
                eprintln!("connect_port_forward failed: {}", e);
            }
        });

        // Wait for listener to be ready
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Connect to the forwarded port
        info!("Connecting to 127.0.0.1:4023");
        let _stream = TcpStream::connect("127.0.0.1:4023").await?;

        // Verify connection on Agent side
        // Wait for DataChannel from Client (DCEP)
        let (dc_tx, dc_rx) = tokio::sync::oneshot::channel();

        let agent_pc_clone = agent_pc.clone();
        tokio::spawn(async move {
            let mut dc_tx = Some(dc_tx);
            while let Some(event) = agent_pc_clone.recv().await {
                if let PeerConnectionEvent::DataChannel(dc) = event {
                    if let Some(tx) = dc_tx.take() {
                        let _ = tx.send(dc);
                    }
                }
            }
        });

        info!("Waiting for Agent PC connection...");
        agent_pc.wait_for_connected().await.unwrap();
        info!("Agent PC connected!");

        let dc = tokio::time::timeout(Duration::from_secs(5), dc_rx).await??;

        // Wait for DC open
        let (open_tx, open_rx) = tokio::sync::oneshot::channel();
        let dc_clone = dc.clone();
        tokio::spawn(async move {
            let mut open_tx = Some(open_tx);
            while let Some(event) = dc_clone.recv().await {
                if let DataChannelEvent::Open = event {
                    if let Some(tx) = open_tx.take() {
                        let _ = tx.send(());
                    }
                }
            }
        });

        tokio::time::timeout(Duration::from_secs(5), open_rx).await??;
        info!("DataChannel Open!");
        Ok(())
    }
}

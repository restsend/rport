use anyhow::{anyhow, Result};
use reqwest::Client;
use rport_common::*;
use rustrtc::{
    transports::sctp::{DataChannel, DataChannelConfig, DataChannelEvent},
    PeerConnection, PeerConnectionEvent, SdpType, SessionDescription,
};
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info};

use crate::config::IceServerConfig;
use crate::webrtc_config::WebRTCConfig;

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

    open_rx.await?;

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
                    if pc_clone.send_data(dc_id, data).await.is_err() {
                        // WebRTC send failed - exit silently
                        break;
                    }
                }
                Err(_) => {
                    // input read failed - exit silently
                    break;
                }
            }
        }
    };

    // Set up WebRTC -> output forwarding
    let output_task = tokio::spawn(async move {
        while let Some(data) = msg_rx.recv().await {
            if output.write_all(&data).await.is_err() {
                // output write failed - exit silently
                break;
            }
            if output.flush().await.is_err() {
                // output flush failed - exit silently
                break;
            }
        }
    });

    // Wait for either task to complete (silently)
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

        // Create WebRTC connection (silently)
        let (peer_connection, data_channel) =
            self.create_webrtc_connection_silent(&agent_id).await?;

        // Wait for connection to be established (silently)
        peer_connection.wait_for_connection().await?;

        forward_stream_to_webrtc(
            peer_connection,
            data_channel,
            tokio::io::stdin(),
            tokio::io::stdout(),
        )
        .await?;

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

    async fn handle_tcp_connection(&self, tcp_stream: TcpStream, agent_id: String) -> Result<()> {
        // Create WebRTC connection for this TCP connection
        let (peer_connection, data_channel) = self.create_webrtc_connection(&agent_id).await?;

        // Spawn a task to keep the peer connection alive and process events
        let pc_clone = peer_connection.clone();
        tokio::spawn(async move {
            while let Some(event) = pc_clone.recv().await {
                tracing::info!("CliClient PC Event received");
                if let PeerConnectionEvent::DataChannel(_) = event {
                    tracing::info!("CliClient PC Event: DataChannel (unexpected)");
                }
            }
        });

        // Wait for connection to be established
        peer_connection.wait_for_connection().await?;

        // Wait for data channel open and handle messages
        let (open_tx, open_rx) = tokio::sync::oneshot::channel();
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel();

        let dc_clone = data_channel.clone();
        tokio::spawn(async move {
            let mut open_tx = Some(open_tx);
            while let Some(event) = dc_clone.recv().await {
                tracing::info!("CliClient DC Event received");
                match event {
                    DataChannelEvent::Open => {
                        tracing::info!("CliClient DC Open");
                        if let Some(tx) = open_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                    DataChannelEvent::Message(data) => {
                        let _ = msg_tx.send(data);
                    }
                    DataChannelEvent::Close => {
                        tracing::info!("CliClient DC Close");
                        break;
                    }
                }
            }
        });

        open_rx.await?;

        info!("WebRTC connection established for TCP client");

        // Split the TCP stream
        let (mut tcp_read, mut tcp_write) = tcp_stream.into_split();

        let pc_clone = peer_connection.clone();
        let dc_id = data_channel.id;

        let tcp_to_webrtc = async move {
            let mut buffer = [0u8; 1024];

            loop {
                match tcp_read.read(&mut buffer).await {
                    Ok(0) => {
                        info!("TCP connection closed");
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
            _ = tcp_to_webrtc => {
                info!("TCP to WebRTC forwarding ended");
            }
            _ = webrtc_to_tcp => {
                info!("WebRTC to TCP forwarding ended");
            }
        }

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
            max_retransmits: Some(10),
            ..Default::default()
        };
        let data_channel =
            peer_connection.create_data_channel("port-forward", Some(data_channel_config))?;

        // Create offer
        let offer = peer_connection.create_offer().await?;
        peer_connection.set_local_description(offer.clone())?;

        // Wait for ICE gathering
        peer_connection.wait_for_gathering_complete().await;

        let offer = peer_connection
            .local_description()
            .ok_or_else(|| anyhow!("Failed to get local description after ICE gathering"))?;
        let offer_sdp = offer.to_sdp_string();
        // Send offer to server
        let offer_msg = OfferMessage {
            id: agent_id.to_string(),
            offer: offer_sdp,
        };

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
        // Silent version for ProxyCommand mode - no logging

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
        let offer = peer_connection.create_offer().await?;
        peer_connection.set_local_description(offer.clone())?;

        // Wait for ICE gathering
        peer_connection.wait_for_gathering_complete().await;

        let offer = peer_connection
            .local_description()
            .ok_or_else(|| anyhow!("Failed to get local description after ICE gathering"))?;

        // Strip IPv6 candidates from offer
        let offer_sdp = offer.to_sdp_string();
        // Send offer to server
        let offer_msg = OfferMessage {
            id: agent_id.to_string(),
            offer: offer_sdp,
        };

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

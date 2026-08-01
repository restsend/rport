use std::sync::Arc;

use rport::{CliClient, ForwardMapping, ForwardStats, RportConfig, forward_stream_to_webrtc};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::ClientConfig;

#[derive(Debug, Clone, PartialEq)]
pub enum ClientStatus {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

pub struct ManagedClient {
    pub config: ClientConfig,
    pub status: watch::Receiver<ClientStatus>,
    status_tx: watch::Sender<ClientStatus>,
    pub stats: Arc<ForwardStats>,
    cancel: CancellationToken,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl ManagedClient {
    pub fn new(config: ClientConfig) -> Self {
        let (status_tx, status) = watch::channel(ClientStatus::Disconnected);
        Self {
            stats: Arc::new(ForwardStats::default()),
            cancel: CancellationToken::new(),
            handle: None,
            config,
            status,
            status_tx,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(*self.status.borrow(), ClientStatus::Connecting | ClientStatus::Connected)
    }

    pub fn start(&mut self) {
        if self.is_running() {
            return;
        }

        self.cancel = CancellationToken::new();
        let cancel = self.cancel.clone();
        let config = self.config.clone();
        let status_tx = self.status_tx.clone();
        let stats = self.stats.clone();

        status_tx.send(ClientStatus::Connecting).ok();

        self.handle = Some(tokio::spawn(async move {
            let forwards: Vec<ForwardMapping> = config
                .forwards
                .iter()
                .filter(|f| f.enabled)
                .map(|f| ForwardMapping {
                    local_port: Some(f.local_port),
                    host: f.remote_host.clone(),
                    port: f.remote_port,
                })
                .collect();

            if forwards.is_empty() {
                status_tx
                    .send(ClientStatus::Failed("No enabled forward rules".to_string()))
                    .ok();
                return;
            }

            let client = CliClient::new(
                &config.server_addr,
                &config.token,
                &config.agent_id,
                None,
                false,
                false,
                &RportConfig::default(),
            );

            let mut listener_handles = Vec::new();

            for fwd in &forwards {
                let local_port = fwd.local_port.unwrap();
                let listener = match tokio::net::TcpListener::bind(format!("127.0.0.1:{}", local_port)).await
                {
                    Ok(l) => l,
                    Err(e) => {
                        status_tx
                            .send(ClientStatus::Failed(format!(
                                "Cannot bind 127.0.0.1:{}: {}",
                                local_port, e
                            )))
                            .ok();
                        return;
                    }
                };

                let cancel = cancel.clone();
                let client = client.clone();
                let host = fwd.host.clone();
                let port = fwd.port;
                let stats = stats.clone();
                let status_tx = status_tx.clone();

                listener_handles.push(tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            result = listener.accept() => {
                                match result {
                                    Ok((tcp_stream, addr)) => {
                                        tracing::info!("Connection from {}", addr);
                                        let (reader, writer) = tcp_stream.into_split();
                                        let client = client.clone();
                                        let host = host.clone();
                                        let stats = stats.clone();
                                        let status_tx = status_tx.clone();
                                        tokio::spawn(async move {
                                            match client.establish_webrtc(&host, port).await {
                                                Ok((pc, dc, remote_rx)) => {
                                                    status_tx.send(ClientStatus::Connected).ok();
                                                    if let Err(e) = forward_stream_to_webrtc(
                                                        pc, dc, Some(30), Some(stats),
                                                        format!("{}:{}", host, port),
                                                        reader, writer, remote_rx,
                                                    ).await {
                                                        tracing::error!("Forward error: {}", e);
                                                    }
                                                }
                                                Err(e) => {
                                                    tracing::error!("WebRTC error: {}", e);
                                                }
                                            }
                                        });
                                    }
                                    Err(e) => {
                                        tracing::error!("Accept error: {}", e);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }));
            }

            cancel.cancelled().await;

            for h in listener_handles {
                h.abort();
            }

            status_tx.send(ClientStatus::Disconnected).ok();
        }));
    }

    pub fn stop(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        self.status_tx.send(ClientStatus::Disconnected).ok();
    }
}

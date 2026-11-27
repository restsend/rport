use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use rustrtc::transports::sctp::{DataChannelConfig, DataChannelEvent};
use rustrtc::{IceCandidate, IceGatheringState, PeerConnection, RtcConfiguration, SessionDescription, SdpType};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::dtls_signaling::*;

fn test_rtc_config() -> RtcConfiguration {
    RtcConfiguration {
        ice_servers: vec![],
        sctp_rto_initial: Duration::from_millis(400),
        sctp_rto_min: Duration::from_millis(200),
        sctp_rto_max: Duration::from_secs(30),
        sctp_max_association_retransmits: 20,
        sctp_receive_window: 2 * 1024 * 1024,
        ice_connection_timeout: Duration::from_secs(30),
        ice_disconnect_threshold: Duration::from_secs(10),
        enable_upnp: false,
        prefer_srflx_over_natted_host: false,
        ..RtcConfiguration::webrtc()
    }
}

#[tokio::test]
async fn test_webrtc_e2e_data_flow() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // === Step 1: TCP echo server ===
    let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo_listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut tcp, _) = echo_listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    match tcp.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if tcp.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    // === Step 2: DTLS signaling server ===
    let mut server = DtlsAgent::bind("127.0.0.1:0", None)
        .await
        .unwrap();
    let server_addr = server.local_addr().unwrap();
    tracing::info!("Signaling server on {}", server_addr);

    // === Step 3: Agent connects ===
    let agent_dtls = DtlsClient::connect(&server_addr.to_string(), None)
        .await
        .unwrap();
    let agent_session = server.accept().await.expect("agent DTLS connect");

    // === Step 4: Client connects ===
    let client_dtls = DtlsClient::connect(&server_addr.to_string(), None)
        .await
        .unwrap();
    let client_session = server.accept().await.expect("client DTLS connect");

    tracing::info!("DTLS sessions established");

    // === Step 5: Bidirectional relay ===
    let agent_tx = agent_session.dtls.clone();
    let client_tx = client_session.dtls.clone();

    let _relay_agent_to_client = {
        let mut agent_rx = agent_session.data_rx;
        let mut agent_buf = BytesMut::new();
        let c_tx = client_tx.clone();
        tokio::spawn(async move {
            loop {
                match recv_message(&mut agent_rx, &mut agent_buf).await {
                    Ok(msg) => {
                        tracing::debug!("Relay agent->client: {:?}", msg);
                        if send_message(&c_tx, &msg).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
    };

    let _relay_client_to_agent = {
        let mut client_rx = client_session.data_rx;
        let mut client_buf = BytesMut::new();
        let a_tx = agent_tx.clone();
        tokio::spawn(async move {
            loop {
                match recv_message(&mut client_rx, &mut client_buf).await {
                    Ok(msg) => {
                        tracing::debug!("Relay client->agent: {:?}", msg);
                        if send_message(&a_tx, &msg).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
    };

    // === Step 6: Agent-side WebRTC ===
    let agent_pc = Arc::new(PeerConnection::new(test_rtc_config()));
    let dc_label = format!("fwd:127.0.0.1:{}", echo_addr.port());

    let agent_dc = agent_pc
        .create_data_channel(
            &dc_label,
            Some(DataChannelConfig {
                ordered: true,
                label: dc_label.clone(),
                ..Default::default()
            }),
        )
        .unwrap();

    let (tcp_msg_tx, tcp_msg_rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();

    let agent_pc_dc = agent_pc.clone();
    let echo_addr_dc = echo_addr;
    tokio::spawn(async move {
        let dc_id = agent_dc.id;
        let mut tcp_rx = Some(tcp_msg_rx);
        loop {
            let event = match tokio::time::timeout(Duration::from_secs(15), agent_dc.recv()).await
            {
                Ok(Some(e)) => e,
                Ok(None) | Err(_) => break,
            };
            match event {
                DataChannelEvent::Open => {
                    tracing::info!("Agent data channel open");
                    if let Some(mut rx) = tcp_rx.take() {
                        if let Ok(tcp) = TcpStream::connect(echo_addr_dc).await {
                            let (mut tcp_r, mut tcp_w) = tcp.into_split();
                            let pc_tcp = agent_pc_dc.clone();
                            tokio::spawn(async move {
                                let mut buf = [0u8; 1024];
                                loop {
                                    match tcp_r.read(&mut buf).await {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => {
                                            if pc_tcp.send_data(dc_id, &buf[..n]).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                            });
                            tokio::spawn(async move {
                                while let Some(data) = rx.recv().await {
                                    if tcp_w.write_all(&data).await.is_err() {
                                        break;
                                    }
                                    let _ = tcp_w.flush().await;
                                }
                            });
                        } else {
                            tracing::error!("Failed to connect to echo server");
                        }
                    }
                }
                DataChannelEvent::Message(data) => {
                    let _ = tcp_msg_tx.send(Bytes::from(data));
                }
                DataChannelEvent::BufferedAmountLow(_) => {}
                DataChannelEvent::Close => {
                    tracing::info!("Agent data channel closed");
                    break;
                }
            }
        }
    });

    let _agent_drain = {
        let pc = agent_pc.clone();
        tokio::spawn(async move {
            while let Some(_) = pc.recv().await {}
        })
    };

    // === Step 7: Client-side WebRTC ===
    let client_pc = Arc::new(PeerConnection::new(test_rtc_config()));
    let client_dc = client_pc
        .create_data_channel(
            &dc_label,
            Some(DataChannelConfig {
                ordered: true,
                label: dc_label.clone(),
                ..Default::default()
            }),
        )
        .unwrap();

    let _client_drain = {
        let pc = client_pc.clone();
        tokio::spawn(async move {
            while let Some(_) = pc.recv().await {}
        })
    };

    // === Step 8: Trickle ICE SDP exchange through relay ===
    let session_id = "e2e-test-session".to_string();
    let handshake_start = Instant::now();

    // Forwarding tasks for trickle ICE
    let forward_client_candidates = {
        let mut candidate_rx = client_pc.subscribe_ice_candidates();
        let mut state_rx = client_pc.subscribe_ice_gathering_state();
        let dtls = client_dtls.dtls.clone();
        let sid = session_id.clone();
        tokio::spawn(async move {
            if *state_rx.borrow() == IceGatheringState::Complete {
                let _ = send_message(&dtls, &SignalingMessage::new_end_of_candidates(sid.clone())).await;
                return;
            }
            loop {
                tokio::select! {
                    result = candidate_rx.recv() => match result {
                        Ok(c) => {
                            send_message(&dtls, &SignalingMessage::new_candidate(
                                sid.clone(), c.to_sdp(),
                            )).await.ok();
                            if *state_rx.borrow() == IceGatheringState::Complete {
                                send_message(&dtls, &SignalingMessage::new_end_of_candidates(sid.clone())).await.ok();
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = state_rx.changed() => {
                        if *state_rx.borrow() == IceGatheringState::Complete {
                            send_message(&dtls, &SignalingMessage::new_end_of_candidates(sid.clone())).await.ok();
                            break;
                        }
                    }
                }
            }
        })
    };

    let forward_agent_candidates = {
        let mut candidate_rx = agent_pc.subscribe_ice_candidates();
        let mut state_rx = agent_pc.subscribe_ice_gathering_state();
        let dtls = agent_dtls.dtls.clone();
        let sid = session_id.clone();
        tokio::spawn(async move {
            if *state_rx.borrow() == IceGatheringState::Complete {
                let _ = send_message(&dtls, &SignalingMessage::new_end_of_candidates(sid.clone())).await;
                return;
            }
            loop {
                tokio::select! {
                    result = candidate_rx.recv() => match result {
                        Ok(c) => {
                            send_message(&dtls, &SignalingMessage::new_candidate(
                                sid.clone(), c.to_sdp(),
                            )).await.ok();
                            if *state_rx.borrow() == IceGatheringState::Complete {
                                send_message(&dtls, &SignalingMessage::new_end_of_candidates(sid.clone())).await.ok();
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = state_rx.changed() => {
                        if *state_rx.borrow() == IceGatheringState::Complete {
                            send_message(&dtls, &SignalingMessage::new_end_of_candidates(sid.clone())).await.ok();
                            break;
                        }
                    }
                }
            }
        })
    };

    // Agent message handler (runs in background)
    let _agent_handler = {
        let pc = agent_pc.clone();
        let dtls_tx = agent_dtls.dtls.clone();
        let mut data_rx = agent_dtls.data_rx;
        let mut data_buf = BytesMut::new();
        let sid = session_id.clone();
        tokio::spawn(async move {
            loop {
                let msg = match recv_message(&mut data_rx, &mut data_buf).await {
                    Ok(m) => m,
                    Err(_) => break,
                };
                match msg {
                    SignalingMessage::Offer { offer_sdp, targets, .. } => {
                        tracing::info!("Agent received offer");
                        if let Some(tgts) = targets {
                            for t in &tgts {
                                tracing::info!("  target: {}:{}", t.host.as_deref().unwrap_or("?"), t.port);
                            }
                        }
                        let remote_offer = SessionDescription::parse(SdpType::Offer, &offer_sdp).unwrap();
                        pc.set_remote_description(remote_offer).await.unwrap();
                        let answer = pc.create_answer().await.unwrap();
                        pc.set_local_description(answer).unwrap();
                        let answer_sdp = pc.local_description().unwrap().to_sdp_string();
                        send_message(&dtls_tx, &SignalingMessage::new_answer(
                            sid.clone(),
                            answer_sdp,
                        )).await.unwrap();
                        tracing::info!("Agent sent answer");
                    }
                    SignalingMessage::Candidate { candidate, .. } => {
                        if let Ok(c) = IceCandidate::from_sdp(&candidate) {
                            pc.add_ice_candidate(c).ok();
                        }
                    }
                    SignalingMessage::EndOfCandidates { .. } => {}
                    _ => {}
                }
            }
        })
    };

    // Client sends offer immediately (no wait for gathering)
    let offer = client_pc.create_offer().await.unwrap();
    client_pc.set_local_description(offer).unwrap();
    let offer_sdp = client_pc.local_description().unwrap().to_sdp_string();

    send_message(
        &client_dtls.dtls,
        &SignalingMessage::new_offer(
            session_id.clone(),
            "test-agent".to_string(),
            offer_sdp,
            Some(vec![Target {
                host: Some("127.0.0.1".to_string()),
                port: echo_addr.port(),
                proto: None,
            }]),
        ),
    )
    .await
    .unwrap();

    // Client processes incoming messages until answer received
    let mut client_rx = client_dtls.data_rx;
    let mut client_buf = BytesMut::new();
    loop {
        let msg = recv_message(&mut client_rx, &mut client_buf).await.unwrap();
        match msg {
            SignalingMessage::Answer { answer_sdp, .. } => {
                tracing::info!("Client received answer in {:?}", handshake_start.elapsed());
                let remote_answer = SessionDescription::parse(SdpType::Answer, &answer_sdp).unwrap();
                client_pc.set_remote_description(remote_answer).await.unwrap();
                tracing::info!("Trickle ICE handshake completed in {:?}", handshake_start.elapsed());
                break;
            }
            SignalingMessage::Candidate { candidate, .. } => {
                if let Ok(c) = IceCandidate::from_sdp(&candidate) {
                    client_pc.add_ice_candidate(c).ok();
                }
            }
            SignalingMessage::EndOfCandidates { .. } => {}
            _ => {}
        }
    }

    drop(forward_client_candidates);
    drop(forward_agent_candidates);

    // === Step 9: Wait for WebRTC connection ===
    let mut client_state_rx = client_pc.subscribe_peer_state();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let rustrtc::PeerConnectionState::Connected = *client_state_rx.borrow() {
                break;
            }
            if let rustrtc::PeerConnectionState::Failed = *client_state_rx.borrow() {
                panic!("WebRTC connection failed");
            }
            client_state_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("WebRTC should connect within 30s");
    tracing::info!("WebRTC connected!");

    // === Step 10: Verify data flow ===
    let (recv_tx, mut recv_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let client_dc_clone = client_dc.clone();
    tokio::spawn(async move {
        while let Some(event) = client_dc_clone.recv().await {
            match event {
                DataChannelEvent::Message(data) => {
                    let _ = recv_tx.send(data.to_vec());
                }
                DataChannelEvent::Close => break,
                _ => {}
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(1000)).await;

    let test_msg = b"Hello WebRTC E2E!";
    client_pc
        .send_data(client_dc.id, test_msg)
        .await
        .unwrap();
    tracing::info!("Sent test data to data channel");

    let response = tokio::time::timeout(Duration::from_secs(10), recv_rx.recv())
        .await
        .expect("Should receive echoed data within timeout")
        .expect("Response channel should not be closed");

    assert_eq!(&response, test_msg, "Echoed data should match sent data");
    tracing::info!("E2E test PASSED: data echoed correctly");
}

/// E2E: UDP forwarding over an unordered, zero-retransmit data channel.
/// Each datagram must arrive as exactly one data-channel message (SCTP
/// message boundaries), including datagrams larger than one SCTP chunk.
#[tokio::test]
async fn test_webrtc_e2e_udp_forwarding() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // === Step 1: UDP echo server ===
    let echo_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo_sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        while let Ok((n, peer)) = echo_sock.recv_from(&mut buf).await {
            if echo_sock.send_to(&buf[..n], peer).await.is_err() {
                break;
            }
        }
    });

    // === Step 2: DTLS signaling server ===
    let mut server = DtlsAgent::bind("127.0.0.1:0", None).await.unwrap();
    let server_addr = server.local_addr().unwrap();

    // === Step 3/4: agent + client DTLS sessions ===
    let agent_dtls = DtlsClient::connect(&server_addr.to_string(), None).await.unwrap();
    let agent_session = server.accept().await.expect("agent DTLS connect");
    let client_dtls = DtlsClient::connect(&server_addr.to_string(), None).await.unwrap();
    let client_session = server.accept().await.expect("client DTLS connect");

    // === Step 5: bidirectional signaling relay ===
    {
        let mut agent_rx = agent_session.data_rx;
        let mut buf = BytesMut::new();
        let c_tx = client_session.dtls.clone();
        tokio::spawn(async move {
            loop {
                match recv_message(&mut agent_rx, &mut buf).await {
                    Ok(msg) => { if send_message(&c_tx, &msg).await.is_err() { break; } }
                    Err(_) => break,
                }
            }
        });
    }
    {
        let mut client_rx = client_session.data_rx;
        let mut buf = BytesMut::new();
        let a_tx = agent_session.dtls.clone();
        tokio::spawn(async move {
            loop {
                match recv_message(&mut client_rx, &mut buf).await {
                    Ok(msg) => { if send_message(&a_tx, &msg).await.is_err() { break; } }
                    Err(_) => break,
                }
            }
        });
    }

    // === Step 6: agent-side WebRTC — UDP-configured data channel ===
    let agent_pc = Arc::new(PeerConnection::new(test_rtc_config()));
    let dc_label = format!("fwd:udp:127.0.0.1:{}", echo_addr.port());
    let udp_dc_config = DataChannelConfig {
        ordered: false,
        max_retransmits: Some(0),
        label: dc_label.clone(),
        ..Default::default()
    };
    let agent_dc = agent_pc
        .create_data_channel(&dc_label, Some(udp_dc_config.clone()))
        .unwrap();

    // Agent UDP pump: each DC message is one datagram; each received
    // datagram is one DC message (mirrors agent::run_udp_forward).
    let agent_pc_dc = agent_pc.clone();
    let _agent_drain = {
        let pc = agent_pc.clone();
        tokio::spawn(async move { while let Some(_) = pc.recv().await {} })
    };
    tokio::spawn(async move {
        let dc_id = agent_dc.id;
        let udp = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
        udp.connect(echo_addr).await.unwrap();
        let mut buf = vec![0u8; 65535];
        loop {
            tokio::select! {
                data = agent_dc.recv() => {
                    match data {
                        Some(DataChannelEvent::Message(data)) => {
                            if data.is_empty() { break; }
                            if udp.send(&data).await.is_err() { break; }
                        }
                        Some(DataChannelEvent::Close) | None => break,
                        _ => {}
                    }
                }
                recv = udp.recv(&mut buf) => {
                    match recv {
                        Ok(0) => continue,
                        Ok(n) => {
                            if agent_pc_dc.send_data(dc_id, &buf[..n]).await.is_err() { break; }
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    });

    // === Step 7: client-side WebRTC ===
    let client_pc = Arc::new(PeerConnection::new(test_rtc_config()));
    let client_dc = client_pc
        .create_data_channel(&dc_label, Some(udp_dc_config))
        .unwrap();

    let _client_drain = {
        let pc = client_pc.clone();
        tokio::spawn(async move { while let Some(_) = pc.recv().await {} })
    };

    // === Step 8: trickle ICE exchange (same as TCP test) ===
    let session_id = "e2e-udp-test-session".to_string();
    for (pc, dtls) in [(client_pc.clone(), client_dtls.dtls.clone()), (agent_pc.clone(), agent_dtls.dtls.clone())] {
        let mut candidate_rx = pc.subscribe_ice_candidates();
        let mut state_rx = pc.subscribe_ice_gathering_state();
        let dtls = dtls.clone();
        let sid = session_id.clone();
        tokio::spawn(async move {
            if *state_rx.borrow() == IceGatheringState::Complete {
                let _ = send_message(&dtls, &SignalingMessage::new_end_of_candidates(sid)).await;
                return;
            }
            loop {
                tokio::select! {
                    result = candidate_rx.recv() => match result {
                        Ok(c) => {
                            send_message(&dtls, &SignalingMessage::new_candidate(sid.clone(), c.to_sdp())).await.ok();
                            if *state_rx.borrow() == IceGatheringState::Complete {
                                send_message(&dtls, &SignalingMessage::new_end_of_candidates(sid)).await.ok();
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = state_rx.changed() => {
                        if *state_rx.borrow() == IceGatheringState::Complete {
                            send_message(&dtls, &SignalingMessage::new_end_of_candidates(sid)).await.ok();
                            break;
                        }
                    }
                }
            }
        });
    }

    // Agent message handler
    let _agent_handler = {
        let pc = agent_pc.clone();
        let dtls_tx = agent_dtls.dtls.clone();
        let mut data_rx = agent_dtls.data_rx;
        let mut data_buf = BytesMut::new();
        let sid = session_id.clone();
        tokio::spawn(async move {
            loop {
                let msg = match recv_message(&mut data_rx, &mut data_buf).await {
                    Ok(m) => m,
                    Err(_) => break,
                };
                match msg {
                    SignalingMessage::Offer { offer_sdp, .. } => {
                        let remote_offer = SessionDescription::parse(SdpType::Offer, &offer_sdp).unwrap();
                        pc.set_remote_description(remote_offer).await.unwrap();
                        let answer = pc.create_answer().await.unwrap();
                        pc.set_local_description(answer).unwrap();
                        let answer_sdp = pc.local_description().unwrap().to_sdp_string();
                        send_message(&dtls_tx, &SignalingMessage::new_answer(sid.clone(), answer_sdp)).await.unwrap();
                    }
                    SignalingMessage::Candidate { candidate, .. } => {
                        if let Ok(c) = IceCandidate::from_sdp(&candidate) {
                            pc.add_ice_candidate(c).ok();
                        }
                    }
                    _ => {}
                }
            }
        })
    };

    let offer = client_pc.create_offer().await.unwrap();
    client_pc.set_local_description(offer).unwrap();
    let offer_sdp = client_pc.local_description().unwrap().to_sdp_string();
    send_message(
        &client_dtls.dtls,
        &SignalingMessage::new_offer(
            session_id.clone(),
            "test-agent".to_string(),
            offer_sdp,
            Some(vec![crate::dtls_signaling::Target::udp(
                Some("127.0.0.1".to_string()),
                echo_addr.port(),
            )]),
        ),
    )
    .await
    .unwrap();

    // Client: wait for answer + candidates, then Connected
    let mut client_rx = client_dtls.data_rx;
    let mut client_buf = BytesMut::new();
    let mut client_state_rx = client_pc.subscribe_peer_state();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                msg = recv_message(&mut client_rx, &mut client_buf) => {
                    match msg.unwrap() {
                        SignalingMessage::Answer { answer_sdp, .. } => {
                            let answer = SessionDescription::parse(SdpType::Answer, &answer_sdp).unwrap();
                            client_pc.set_remote_description(answer).await.unwrap();
                        }
                        SignalingMessage::Candidate { candidate, .. } => {
                            if let Ok(c) = IceCandidate::from_sdp(&candidate) {
                                client_pc.add_ice_candidate(c).ok();
                            }
                        }
                        _ => {}
                    }
                }
                _ = client_state_rx.changed() => {
                    if let rustrtc::PeerConnectionState::Connected = *client_state_rx.borrow() {
                        break;
                    }
                    if let rustrtc::PeerConnectionState::Failed = *client_state_rx.borrow() {
                        panic!("WebRTC connection failed");
                    }
                }
            }
        }
    })
    .await
    .expect("WebRTC should connect within 30s");
    tracing::info!("WebRTC connected (UDP test)");

    // === Step 9: datagram echo round-trips ===
    let (recv_tx, mut recv_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let client_dc_clone = client_dc.clone();
    tokio::spawn(async move {
        while let Some(event) = client_dc_clone.recv().await {
            if let DataChannelEvent::Message(data) = event {
                let _ = recv_tx.send(data.to_vec());
            } else if matches!(event, DataChannelEvent::Close) {
                break;
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Small datagram first...
    let small = b"ping".to_vec();
    client_pc.send_data(client_dc.id, &small).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(10), recv_rx.recv())
        .await.expect("echo within timeout").expect("channel open");
    assert_eq!(got, small, "small datagram must round-trip intact");

    // ...then a >1200-byte datagram: SCTP fragments it on the wire but must
    // reassemble it as ONE data-channel message (datagram boundary kept).
    let large: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
    client_pc.send_data(client_dc.id, &large).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(10), recv_rx.recv())
        .await.expect("echo within timeout").expect("channel open");
    assert_eq!(got.len(), large.len(), "fragmented datagram must arrive whole");
    assert_eq!(got, large, "fragmented datagram payload must match");

    // ...and a final small datagram proves boundaries are not smeared
    // across neighbouring datagrams.
    let last = b"pong".to_vec();
    client_pc.send_data(client_dc.id, &last).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(10), recv_rx.recv())
        .await.expect("echo within timeout").expect("channel open");
    assert_eq!(got, last, "message boundaries must survive a large datagram");

    tracing::info!("UDP E2E test PASSED: datagrams echoed with boundaries intact");
}

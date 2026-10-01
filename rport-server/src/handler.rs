use crate::{AnswerMessage, CandidateMessage, OfferMessage, ServerMessage};
use anyhow::anyhow;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderName, HeaderValue, StatusCode},
    response::{sse::Event, IntoResponse, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::TryStreamExt;
use serde::Deserialize;
use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

pub const PING_INTERVAL: u64 = 30; // seconds

#[derive(Clone)]
pub struct AgentConnection {
    pub id: String,
    pub token: String,
    pub last_ping: SystemTime,
    pub sender: mpsc::UnboundedSender<ServerMessage>,
    pub connection_id: Uuid,
}

pub struct PendingOffer {
    pub offer: String,
    pub client_ip: String,
    pub sender: tokio::sync::oneshot::Sender<String>,
}

#[derive(Deserialize)]
pub struct ConnectQuery {
    token: String,
    id: String,
}

use crate::{clientaddr::ClientAddr, AppState};
pub async fn connect_sse(
    client_ip: ClientAddr,
    Query(params): Query<ConnectQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let ConnectQuery { token, id } = params;

    info!(id, token, %client_ip, "agent connected");

    let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
    let connection_id = Uuid::new_v4();
    let agent = AgentConnection {
        id: id.clone(),
        token: token.clone(),
        last_ping: SystemTime::now(),
        sender: sender.clone(),
        connection_id,
    };

    // Store the agent connection
    {
        let mut agents = state.agents.write().await;
        agents.insert(format!("{}:{}", token, id), agent);
    }

    let agent_key = format!("{}:{}", token, id);
    let state_clone = state.clone();

    // Spawn ping task
    let ping_tx = sender.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(PING_INTERVAL));
        loop {
            interval.tick().await;

            let ping_message = ServerMessage {
                message_type: "ping".to_string(),
                data: serde_json::json!({
                    "time": SystemTime::now()
                }),
            };
            if ping_tx.send(ping_message).is_err() {
                break;
            }
        }
    });

    let stream = async_stream::stream! {
        while let Some(message) = rx.recv().await {
            match serde_json::to_string(&message) {
                Ok(json) => {
                    yield Ok::<Event, axum::BoxError>(Event::default().data(json));
                }
                Err(e) => {
                    error!("Failed to serialize message: {}", e);
                    break;
                }
            }
        }
        // Clean up when stream ends
        info!(id, "SSE stream ended for agent");
        let mut agents = state_clone.agents.write().await;
        if let Some(agent) = agents.get(&agent_key) {
            if agent.connection_id == connection_id {
                agents.remove(&agent_key);
            }
        }
    };

    let sse_response = Sse::new(stream.map_err(|e| {
        error!("Failed to send SSE event: {}", e);
        anyhow!(e)
    }))
    .keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive-text"),
    );

    // Create response with X-Accel-Buffering header
    let mut response = sse_response.into_response();
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );

    response
}

pub async fn create_offer(
    client_ip: ClientAddr,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
    Json(offer_msg): Json<OfferMessage>,
) -> impl IntoResponse {
    let token = match params.get("token") {
        Some(t) => t,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "token required"})),
            )
        }
    };

    let agent_key = format!("{}:{}", token, offer_msg.id);
    let uuid = Uuid::new_v4();

    info!(
        %client_ip,
        agent_key,
        uuid = %uuid,
        "cli connecting to agent"
    );

    // Check if agent exists
    let agents = state.agents.read().await;
    let agent = match agents.get(&agent_key) {
        Some(agent) => agent,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "agent not found"})),
            )
        }
    };

    // Create oneshot channel for answer
    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();

    // Store pending offer
    {
        let mut pending_offers = state.pending_offers.write().await;
        pending_offers.insert(
            uuid,
            PendingOffer {
                offer: offer_msg.offer.clone(),
                client_ip: client_ip.to_string(),
                sender: answer_tx,
            },
        );
    }

    // Send offer to agent with client IP information
    let server_message = ServerMessage {
        message_type: "offer".to_string(),
        data: serde_json::json!({
            "uuid": uuid,
            "offer": offer_msg.offer,
            "client_ip": client_ip.to_string(),
        }),
    };

    if let Err(_) = agent.sender.send(server_message) {
        // Agent is gone, clean up
        let mut pending_offers = state.pending_offers.write().await;
        pending_offers.remove(&uuid);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "failed to send offer to agent"})),
        );
    }

    // Wait for answer with timeout
    let answer = match tokio::time::timeout(Duration::from_secs(30), answer_rx).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => {
            warn!("Answer channel closed for offer {}", uuid);
            let mut pending_offers = state.pending_offers.write().await;
            pending_offers.remove(&uuid);
            return (
                StatusCode::REQUEST_TIMEOUT,
                Json(serde_json::json!({"error": "answer timeout"})),
            );
        }
        Err(_) => {
            warn!("Answer timeout for offer {}", uuid);
            let mut pending_offers = state.pending_offers.write().await;
            pending_offers.remove(&uuid);
            return (
                StatusCode::REQUEST_TIMEOUT,
                Json(serde_json::json!({"error": "answer timeout"})),
            );
        }
    };

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "uuid": uuid,
            "offer": offer_msg.offer,
            "answer": answer
        })),
    )
}

pub async fn submit_answer(
    Path(uuid): Path<Uuid>,
    State(state): State<AppState>,
    Json(answer_msg): Json<AnswerMessage>,
) -> impl IntoResponse {
    let mut pending_offers = state.pending_offers.write().await;

    match pending_offers.remove(&uuid) {
        Some(pending_offer) => {
            if let Err(_) = pending_offer.sender.send(answer_msg.answer) {
                warn!("Failed to send answer for offer {}", uuid);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "failed to send answer"})),
                );
            }
            (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "offer not found"})),
        ),
    }
}
pub async fn submit_candidate(
    Path(uuid): Path<Uuid>,
    State(state): State<AppState>,
    Json(candidate_msg): Json<CandidateMessage>,
) -> impl IntoResponse {
    let pending = state.pending_candidates.read().await.get(&uuid).cloned();
    if let Some(tx) = pending {
        if tx.send(candidate_msg.candidate).is_ok() {
            return (StatusCode::OK, Json(serde_json::json!({"status": "ok"})));
        }
    }
    (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "offer not found"})))
}

pub async fn get_iceservers(State(state): State<AppState>) -> impl IntoResponse {
    // Generate temporary TURN credentials

    let mut ice_servers = vec![serde_json::json!({
        "urls": [state.turn_server.get_stun_url()]
    })];

    match state.turn_server.generate_credentials().await {
        Some(turn_creds) => {
            ice_servers.push(serde_json::json!({
                "urls": [state.turn_server.get_turn_url()],
                "username": turn_creds.username,
                "credential": turn_creds.password
            }));
        }
        None => {
            tracing::warn!("Failed to generate TURN credentials");
        }
    }

    (StatusCode::OK, Json(ice_servers))
}
pub fn create_router_with_state(state: AppState) -> Router {
    Router::new()
        .route("/rport/iceservers", get(get_iceservers))
        .route("/rport/connect", get(connect_sse))
        .route("/rport/offer", post(create_offer))
        .route("/rport/answer/{uuid}", post(submit_answer))
        .route("/rport/candidate/{uuid}", post(submit_candidate))
        .with_state(state)
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_server::TurnServer;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn test_state() -> AppState {
        let turn = TurnServer::new(
            false,
            "127.0.0.1:3478".parse().unwrap(),
            Some("127.0.0.1".to_string()),
        )
        .await
        .unwrap();
        AppState::new_with_turn(Arc::new(turn))
    }

    fn get_req(uri: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    fn json_post_req(uri: &str, body: serde_json::Value) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn test_iceservers_endpoint() {
        let app = create_router_with_state(test_state().await);
        let resp = app.oneshot(get_req("/rport/iceservers")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let servers = json.as_array().expect("array of ice servers");
        // STUN entry is always present; TURN entry included when credentials generated
        assert!(!servers.is_empty());
        assert!(servers[0]["urls"][0]
            .as_str()
            .unwrap()
            .starts_with("stun:127.0.0.1:"));
    }

    #[tokio::test]
    async fn test_offer_without_token_is_bad_request() {
        let app = create_router_with_state(test_state().await);
        let offer = OfferMessage {
            id: "agent1".to_string(),
            offer: "sdp".to_string(),
        };
        let resp = app
            .oneshot(json_post_req("/rport/offer", serde_json::to_value(offer).unwrap()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let json = body_json(resp).await;
        assert_eq!(json["error"], "token required");
    }

    #[tokio::test]
    async fn test_offer_to_unknown_agent_is_not_found() {
        let app = create_router_with_state(test_state().await);
        let offer = OfferMessage {
            id: "nobody".to_string(),
            offer: "sdp".to_string(),
        };
        let resp = app
            .oneshot(json_post_req(
                "/rport/offer?token=tok",
                serde_json::to_value(offer).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let json = body_json(resp).await;
        assert_eq!(json["error"], "agent not found");
    }

    #[tokio::test]
    async fn test_submit_answer_unknown_uuid_is_not_found() {
        let app = create_router_with_state(test_state().await);
        let answer = AnswerMessage {
            answer: "sdp".to_string(),
            seq: None,
            ack: None,
        };
        let resp = app
            .oneshot(json_post_req(
                &format!("/rport/answer/{}", Uuid::new_v4()),
                serde_json::to_value(answer).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_submit_candidate_unknown_uuid_is_not_found() {
        let app = create_router_with_state(test_state().await);
        let candidate = CandidateMessage {
            candidate: "candidate:1".to_string(),
            seq: None,
            ack: None,
        };
        let resp = app
            .oneshot(json_post_req(
                &format!("/rport/candidate/{}", Uuid::new_v4()),
                serde_json::to_value(candidate).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_submit_candidate_forwards_to_channel() {
        let state = test_state().await;
        let app = create_router_with_state(state.clone());
        let uuid = Uuid::new_v4();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        state
            .pending_candidates
            .write()
            .await
            .insert(uuid, tx);

        let candidate = CandidateMessage {
            candidate: "candidate:1 1 UDP 2130706431 10.0.0.1 8998 typ host".to_string(),
            seq: None,
            ack: None,
        };
        let resp = app
            .oneshot(json_post_req(
                &format!("/rport/candidate/{}", uuid),
                serde_json::to_value(candidate).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(rx.recv().await.unwrap(), "candidate:1 1 UDP 2130706431 10.0.0.1 8998 typ host");
    }

    /// Full signaling flow: agent connects over SSE, client posts an offer,
    /// the agent receives it through the SSE stream and posts an answer back,
    /// and the client's offer request resolves with that answer.
    #[tokio::test]
    async fn test_sse_offer_answer_flow() {
        let state = test_state().await;
        let app = create_router_with_state(state.clone());

        // 1. Agent connects over SSE; keep reading the body in the background.
        let sse_resp = app
            .clone()
            .oneshot(get_req("/rport/connect?token=tok&id=agent1"))
            .await
            .unwrap();
        assert_eq!(sse_resp.status(), StatusCode::OK);
        let headers = sse_resp.headers().clone();
        assert!(headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"));
        assert_eq!(headers.get("x-accel-buffering").unwrap(), "no");
        let mut sse_body = sse_resp.into_body();

        // 2. Wait until the agent is registered in the shared state.
        let mut registered = false;
        for _ in 0..100 {
            if state.agents.read().await.contains_key("tok:agent1") {
                registered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(registered, "agent should be registered in state");

        // 3. Client posts an offer (runs until the answer arrives).
        let app_for_offer = app.clone();
        let offer_task = tokio::spawn(async move {
            let offer = OfferMessage {
                id: "agent1".to_string(),
                offer: "fake-offer-sdp".to_string(),
            };
            let resp = app_for_offer
                .oneshot(json_post_req(
                    "/rport/offer?token=tok",
                    serde_json::to_value(offer).unwrap(),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let json = body_json(resp).await;
            assert_eq!(json["answer"], "fake-answer-sdp");
            assert_eq!(json["offer"], "fake-offer-sdp");
        });

        // 4. Agent receives the offer via SSE and submits an answer.
        let mut line_buf = String::new();
        let mut uuid: Option<Uuid> = None;
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), sse_body.frame())
                .await
                .expect("SSE frame timeout")
                .expect("SSE body ended unexpectedly")
                .expect("SSE frame error");
            if let Some(data) = frame.data_ref() {
                line_buf.push_str(&String::from_utf8_lossy(data));
                while let Some(pos) = line_buf.find('\n') {
                    let line: String = line_buf.drain(..=pos).collect();
                    let line = line.trim_end();
                    if let Some(payload) = line.strip_prefix("data: ") {
                        let msg: ServerMessage = serde_json::from_str(payload).unwrap();
                        assert_eq!(msg.message_type, "offer");
                        let offer_uuid: Uuid = msg.data["uuid"]
                            .as_str()
                            .unwrap()
                            .parse()
                            .unwrap();
                        assert_eq!(msg.data["client_ip"], "0.0.0.0:0");
                        uuid = Some(offer_uuid);
                        break;
                    }
                }
            }
            if uuid.is_some() {
                break;
            }
        }

        let answer = AnswerMessage {
            answer: "fake-answer-sdp".to_string(),
            seq: None,
            ack: None,
        };
        let resp = app
            .oneshot(json_post_req(
                &format!("/rport/answer/{}", uuid.unwrap()),
                serde_json::to_value(answer).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 5. The client's offer request resolves with the answer.
        tokio::time::timeout(Duration::from_secs(5), offer_task)
            .await
            .expect("offer request timed out")
            .expect("offer task panicked");
    }

    /// A second SSE connection with the same token/id replaces the first
    /// registration (latest connection wins).
    #[tokio::test]
    async fn test_reconnect_replaces_agent_registration() {
        let state = test_state().await;
        let app = create_router_with_state(state.clone());

        let resp1 = app
            .clone()
            .oneshot(get_req("/rport/connect?token=tok&id=agent1"))
            .await
            .unwrap();
        assert_eq!(resp1.status(), StatusCode::OK);

        let mut registered = false;
        for _ in 0..100 {
            if state.agents.read().await.contains_key("tok:agent1") {
                registered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(registered);
        let first_connection_id = state.agents.read().await["tok:agent1"].connection_id;

        let resp2 = app
            .oneshot(get_req("/rport/connect?token=tok&id=agent1"))
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);

        let mut replaced = false;
        for _ in 0..100 {
            let agents = state.agents.read().await;
            if let Some(agent) = agents.get("tok:agent1") {
                if agent.connection_id != first_connection_id {
                    replaced = true;
                    break;
                }
            }
            drop(agents);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(replaced, "reconnect should replace the agent registration");
    }
}

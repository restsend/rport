use crate::{AnswerMessage, OfferMessage, ServerMessage};
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
pub fn create_router(turn_server: std::sync::Arc<crate::TurnServer>) -> Router {
    let state = AppState::new_with_turn(turn_server);

    Router::new()
        .route("/rport/iceservers", get(get_iceservers))
        .route("/rport/connect", get(connect_sse))
        .route("/rport/offer", post(create_offer))
        .route("/rport/answer/{uuid}", post(submit_answer))
        .with_state(state)
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

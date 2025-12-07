use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use uuid::Uuid;
pub mod clientaddr;
pub mod handler;
pub mod state;
pub mod turn_server;
pub use state::*;
pub use turn_server::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfferMessage {
    pub id: String,
    pub offer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerMessage {
    pub answer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMessage {
    pub message_type: String,
    pub data: serde_json::Value,
}

use crate::handler::{AgentConnection, PendingOffer};

#[derive(Clone)]
pub struct AppState {
    pub agents: Arc<RwLock<HashMap<String, AgentConnection>>>,
    pub pending_offers: Arc<RwLock<HashMap<Uuid, PendingOffer>>>,
    pub turn_server: Arc<TurnServer>,
}

impl AppState {
    pub fn new_with_turn(turn_server: Arc<TurnServer>) -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            pending_offers: Arc::new(RwLock::new(HashMap::new())),
            turn_server,
        }
    }
}

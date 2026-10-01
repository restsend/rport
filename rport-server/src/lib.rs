use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{mpsc, RwLock};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateMessage {
    pub candidate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<u64>,
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
    pub pending_candidates: Arc<RwLock<HashMap<Uuid, mpsc::UnboundedSender<String>>>>,
    pub turn_server: Arc<TurnServer>,
}

impl AppState {
    pub fn new_with_turn(turn_server: Arc<TurnServer>) -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            pending_offers: Arc::new(RwLock::new(HashMap::new())),
            pending_candidates: Arc::new(RwLock::new(HashMap::new())),
            turn_server,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_server::TurnServer;

    async fn test_state() -> AppState {
        let turn = TurnServer::new(false, "127.0.0.1:3478".parse().unwrap(), None)
            .await
            .unwrap();
        AppState::new_with_turn(Arc::new(turn))
    }

    #[tokio::test]
    async fn test_app_state_starts_empty() {
        let state = test_state().await;
        assert!(state.agents.read().await.is_empty());
        assert!(state.pending_offers.read().await.is_empty());
        assert!(state.pending_candidates.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_app_state_is_clonable_and_shared() {
        let state = test_state().await;
        let cloned = state.clone();
        assert!(cloned.agents.write().await.insert("k".to_string(), {
            let (tx, _rx) = mpsc::unbounded_channel();
            handler::AgentConnection {
                id: "i".to_string(),
                token: "t".to_string(),
                last_ping: std::time::SystemTime::now(),
                sender: tx,
                connection_id: Uuid::new_v4(),
            }
        }).is_none());
        assert_eq!(state.agents.read().await.len(), 1, "clone must share the same map");
    }

    #[test]
    fn test_offer_message_serde_roundtrip() {
        let msg = OfferMessage {
            id: "agent1".to_string(),
            offer: "v=0".to_string(),
        };
        let round: OfferMessage =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(round.id, "agent1");
        assert_eq!(round.offer, "v=0");
    }

    #[test]
    fn test_answer_message_none_fields_skipped() {
        let msg = AnswerMessage {
            answer: "a".to_string(),
            seq: None,
            ack: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert!(json.get("seq").is_none());
        assert!(json.get("ack").is_none());
        // Missing fields deserialize as None
        let round: AnswerMessage = serde_json::from_str(r#"{"answer":"a"}"#).unwrap();
        assert_eq!(round.seq, None);
        assert_eq!(round.ack, None);
    }

    #[test]
    fn test_candidate_message_partial_seq() {
        let msg = CandidateMessage {
            candidate: "c".to_string(),
            seq: Some(1),
            ack: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["seq"], 1);
        assert!(json.get("ack").is_none());
    }
}

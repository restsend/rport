use std::time::{Duration, Instant};
use tracing::debug;

use crate::dtls_signaling::SignalingMessage;

#[derive(Debug, Clone)]
pub struct PendingMessage {
    pub seq: u64,
    pub msg: SignalingMessage,
    pub retransmit_count: u32,
    pub deadline: Instant,
}

pub struct ReliableSession {
    next_seq: u64,
    last_recv_seq: Option<u64>,
    pending: Vec<PendingMessage>,
    peer_reliable: bool,
    rto: Duration,
    max_retransmits: u32,
    exhausted: bool,
}

impl ReliableSession {
    pub fn new(rto: Duration, max_retransmits: u32) -> Self {
        Self {
            next_seq: 1,
            last_recv_seq: None,
            pending: vec![],
            peer_reliable: false,
            rto,
            max_retransmits,
            exhausted: false,
        }
    }

    /// Attach seq and ack to an outgoing message.
    /// If `track` is true, the message is added to the retransmit queue.
    pub fn prepare_send(&mut self, msg: &mut SignalingMessage, track: bool) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let ack = self.last_recv_seq;
        self.attach_seq_ack(msg, Some(seq), ack);
        if track {
            self.pending.push(PendingMessage {
                seq,
                msg: msg.clone(),
                retransmit_count: 0,
                deadline: Instant::now() + self.rto,
            });
        }
    }

    /// Process an incoming message. Returns a seq to ACK if the peer sent one.
    pub fn process_recv(&mut self, msg: &SignalingMessage) {
        let (peer_seq, peer_ack) = self.extract_seq_ack(msg);
        if let Some(s) = peer_seq {
            self.peer_reliable = true;
            self.last_recv_seq = Some(s);
        }
        if let Some(a) = peer_ack {
            self.pending.retain(|p| {
                if p.seq <= a {
                    debug!("Peer ACKed seq={}, removing from pending", p.seq);
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Return pending messages whose retransmit deadline has passed.
    pub fn due_retransmits(&mut self, now: Instant) -> Vec<PendingMessage> {
        if self.exhausted {
            return vec![];
        }
        let mut due = vec![];
        self.pending.retain(|p| {
            if p.deadline <= now {
                if p.retransmit_count < self.max_retransmits {
                    let mut r = p.clone();
                    r.retransmit_count += 1;
                    r.deadline = now + self.rto;
                    debug!(
                        "Retransmitting seq={} (attempt {}/{})",
                        r.seq,
                        r.retransmit_count,
                        self.max_retransmits
                    );
                    due.push(r);
                    false
                } else {
                    self.exhausted = true;
                    true
                }
            } else {
                true
            }
        });
        due
    }

    pub fn is_peer_reliable(&self) -> bool {
        self.peer_reliable
    }

    pub fn last_recv_seq(&self) -> Option<u64> {
        self.last_recv_seq
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn mark_exhausted(&mut self) {
        self.exhausted = true;
    }

    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    fn attach_seq_ack(&self, msg: &mut SignalingMessage, seq: Option<u64>, ack: Option<u64>) {
        match msg {
            SignalingMessage::Offer { seq: ref mut s, ack: ref mut a, .. } => { *s = seq; *a = ack; }
            SignalingMessage::Answer { seq: ref mut s, ack: ref mut a, .. } => { *s = seq; *a = ack; }
            SignalingMessage::Candidate { seq: ref mut s, ack: ref mut a, .. } => { *s = seq; *a = ack; }
            SignalingMessage::EndOfCandidates { seq: ref mut s, ack: ref mut a, .. } => { *s = seq; *a = ack; }
            SignalingMessage::Error { seq: ref mut s, ack: ref mut a, .. } => { *s = seq; *a = ack; }
            SignalingMessage::Ack { seq: ref mut s, ack: ref mut a, .. } => { *s = seq; *a = ack; }
            _ => {}
        }
    }

    fn extract_seq_ack(&self, msg: &SignalingMessage) -> (Option<u64>, Option<u64>) {
        match msg {
            SignalingMessage::Offer { seq, ack, .. } => (*seq, *ack),
            SignalingMessage::Answer { seq, ack, .. } => (*seq, *ack),
            SignalingMessage::Candidate { seq, ack, .. } => (*seq, *ack),
            SignalingMessage::EndOfCandidates { seq, ack, .. } => (*seq, *ack),
            SignalingMessage::Error { seq, ack, .. } => (*seq, *ack),
            SignalingMessage::Ack { ack_seq, .. } => (None, Some(*ack_seq)),
            _ => (None, None),
        }
    }
}

//! Simple pub/sub event broker for real-time peer notifications.
//!
//! # Overview
//!
//! The event broker allows agents to subscribe to real-time notifications
//! from the daemon. Useful for coordinating multi-agent work:
//!
//! - **lock_acquired**: Session A acquired write lock on /path
//! - **lock_released**: Session A released lock, path is now free
//! - **peer_commit**: Session B committed changes to branch-name
//! - **conflict**: Two sessions tried to write the same path
//!
//! # Implementation
//!
//! Uses `tokio::sync::broadcast` (multi-producer, multi-consumer):
//!
//! - **Publish**: O(1) per subscriber (send is async, never blocks)
//! - **Subscribe**: O(1), returns a `BroadcastReceiver`
//! - **Backpressure**: Events are buffered (default 100 in-flight)
//! - **Drop on overflow**: Slow subscribers may miss events (acceptable for telemetry)
//!
//! # Subscribers
//!
//! Subscribers call `subscribe()` to get a `tokio::sync::broadcast` receiver,
//! then `.recv()` to wait for events.
//!
//! # Publishers
//!
//! Publishers call `publish(session_id, event_type, data)` to broadcast.
//! This is non-blocking; slow subscribers do not block the daemon.
//!
//! # Example
//!
//! ```ignore
//! let broker = EventBroker::new();
//!
//! // Agent subscribes to peer commits
//! let mut rx = broker.subscribe("peer_commit");
//! tokio::spawn(async move {
//!     while let Ok(event) = rx.recv().await {
//!         eprintln!("Peer committed: {:?}", event);
//!     }
//! });
//!
//! // Daemon publishes events
//! broker.publish(session_id, "peer_commit", serde_json::json!({
//!     "session_id": session_id,
//!     "branch": "feature-x",
//!     "commit": "abc123...",
//! }));
//! ```
//!
//! # Event Format
//!
//! All events are JSON objects with at minimum:
//! ```json
//! {
//!   "event_type": "peer_commit",
//!   "session_id": "uuid",
//!   "timestamp": "2026-04-12T12:34:56Z",
//!   "<event-specific-data>": "..."
//! }
//! ```

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use uuid::Uuid;

const CHANNEL_CAPACITY: usize = 64;
const HISTORY_CAPACITY: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub source_session: Uuid,
    pub kind:           String,
    pub payload:        Value,
    pub emitted_at:     DateTime<Utc>,
}

pub struct EventBroker {
    /// Per-session broadcast channels. Created lazily on first subscribe.
    channels: Mutex<HashMap<Uuid, broadcast::Sender<Event>>>,
    /// Global broadcast channel — all events go here too.
    global: broadcast::Sender<Event>,
    /// Bounded in-memory event history for polling clients.
    history: Mutex<VecDeque<Event>>,
}

impl EventBroker {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            channels: Mutex::new(HashMap::new()),
            global:   tx,
            history:  Mutex::new(VecDeque::with_capacity(HISTORY_CAPACITY)),
        }
    }

    /// Publish an event from `source_session` to all subscribers.
    pub fn publish(&self, source_session: Uuid, kind: &str, payload: Value) {
        let event = Event {
            source_session,
            kind:    kind.to_owned(),
            payload,
            emitted_at: Utc::now(),
        };
        {
            let mut history = self.history.lock().unwrap();
            history.push_back(event.clone());
            while history.len() > HISTORY_CAPACITY {
                history.pop_front();
            }
        }
        let _ = self.global.send(event.clone());
        let channels = self.channels.lock().unwrap();
        if let Some(tx) = channels.get(&source_session) {
            let _ = tx.send(event);
        }
    }

    /// Subscribe to all events from a specific session.
    pub fn subscribe_session(&self, session_id: Uuid) -> broadcast::Receiver<Event> {
        let mut channels = self.channels.lock().unwrap();
        channels
            .entry(session_id)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Subscribe to all events across all sessions.
    pub fn subscribe_global(&self) -> broadcast::Receiver<Event> {
        self.global.subscribe()
    }

    /// Return recent events, optionally filtered by source session.
    pub fn recent(&self, session_id: Option<Uuid>, limit: usize) -> Vec<Event> {
        let history = self.history.lock().unwrap();
        let mut events: Vec<Event> = history
            .iter()
            .rev()
            .filter(|e| match session_id {
                Some(sid) => e.source_session == sid,
                None => true,
            })
            .take(limit)
            .cloned()
            .collect();
        events.reverse();
        events
    }
}

#[cfg(test)]
mod tests {
    use super::EventBroker;
    use uuid::Uuid;

    #[test]
    fn recent_returns_ordered_events_and_filters_by_session() {
        let broker = EventBroker::new();
        let s1 = Uuid::now_v7();
        let s2 = Uuid::now_v7();

        broker.publish(s1, "lock_acquired", serde_json::json!({"path": "a.txt"}));
        broker.publish(s2, "peer_commit", serde_json::json!({"branch": "feat/x"}));
        broker.publish(s1, "lock_released", serde_json::json!({"path": "a.txt"}));

        let all = broker.recent(None, 10);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].kind, "lock_acquired");
        assert_eq!(all[2].kind, "lock_released");

        let s1_only = broker.recent(Some(s1), 10);
        assert_eq!(s1_only.len(), 2);
        assert!(s1_only.iter().all(|e| e.source_session == s1));
    }
}

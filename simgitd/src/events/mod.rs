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

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::broadcast;
use uuid::Uuid;

const CHANNEL_CAPACITY: usize = 64;

#[derive(Debug, Clone)]
pub struct Event {
    pub source_session: Uuid,
    pub kind:           String,
    pub payload:        Value,
}

pub struct EventBroker {
    /// Per-session broadcast channels. Created lazily on first subscribe.
    channels: Mutex<HashMap<Uuid, broadcast::Sender<Event>>>,
    /// Global broadcast channel — all events go here too.
    global: broadcast::Sender<Event>,
}

impl EventBroker {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            channels: Mutex::new(HashMap::new()),
            global:   tx,
        }
    }

    /// Publish an event from `source_session` to all subscribers.
    pub fn publish(&self, source_session: Uuid, kind: &str, payload: Value) {
        let event = Event {
            source_session,
            kind:    kind.to_owned(),
            payload,
        };
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
}

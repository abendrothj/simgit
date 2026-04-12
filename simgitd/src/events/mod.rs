//! Simple pub/sub event broker for real-time peer notifications.
//!
//! Subscribers call `subscribe()` to get a `tokio::sync::broadcast` receiver.
//! Publishers call `publish()` to fan out an event to all current subscribers.

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

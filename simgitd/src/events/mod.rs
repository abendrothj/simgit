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
    use std::time::Duration;
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

    #[tokio::test]
    async fn global_subscriber_receives_all_published_events() {
        let broker = EventBroker::new();
        let s1 = Uuid::now_v7();
        let mut rx = broker.subscribe_global();

        broker.publish(s1, "peer_commit", serde_json::json!({"branch": "feat/x"}));
        broker.publish(s1, "lock_acquired", serde_json::json!({"path": "b.txt"}));

        let e1 = rx.recv().await.expect("first event");
        assert_eq!(e1.kind, "peer_commit");
        assert_eq!(e1.source_session, s1);

        let e2 = rx.recv().await.expect("second event");
        assert_eq!(e2.kind, "lock_acquired");
    }

    #[tokio::test]
    async fn session_subscriber_receives_events_from_that_session_only() {
        let broker = EventBroker::new();
        let s1 = Uuid::now_v7();
        let s2 = Uuid::now_v7();
        let mut rx = broker.subscribe_session(s1);

        broker.publish(s1, "session1_event", serde_json::json!({}));
        broker.publish(s2, "session2_event", serde_json::json!({}));
        broker.publish(s1, "session1_again", serde_json::json!({}));

        let e1 = rx.recv().await.expect("first event");
        assert_eq!(e1.kind, "session1_event");
        assert_eq!(e1.source_session, s1);

        let e2 = rx.recv().await.expect("second event");
        assert_eq!(e2.kind, "session1_again");
        assert_eq!(e2.source_session, s1);
    }

    #[tokio::test]
    async fn global_subscriber_receives_same_event_as_session_subscriber() {
        let broker = EventBroker::new();
        let s1 = Uuid::now_v7();
        let mut global_rx = broker.subscribe_global();
        let mut session_rx = broker.subscribe_session(s1);

        broker.publish(s1, "confirm_delivery", serde_json::json!({"seq": 1}));

        let global_event = global_rx.recv().await.expect("global should receive");
        let session_event = session_rx.recv().await.expect("session should receive");
        assert_eq!(global_event.kind, "confirm_delivery");
        assert_eq!(session_event.kind, "confirm_delivery");
        assert_eq!(global_event.emitted_at, session_event.emitted_at,
            "global and session subscribers should see the same event timestamp");
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive_each_event() {
        let broker = EventBroker::new();
        let s1 = Uuid::now_v7();
        let mut rx1 = broker.subscribe_global();
        let mut rx2 = broker.subscribe_global();
        let mut rx3 = broker.subscribe_global();

        broker.publish(s1, "broadcast_test", serde_json::json!({"n": 42}));

        for (i, rx) in [&mut rx1, &mut rx2, &mut rx3].iter_mut().enumerate() {
            let event = rx.recv().await.map_err(|_| format!("subscriber {i} failed"));
            let event = event.expect("all subscribers should receive the event");
            assert_eq!(event.kind, "broadcast_test");
        }
    }

    #[tokio::test]
    async fn published_events_are_recorded_in_history_regardless_of_subscribers() {
        let broker = EventBroker::new();
        let s1 = Uuid::now_v7();

        broker.publish(s1, "evt1", serde_json::json!({"id": 1}));
        broker.publish(s1, "evt2", serde_json::json!({"id": 2}));
        broker.publish(s1, "evt3", serde_json::json!({"id": 3}));

        let recent = broker.recent(None, 10);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].kind, "evt1");
        assert_eq!(recent[2].kind, "evt3");
    }

    #[test]
    fn history_capacity_drops_oldest_events() {
        let broker = EventBroker::new();
        let s1 = Uuid::now_v7();

        // Publish more events than the history capacity (256).
        for i in 0..300 {
            broker.publish(s1, "evt", serde_json::json!({"idx": i}));
        }

        let recent = broker.recent(None, 500);
        assert_eq!(recent.len(), 256, "history should be capped at capacity");

        // The first event (idx 0) should have been dropped; the first retained
        // should be idx 44 = 300 - 256.
        let first_idx = recent[0].payload["idx"].as_u64().expect("idx field");
        assert_eq!(first_idx, 44, "oldest retained event should be at index 300-256");
    }

    #[test]
    fn recent_limit_respects_cap() {
        let broker = EventBroker::new();
        let s1 = Uuid::now_v7();

        for i in 0..50 {
            broker.publish(s1, "evt", serde_json::json!({"idx": i}));
        }

        let subset = broker.recent(None, 10);
        assert_eq!(subset.len(), 10);
        assert_eq!(subset[0].payload["idx"], serde_json::json!(40));
        assert_eq!(subset[9].payload["idx"], serde_json::json!(49));
    }

    #[tokio::test]
    async fn publish_without_active_subscribers_does_not_panic() {
        let broker = EventBroker::new();
        let s1 = Uuid::now_v7();

        // No subscribers for this session yet — should be a no-op.
        broker.publish(s1, "lonely_event", serde_json::json!({}));

        // Subscribe after publish — should receive nothing (no replay).
        let mut rx = broker.subscribe_global();
        tokio::time::sleep(Duration::from_millis(5)).await;

        // Publish a new event and verify it arrives.
        broker.publish(s1, "new_event", serde_json::json!({}));
        let event = rx.recv().await.expect("subsequent event should be received");
        assert_eq!(event.kind, "new_event");
    }

    #[tokio::test]
    async fn dropped_subscriber_does_not_block_publish() {
        let broker = EventBroker::new();
        let s1 = Uuid::now_v7();

        // Subscribe and immediately drop the receiver.
        let rx = broker.subscribe_global();
        drop(rx);

        // Publish must still succeed (broadcast handles lagged/closed receivers).
        broker.publish(s1, "after_drop", serde_json::json!({}));

        // A second subscriber should still receive events normally.
        let mut rx2 = broker.subscribe_global();
        broker.publish(s1, "post_drop", serde_json::json!({}));
        let event = rx2.recv().await.expect("second subscriber should receive");
        assert_eq!(event.kind, "post_drop");
    }
}

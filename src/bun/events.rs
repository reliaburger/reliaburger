//! Bounded, in-memory cluster events for the Relish TUI.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Maximum number of historical events retained in memory.
pub const EVENT_BUFFER_CAP: usize = 1024;

/// A cluster event suitable for display and streaming.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClusterEvent {
    /// Monotonically increasing process-local sequence number.
    pub sequence: u64,
    /// Unix timestamp in seconds.
    pub timestamp: u64,
    /// Kind of lifecycle event.
    pub kind: EventKind,
    /// Event severity.
    pub severity: EventSeverity,
    /// Related app, when applicable.
    pub app: Option<String>,
    /// Related namespace, when applicable.
    pub namespace: Option<String>,
    /// Related node, when applicable.
    pub node: Option<String>,
    /// Human-readable description.
    pub message: String,
}

/// Categories emitted by the Bun agent in Phase 13.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    /// A deployment completed or failed.
    Deploy,
    /// An instance restarted.
    Restart,
    /// Instance health changed.
    Health,
    /// An app stopped.
    Stop,
    /// A job completed successfully.
    JobCompleted,
    /// A job exhausted its retries.
    JobFailed,
    /// An alert changed state.
    Alert,
    /// A fault was injected or cleared.
    Fault,
}

/// Importance of an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSeverity {
    /// Normal operation.
    Info,
    /// Degraded operation that deserves attention.
    Warning,
    /// Operation failed or service is unavailable.
    Critical,
}

/// A bounded event history with a live broadcast feed.
pub struct EventStore {
    buffer: VecDeque<ClusterEvent>,
    next_sequence: u64,
    live: broadcast::Sender<ClusterEvent>,
}

impl Default for EventStore {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStore {
    /// Create an empty store.
    pub fn new() -> Self {
        let (live, _) = broadcast::channel(256);
        Self {
            buffer: VecDeque::with_capacity(EVENT_BUFFER_CAP),
            next_sequence: 1,
            live,
        }
    }

    /// Record an event, returning its assigned sequence number.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        timestamp: u64,
        kind: EventKind,
        severity: EventSeverity,
        app: Option<String>,
        namespace: Option<String>,
        node: Option<String>,
        message: String,
    ) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let event = ClusterEvent {
            sequence,
            timestamp,
            kind,
            severity,
            app,
            namespace,
            node,
            message,
        };
        if self.buffer.len() == EVENT_BUFFER_CAP {
            self.buffer.pop_front();
        }
        self.buffer.push_back(event.clone());
        let _ = self.live.send(event);
        sequence
    }

    /// Return recent matching events in chronological order.
    pub fn recent(
        &self,
        limit: usize,
        app: Option<&str>,
        min_severity: Option<EventSeverity>,
    ) -> Vec<ClusterEvent> {
        let mut events: Vec<_> = self
            .buffer
            .iter()
            .rev()
            .filter(|event| app.is_none_or(|name| event.app.as_deref() == Some(name)))
            .filter(|event| min_severity.is_none_or(|level| event.severity >= level))
            .take(limit.min(EVENT_BUFFER_CAP))
            .cloned()
            .collect();
        events.reverse();
        events
    }

    /// Subscribe to events recorded after this call.
    pub fn subscribe(&self) -> broadcast::Receiver<ClusterEvent> {
        self.live.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(store: &mut EventStore, app: &str, severity: EventSeverity) -> u64 {
        store.record(
            1,
            EventKind::Deploy,
            severity,
            Some(app.to_string()),
            Some("default".to_string()),
            None,
            format!("deployed {app}"),
        )
    }

    #[test]
    fn sequence_increases_and_ring_discards_oldest() {
        let mut store = EventStore::new();
        for index in 0..EVENT_BUFFER_CAP + 2 {
            record(&mut store, &format!("app-{index}"), EventSeverity::Info);
        }
        let events = store.recent(EVENT_BUFFER_CAP, None, None);
        assert_eq!(events.len(), EVENT_BUFFER_CAP);
        assert_eq!(events.first().map(|event| event.sequence), Some(3));
        assert_eq!(events.last().map(|event| event.sequence), Some(1026));
    }

    #[test]
    fn recent_filters_by_app_and_minimum_severity() {
        let mut store = EventStore::new();
        record(&mut store, "web", EventSeverity::Info);
        record(&mut store, "web", EventSeverity::Critical);
        record(&mut store, "worker", EventSeverity::Critical);
        let events = store.recent(10, Some("web"), Some(EventSeverity::Warning));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].app.as_deref(), Some("web"));
    }

    #[tokio::test]
    async fn subscribers_receive_new_events() {
        let mut store = EventStore::new();
        let mut receiver = store.subscribe();
        record(&mut store, "web", EventSeverity::Info);
        assert_eq!(receiver.recv().await.unwrap().sequence, 1);
    }
}

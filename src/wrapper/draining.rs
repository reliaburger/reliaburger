/// Connection draining for zero-downtime deploys.
///
/// When a backend needs to be replaced (rolling deploy, health
/// failure), Bun tells Wrapper to drain it. The backend moves
/// from active to draining: no new requests are routed to it,
/// but in-flight requests are allowed to complete. Once all
/// connections are done (or the timeout expires), Wrapper tells
/// Bun the drain is complete and the container can be stopped.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, mpsc};

/// A drain tracker shared between the live Wrapper proxy and Bun's deploy
/// path.
///
/// The proxy holds a clone and bumps a draining backend's connection count
/// as requests come and go; Bun holds another clone, starts a drain when it
/// retires an old instance, and waits until that instance's in-flight count
/// reaches zero (or the timeout expires) before killing the container. Both
/// sides reach through the same `Arc<Mutex<DrainTracker>>`, so the tracker
/// that was library-only in earlier phases now governs the real path.
///
/// `Arc<Mutex<…>>` is Rust's shared-mutable-state idiom for async code: the
/// `Arc` (atomically reference-counted pointer) lets several tasks own a
/// handle to the same tracker, and the tokio `Mutex` serialises access so
/// only one task mutates it at a time. We use the tokio `Mutex`, not the
/// std one, because it yields rather than blocking the runtime thread.
#[derive(Clone)]
pub struct SharedDrains(Arc<Mutex<DrainTracker>>);

impl SharedDrains {
    /// Wrap a tracker so several tasks can share it.
    pub fn new(tracker: DrainTracker) -> Self {
        Self(Arc::new(Mutex::new(tracker)))
    }

    /// Begin draining an instance. New traffic should already be routed away
    /// (the caller rebuilds the routing table first); this only starts the
    /// in-flight countdown.
    pub async fn start_drain(&self, cmd: &DrainCommand) -> bool {
        self.0.lock().await.start_drain(cmd)
    }

    /// True while `instance_id` is still draining.
    pub async fn is_draining(&self, instance_id: &str) -> bool {
        self.0.lock().await.is_draining(instance_id)
    }

    /// Record a request arriving at a draining backend.
    pub async fn increment_connections(&self, instance_id: &str) {
        self.0.lock().await.increment_connections(instance_id);
    }

    /// Record a request to a draining backend finishing.
    pub async fn decrement_connections(&self, instance_id: &str) {
        self.0.lock().await.decrement_connections(instance_id);
    }

    /// Record a WebSocket splice opening to a draining backend (ING4).
    pub async fn increment_websocket(&self, instance_id: &str) {
        self.0.lock().await.increment_websocket(instance_id);
    }

    /// Record a WebSocket splice to a draining backend closing (ING4).
    pub async fn decrement_websocket(&self, instance_id: &str) {
        self.0.lock().await.decrement_websocket(instance_id);
    }

    /// Sweep for completed drains, returning the instance IDs that are done.
    pub async fn check_completions(&self) -> Vec<String> {
        self.0.lock().await.check_completions().await
    }

    /// Wait until `instance_id` has drained (zero in-flight) or its deadline
    /// passes, whichever comes first. Returns once the instance is no longer
    /// tracked. Polls rather than blocks, so it never wedges the runtime.
    pub async fn wait_drained(&self, instance_id: &str) {
        loop {
            let done = {
                let mut tracker = self.0.lock().await;
                if !tracker.is_draining(instance_id) {
                    break;
                }
                tracker.check_completions().await;
                !tracker.is_draining(instance_id)
            };
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// A command from Bun to Wrapper to drain a backend.
#[derive(Debug, Clone)]
pub struct DrainCommand {
    /// App name.
    pub app_name: String,
    /// Instance being drained.
    pub instance_id: String,
    /// How long to wait for in-flight requests before force-closing.
    pub timeout: Duration,
}

/// Notification from Wrapper to Bun that draining is complete.
#[derive(Debug, Clone)]
pub struct DrainComplete {
    /// App name.
    pub app_name: String,
    /// Instance that finished draining.
    pub instance_id: String,
}

/// Tracks draining backends and their deadlines.
pub struct DrainTracker {
    /// Backends currently draining, keyed by instance ID.
    draining: HashMap<String, DrainEntry>,
    /// Channel to notify Bun when draining is complete.
    complete_tx: mpsc::Sender<DrainComplete>,
}

/// A single backend being drained.
struct DrainEntry {
    app_name: String,
    /// Hard deadline — force close after this.
    deadline: Instant,
    /// Number of in-flight connections to this backend.
    active_connections: u32,
    /// Number of WebSocket connections (subset of active_connections).
    websocket_connections: u32,
}

impl DrainTracker {
    /// Create a new drain tracker.
    pub fn new(complete_tx: mpsc::Sender<DrainComplete>) -> Self {
        Self {
            draining: HashMap::new(),
            complete_tx,
        }
    }

    /// Start draining a backend. Returns true if the backend was
    /// added, false if it was already draining.
    pub fn start_drain(&mut self, cmd: &DrainCommand) -> bool {
        if self.draining.contains_key(&cmd.instance_id) {
            return false;
        }

        self.draining.insert(
            cmd.instance_id.clone(),
            DrainEntry {
                app_name: cmd.app_name.clone(),
                deadline: Instant::now() + cmd.timeout,
                active_connections: 0,
                websocket_connections: 0,
            },
        );
        true
    }

    /// Record that a new connection was routed to a draining backend.
    /// (This shouldn't happen — the routing table should exclude
    /// draining backends — but we track it for safety.)
    pub fn increment_connections(&mut self, instance_id: &str) {
        if let Some(entry) = self.draining.get_mut(instance_id) {
            entry.active_connections += 1;
        }
    }

    /// Record that a connection to a draining backend completed.
    pub fn decrement_connections(&mut self, instance_id: &str) {
        if let Some(entry) = self.draining.get_mut(instance_id) {
            entry.active_connections = entry.active_connections.saturating_sub(1);
        }
    }

    /// Record that a WebSocket connection was established to a draining backend.
    pub fn increment_websocket(&mut self, instance_id: &str) {
        if let Some(entry) = self.draining.get_mut(instance_id) {
            entry.websocket_connections += 1;
        }
    }

    /// Record that a WebSocket connection to a draining backend closed.
    pub fn decrement_websocket(&mut self, instance_id: &str) {
        if let Some(entry) = self.draining.get_mut(instance_id) {
            entry.websocket_connections = entry.websocket_connections.saturating_sub(1);
        }
    }

    /// Get the WebSocket Close frame bytes to send during draining.
    ///
    /// Returns a Close frame with status 1001 (Going Away) for each
    /// draining backend that has active WebSocket connections.
    pub fn websocket_close_frame() -> Vec<u8> {
        super::websocket::build_close_frame(1001)
    }

    /// Check whether a backend is currently draining.
    pub fn is_draining(&self, instance_id: &str) -> bool {
        self.draining.contains_key(instance_id)
    }

    /// Check all draining backends and complete any that are done
    /// (zero connections or past deadline). Returns the instance IDs
    /// that completed.
    pub async fn check_completions(&mut self) -> Vec<String> {
        let now = Instant::now();
        let mut completed = Vec::new();

        for (id, entry) in &self.draining {
            // A drain completes when both plain and WebSocket in-flight counts
            // reach zero, or the deadline passes. Waiting on the WebSocket
            // count too means a rolling deploy honours `drain_timeout` for a
            // live WebSocket splice, not just HTTP requests (ING4).
            let idle = entry.active_connections == 0 && entry.websocket_connections == 0;
            if idle || now >= entry.deadline {
                completed.push(id.clone());
                let _ = self
                    .complete_tx
                    .send(DrainComplete {
                        app_name: entry.app_name.clone(),
                        instance_id: id.clone(),
                    })
                    .await;
            }
        }

        for id in &completed {
            self.draining.remove(id);
        }

        completed
    }

    /// Number of backends currently draining.
    pub fn draining_count(&self) -> usize {
        self.draining.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_cmd(app: &str, instance: &str, timeout_secs: u64) -> DrainCommand {
        DrainCommand {
            app_name: app.to_string(),
            instance_id: instance.to_string(),
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    #[tokio::test]
    async fn start_drain_adds_entry() {
        let (tx, _rx) = mpsc::channel(16);
        let mut tracker = DrainTracker::new(tx);

        let cmd = drain_cmd("web", "web-0", 30);
        assert!(tracker.start_drain(&cmd));
        assert!(tracker.is_draining("web-0"));
        assert_eq!(tracker.draining_count(), 1);
    }

    #[tokio::test]
    async fn duplicate_drain_returns_false() {
        let (tx, _rx) = mpsc::channel(16);
        let mut tracker = DrainTracker::new(tx);

        let cmd = drain_cmd("web", "web-0", 30);
        assert!(tracker.start_drain(&cmd));
        assert!(!tracker.start_drain(&cmd));
    }

    #[tokio::test]
    async fn zero_connections_completes_immediately() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut tracker = DrainTracker::new(tx);

        tracker.start_drain(&drain_cmd("web", "web-0", 30));

        let completed = tracker.check_completions().await;
        assert_eq!(completed, vec!["web-0"]);
        assert!(!tracker.is_draining("web-0"));

        // Should have sent completion notification
        let notification = rx.try_recv().unwrap();
        assert_eq!(notification.instance_id, "web-0");
        assert_eq!(notification.app_name, "web");
    }

    #[tokio::test]
    async fn active_connections_prevent_completion() {
        let (tx, _rx) = mpsc::channel(16);
        let mut tracker = DrainTracker::new(tx);

        tracker.start_drain(&drain_cmd("web", "web-0", 30));
        tracker.increment_connections("web-0");

        let completed = tracker.check_completions().await;
        assert!(completed.is_empty());
        assert!(tracker.is_draining("web-0"));
    }

    #[tokio::test]
    async fn connections_drained_then_completes() {
        let (tx, _rx) = mpsc::channel(16);
        let mut tracker = DrainTracker::new(tx);

        tracker.start_drain(&drain_cmd("web", "web-0", 30));
        tracker.increment_connections("web-0");
        tracker.increment_connections("web-0");

        // Still active
        let completed = tracker.check_completions().await;
        assert!(completed.is_empty());

        // Drain both connections
        tracker.decrement_connections("web-0");
        tracker.decrement_connections("web-0");

        let completed = tracker.check_completions().await;
        assert_eq!(completed, vec!["web-0"]);
    }

    #[tokio::test]
    async fn timeout_forces_completion() {
        let (tx, _rx) = mpsc::channel(16);
        let mut tracker = DrainTracker::new(tx);

        // Use a zero timeout so it expires immediately
        tracker.start_drain(&drain_cmd("web", "web-0", 0));
        tracker.increment_connections("web-0");

        // Despite active connections, timeout = 0 means deadline is already past
        // (or at Instant::now()). We need to wait a tiny bit.
        tokio::time::sleep(Duration::from_millis(1)).await;

        let completed = tracker.check_completions().await;
        assert_eq!(completed, vec!["web-0"]);
    }

    #[tokio::test]
    async fn not_draining_returns_false() {
        let (tx, _rx) = mpsc::channel(16);
        let tracker = DrainTracker::new(tx);

        assert!(!tracker.is_draining("nonexistent"));
    }

    #[tokio::test]
    async fn open_websocket_prevents_completion() {
        // ING4: a live WebSocket splice holds the drain open even though the
        // plain HTTP count is zero, so a rolling deploy waits for it.
        let (tx, _rx) = mpsc::channel(16);
        let mut tracker = DrainTracker::new(tx);

        tracker.start_drain(&drain_cmd("web", "web-0", 30));
        // A WebSocket bumps both the connection and the websocket counter.
        tracker.increment_connections("web-0");
        tracker.increment_websocket("web-0");

        // The HTTP request part finishes, but the WebSocket is still spliced.
        tracker.decrement_connections("web-0");
        let completed = tracker.check_completions().await;
        assert!(
            completed.is_empty(),
            "drain completed while a WebSocket was still open"
        );
        assert!(tracker.is_draining("web-0"));

        // The WebSocket closes; now the drain can complete.
        tracker.decrement_websocket("web-0");
        let completed = tracker.check_completions().await;
        assert_eq!(completed, vec!["web-0"]);
    }

    #[tokio::test]
    async fn websocket_drain_times_out_if_never_closes() {
        // ING4: a WebSocket that never closes still lets the deploy proceed
        // once the drain deadline passes, rather than blocking forever.
        let (tx, _rx) = mpsc::channel(16);
        let mut tracker = DrainTracker::new(tx);

        tracker.start_drain(&drain_cmd("web", "web-0", 0));
        tracker.increment_connections("web-0");
        tracker.increment_websocket("web-0");

        tokio::time::sleep(Duration::from_millis(1)).await;
        let completed = tracker.check_completions().await;
        assert_eq!(completed, vec!["web-0"]);
    }
}

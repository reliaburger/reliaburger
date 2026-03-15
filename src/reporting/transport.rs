/// Network transport for reporting tree messages.
///
/// Follows the same pattern as `MustardTransport`: a trait for
/// dependency injection with an in-memory implementation for testing.
/// The real TCP transport will be added when this is wired into the
/// agent. mTLS is deferred to Phase 4.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};

use super::ReportingError;
use super::types::ReportingMessage;

/// Transport for sending and receiving reporting tree messages.
///
/// Implementations must be `Send + Sync` for use across async tasks.
/// Uses RPITIT (Rust 2024) to avoid `async_trait` overhead.
pub trait ReportingTransport: Send + Sync {
    /// Send a reporting message to the given address.
    fn send(
        &self,
        target: SocketAddr,
        message: &ReportingMessage,
    ) -> impl std::future::Future<Output = Result<(), ReportingError>> + Send;

    /// Receive the next inbound reporting message.
    /// Returns the sender's address and the message.
    /// Returns `None` when the transport is shut down.
    fn recv(
        &self,
    ) -> impl std::future::Future<Output = Option<(SocketAddr, ReportingMessage)>> + Send;
}

// ---------------------------------------------------------------------------
// In-memory transport for testing
// ---------------------------------------------------------------------------

/// Routes reporting messages between nodes in the same process.
pub struct InMemoryReportingNetwork {
    inner: Arc<Mutex<NetworkInner>>,
}

struct NetworkInner {
    inboxes: HashMap<SocketAddr, mpsc::Sender<(SocketAddr, ReportingMessage)>>,
    partitions: Vec<(SocketAddr, SocketAddr)>,
}

impl InMemoryReportingNetwork {
    /// Create a new in-memory reporting network.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(NetworkInner {
                inboxes: HashMap::new(),
                partitions: Vec::new(),
            })),
        }
    }

    /// Create a transport handle for a node at the given address.
    pub async fn register(&self, address: SocketAddr) -> InMemoryReportingTransport {
        let (tx, rx) = mpsc::channel(256);
        let mut inner = self.inner.lock().await;
        inner.inboxes.insert(address, tx);
        InMemoryReportingTransport {
            address,
            network: Arc::clone(&self.inner),
            rx: Mutex::new(rx),
        }
    }

    /// Block all messages between two addresses (bidirectional).
    pub async fn partition(&self, a: SocketAddr, b: SocketAddr) {
        let mut inner = self.inner.lock().await;
        inner.partitions.push((a, b));
        inner.partitions.push((b, a));
    }

    /// Remove all partitions, restoring full connectivity.
    pub async fn heal(&self) {
        let mut inner = self.inner.lock().await;
        inner.partitions.clear();
    }
}

impl Default for InMemoryReportingNetwork {
    fn default() -> Self {
        Self::new()
    }
}

/// A single node's handle into the in-memory reporting network.
pub struct InMemoryReportingTransport {
    address: SocketAddr,
    network: Arc<Mutex<NetworkInner>>,
    rx: Mutex<mpsc::Receiver<(SocketAddr, ReportingMessage)>>,
}

impl InMemoryReportingTransport {
    /// Non-blocking receive for tests.
    pub fn try_recv(&self) -> Option<(SocketAddr, ReportingMessage)> {
        if let Ok(mut rx) = self.rx.try_lock() {
            rx.try_recv().ok()
        } else {
            None
        }
    }
}

impl ReportingTransport for InMemoryReportingTransport {
    async fn send(
        &self,
        target: SocketAddr,
        message: &ReportingMessage,
    ) -> Result<(), ReportingError> {
        let inner = self.network.lock().await;

        if inner
            .partitions
            .iter()
            .any(|(from, to)| *from == self.address && *to == target)
        {
            return Ok(());
        }

        if let Some(tx) = inner.inboxes.get(&target) {
            let _ = tx.try_send((self.address, message.clone()));
            Ok(())
        } else {
            Err(ReportingError::SendFailed {
                reason: format!("no node registered at {target}"),
            })
        }
    }

    async fn recv(&self) -> Option<(SocketAddr, ReportingMessage)> {
        let mut rx = self.rx.lock().await;
        rx.recv().await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meat::NodeId;
    use crate::reporting::types::{ResourceUsage, StateReport};
    use std::time::{Duration, SystemTime};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn sample_msg(name: &str) -> ReportingMessage {
        ReportingMessage::Report(StateReport {
            node_id: NodeId::new(name),
            timestamp: SystemTime::now(),
            running_apps: vec![],
            cached_specs: vec![],
            resource_usage: ResourceUsage::default(),
            event_log: vec![],
        })
    }

    #[tokio::test]
    async fn send_and_receive_between_two_nodes() {
        let net = InMemoryReportingNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        t1.send(addr(2), &sample_msg("w1")).await.unwrap();

        let (from, msg) = t2.recv().await.unwrap();
        assert_eq!(from, addr(1));
        match msg {
            ReportingMessage::Report(r) => assert_eq!(r.node_id, NodeId::new("w1")),
            _ => panic!("expected Report"),
        }
    }

    #[tokio::test]
    async fn send_to_unregistered_address_fails() {
        let net = InMemoryReportingNetwork::new();
        let t1 = net.register(addr(1)).await;

        let result = t1.send(addr(99), &sample_msg("w1")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn partition_drops_messages() {
        let net = InMemoryReportingNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        net.partition(addr(1), addr(2)).await;

        t1.send(addr(2), &sample_msg("w1")).await.unwrap();

        let result = tokio::time::timeout(Duration::from_millis(50), t2.recv()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn heal_restores_connectivity() {
        let net = InMemoryReportingNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        net.partition(addr(1), addr(2)).await;
        net.heal().await;

        t1.send(addr(2), &sample_msg("w1")).await.unwrap();
        let (from, _) = t2.recv().await.unwrap();
        assert_eq!(from, addr(1));
    }
}

/// Network transport abstraction for gossip messages.
///
/// Decouples the SWIM protocol logic from the actual network layer.
/// The real implementation sends UDP datagrams; the in-memory
/// implementation routes messages between `MustardNode` instances
/// in the same process for testing.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};

use super::MustardError;
use super::message::GossipMessage;

/// Network transport for sending and receiving gossip messages.
///
/// Implementations must be `Send + Sync` so they can be shared
/// across async tasks. The trait uses `impl Future` return types
/// (Rust 2024 edition RPITit) rather than `async_trait`, which
/// avoids the heap allocation that `Box<dyn Future>` requires.
pub trait MustardTransport: Send + Sync {
    /// Send a gossip message to a specific node address.
    fn send(
        &self,
        target: SocketAddr,
        message: &GossipMessage,
    ) -> impl std::future::Future<Output = Result<(), MustardError>> + Send;

    /// Receive the next inbound gossip message.
    /// Returns the sender's address and the message.
    /// Returns `None` when the transport is shut down.
    fn recv(&self)
    -> impl std::future::Future<Output = Option<(SocketAddr, GossipMessage)>> + Send;
}

// ---------------------------------------------------------------------------
// In-memory transport for testing
// ---------------------------------------------------------------------------

/// Routes gossip messages between nodes in the same process.
///
/// Each node gets its own `InMemoryTransport` handle, all connected
/// through a shared `InMemoryNetwork`. The network supports partition
/// injection for chaos testing.
pub struct InMemoryNetwork {
    inner: Arc<Mutex<NetworkInner>>,
}

struct NetworkInner {
    /// Per-address inbox: messages waiting to be received.
    inboxes: HashMap<SocketAddr, mpsc::Sender<(SocketAddr, GossipMessage)>>,
    /// Blocked routes: messages from A to B are silently dropped.
    partitions: Vec<(SocketAddr, SocketAddr)>,
}

impl InMemoryNetwork {
    /// Create a new in-memory network.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(NetworkInner {
                inboxes: HashMap::new(),
                partitions: Vec::new(),
            })),
        }
    }

    /// Create a transport handle for a node at the given address.
    /// The node can send to any other registered address and receive
    /// messages addressed to itself.
    pub async fn register(&self, address: SocketAddr) -> InMemoryTransport {
        let (tx, rx) = mpsc::channel(256);
        let mut inner = self.inner.lock().await;
        inner.inboxes.insert(address, tx);
        InMemoryTransport {
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

impl Default for InMemoryNetwork {
    fn default() -> Self {
        Self::new()
    }
}

/// A single node's handle into the in-memory network.
pub struct InMemoryTransport {
    address: SocketAddr,
    network: Arc<Mutex<NetworkInner>>,
    rx: Mutex<mpsc::Receiver<(SocketAddr, GossipMessage)>>,
}

impl MustardTransport for InMemoryTransport {
    async fn send(&self, target: SocketAddr, message: &GossipMessage) -> Result<(), MustardError> {
        let inner = self.network.lock().await;

        // Check for partition
        if inner
            .partitions
            .iter()
            .any(|(from, to)| *from == self.address && *to == target)
        {
            // Silently drop — the sender doesn't know about the partition
            return Ok(());
        }

        if let Some(tx) = inner.inboxes.get(&target) {
            // Ignore send errors (receiver dropped = node shut down)
            let _ = tx.try_send((self.address, message.clone()));
            Ok(())
        } else {
            Err(MustardError::SendFailed {
                reason: format!("no node registered at {target}"),
            })
        }
    }

    async fn recv(&self) -> Option<(SocketAddr, GossipMessage)> {
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
    use crate::mustard::message::{GossipMessage, GossipPayload};
    use crate::patty::NodeId;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn ping_msg(sender: &str) -> GossipMessage {
        GossipMessage::new(
            NodeId::new(sender),
            1,
            GossipPayload::Ping { updates: vec![] },
        )
    }

    #[tokio::test]
    async fn send_and_receive_between_two_nodes() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        t1.send(addr(2), &ping_msg("node-1")).await.unwrap();

        let (from, msg) = t2.recv().await.unwrap();
        assert_eq!(from, addr(1));
        assert_eq!(msg.sender, NodeId::new("node-1"));
    }

    #[tokio::test]
    async fn send_to_unregistered_address_fails() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;

        let result = t1.send(addr(99), &ping_msg("node-1")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn partition_drops_messages() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        net.partition(addr(1), addr(2)).await;

        // Send should succeed (sender doesn't know about partition)
        t1.send(addr(2), &ping_msg("node-1")).await.unwrap();

        // But receiver gets nothing
        let result = tokio::time::timeout(std::time::Duration::from_millis(50), t2.recv()).await;
        assert!(result.is_err()); // Timeout — no message arrived
    }

    #[tokio::test]
    async fn partition_is_bidirectional() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        net.partition(addr(1), addr(2)).await;

        // Neither direction works
        t2.send(addr(1), &ping_msg("node-2")).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_millis(50), t1.recv()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn heal_restores_connectivity() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        net.partition(addr(1), addr(2)).await;
        net.heal().await;

        t1.send(addr(2), &ping_msg("node-1")).await.unwrap();
        let (from, _) = t2.recv().await.unwrap();
        assert_eq!(from, addr(1));
    }

    #[tokio::test]
    async fn multiple_nodes_communicate() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;
        let t3 = net.register(addr(3)).await;

        // t1 sends to t2 and t3
        t1.send(addr(2), &ping_msg("node-1")).await.unwrap();
        t1.send(addr(3), &ping_msg("node-1")).await.unwrap();

        let (from2, _) = t2.recv().await.unwrap();
        let (from3, _) = t3.recv().await.unwrap();
        assert_eq!(from2, addr(1));
        assert_eq!(from3, addr(1));
    }

    #[tokio::test]
    async fn partition_only_affects_specified_pair() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;
        let t3 = net.register(addr(3)).await;

        // Partition 1<->2 but not 1<->3
        net.partition(addr(1), addr(2)).await;

        t1.send(addr(3), &ping_msg("node-1")).await.unwrap();
        let (from, _) = t3.recv().await.unwrap();
        assert_eq!(from, addr(1));

        // But 1->2 is blocked
        t1.send(addr(2), &ping_msg("node-1")).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_millis(50), t2.recv()).await;
        assert!(result.is_err());
    }
}

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
// TCP transport for production
// ---------------------------------------------------------------------------

/// Maximum reporting message size (1 MiB).
const MAX_REPORT_SIZE: usize = 1_048_576;

/// Real TCP transport for reporting tree messages.
///
/// Uses length-prefixed framing: 4-byte big-endian length + bincode payload.
/// Server mode accepts incoming connections (council members).
/// Client mode connects to the target for each send (workers).
pub struct TcpReportingTransport {
    address: SocketAddr,
    inbound_rx: Mutex<mpsc::Receiver<(SocketAddr, ReportingMessage)>>,
}

impl TcpReportingTransport {
    /// Create a TCP reporting transport bound to the given address.
    ///
    /// Spawns a background accept loop to receive inbound messages.
    pub async fn bind(
        addr: SocketAddr,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<Self, ReportingError> {
        let listener =
            tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| ReportingError::SendFailed {
                    reason: format!("failed to bind TCP on {addr}: {e}"),
                })?;
        let bound_addr = listener
            .local_addr()
            .map_err(|e| ReportingError::SendFailed {
                reason: format!("failed to get local address: {e}"),
            })?;

        let (inbound_tx, inbound_rx) = mpsc::channel(256);

        // Spawn accept loop
        tokio::spawn(Self::accept_loop(listener, inbound_tx, shutdown));

        Ok(Self {
            address: bound_addr,
            inbound_rx: Mutex::new(inbound_rx),
        })
    }

    /// The local address this transport is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// Background task: accept connections and read framed messages.
    async fn accept_loop(
        listener: tokio::net::TcpListener,
        tx: mpsc::Sender<(SocketAddr, ReportingMessage)>,
        shutdown: tokio_util::sync::CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer)) => {
                            let tx = tx.clone();
                            tokio::spawn(Self::handle_connection(stream, peer, tx));
                        }
                        Err(_) => continue,
                    }
                }
            }
        }
    }

    /// Read one framed message from a TCP connection.
    async fn handle_connection(
        mut stream: tokio::net::TcpStream,
        peer: SocketAddr,
        tx: mpsc::Sender<(SocketAddr, ReportingMessage)>,
    ) {
        use tokio::io::AsyncReadExt;

        // Read 4-byte length prefix
        let mut len_buf = [0u8; 4];
        if stream.read_exact(&mut len_buf).await.is_err() {
            return;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_REPORT_SIZE {
            return;
        }

        // Read payload
        let mut payload = vec![0u8; len];
        if stream.read_exact(&mut payload).await.is_err() {
            return;
        }

        // Deserialise
        if let Ok(msg) = bincode::deserialize::<ReportingMessage>(&payload) {
            let _ = tx.send((peer, msg)).await;
        }
    }

    /// Send a length-prefixed bincode message over a new TCP connection.
    async fn send_framed(
        target: SocketAddr,
        message: &ReportingMessage,
    ) -> Result<(), ReportingError> {
        use tokio::io::AsyncWriteExt;

        let payload = bincode::serialize(message)
            .map_err(|e| ReportingError::Serialisation(e.to_string()))?;
        if payload.len() > MAX_REPORT_SIZE {
            return Err(ReportingError::ReportTooLarge {
                size: payload.len(),
                max: MAX_REPORT_SIZE,
            });
        }

        let mut stream = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::net::TcpStream::connect(target),
        )
        .await
        .map_err(|_| ReportingError::SendFailed {
            reason: format!("TCP connect to {target} timed out"),
        })?
        .map_err(|e| ReportingError::SendFailed {
            reason: format!("TCP connect to {target}: {e}"),
        })?;

        let len_bytes = (payload.len() as u32).to_be_bytes();
        stream
            .write_all(&len_bytes)
            .await
            .map_err(|e| ReportingError::SendFailed {
                reason: format!("TCP write to {target}: {e}"),
            })?;
        stream
            .write_all(&payload)
            .await
            .map_err(|e| ReportingError::SendFailed {
                reason: format!("TCP write to {target}: {e}"),
            })?;

        Ok(())
    }
}

impl ReportingTransport for TcpReportingTransport {
    async fn send(
        &self,
        target: SocketAddr,
        message: &ReportingMessage,
    ) -> Result<(), ReportingError> {
        Self::send_framed(target, message).await
    }

    async fn recv(&self) -> Option<(SocketAddr, ReportingMessage)> {
        let mut rx = self.inbound_rx.lock().await;
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

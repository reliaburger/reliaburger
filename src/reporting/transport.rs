/// Network transport for reporting tree messages.
///
/// Follows the same pattern as `MustardTransport`: a trait for
/// dependency injection with an in-memory implementation for testing.
/// The TCP transport optionally runs over mTLS: when the node has an
/// identity, the accept loop requires a client certificate and sends dial
/// peers over TLS, using the same verifiers and CRL handle as the Raft RPC
/// transport.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};

use super::ReportingError;
use super::types::ReportingMessage;
use crate::meat::NodeId;

/// An inbound reporting message plus the sender's authenticated identity.
///
/// `peer_node_id` is `Some` only when the connection was mutually
/// authenticated and the peer's certificate carries a node SPIFFE id; it is
/// `None` on plaintext transports (mTLS off). The aggregator uses it to bind a
/// self-report's claimed `node_id` to the cert identity so a peer can't
/// overwrite another node's entry (C6).
pub type InboundReport = (SocketAddr, Option<NodeId>, ReportingMessage);

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
    /// Returns the sender's address, its authenticated node id (if the
    /// connection was mutually authenticated), and the message.
    /// Returns `None` when the transport is shut down.
    fn recv(&self) -> impl std::future::Future<Output = Option<InboundReport>> + Send;
}

// ---------------------------------------------------------------------------
// In-memory transport for testing
// ---------------------------------------------------------------------------

/// Routes reporting messages between nodes in the same process.
pub struct InMemoryReportingNetwork {
    inner: Arc<Mutex<NetworkInner>>,
}

struct NetworkInner {
    inboxes: HashMap<SocketAddr, mpsc::Sender<InboundReport>>,
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

    /// Create a transport handle for a node at the given address. Messages it
    /// sends carry no authenticated identity (models a plaintext peer).
    pub async fn register(&self, address: SocketAddr) -> InMemoryReportingTransport {
        self.register_as(address, None).await
    }

    /// Like [`register`](Self::register), but messages this handle sends are
    /// tagged with `authenticated_as` — modelling a peer whose mTLS client
    /// certificate binds it to that node id, for testing the C6 identity check.
    pub async fn register_as(
        &self,
        address: SocketAddr,
        authenticated_as: Option<NodeId>,
    ) -> InMemoryReportingTransport {
        let (tx, rx) = mpsc::channel(256);
        let mut inner = self.inner.lock().await;
        inner.inboxes.insert(address, tx);
        InMemoryReportingTransport {
            address,
            authenticated_as,
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
    /// The node id this handle's messages present as (mTLS cert identity).
    authenticated_as: Option<NodeId>,
    network: Arc<Mutex<NetworkInner>>,
    rx: Mutex<mpsc::Receiver<InboundReport>>,
}

impl InMemoryReportingTransport {
    /// Non-blocking receive for tests.
    pub fn try_recv(&self) -> Option<InboundReport> {
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
            let _ = tx.try_send((self.address, self.authenticated_as.clone(), message.clone()));
            Ok(())
        } else {
            Err(ReportingError::SendFailed {
                reason: format!("no node registered at {target}"),
            })
        }
    }

    async fn recv(&self) -> Option<InboundReport> {
        let mut rx = self.rx.lock().await;
        rx.recv().await
    }
}

// ---------------------------------------------------------------------------
// TCP transport for production
// ---------------------------------------------------------------------------

/// Maximum reporting message size (1 MiB).
const MAX_REPORT_SIZE: usize = 1_048_576;

/// How long the accept side waits for a peer to complete its handshake and
/// deliver a full framed message before dropping the connection (CP11). A
/// stalled peer (partial length prefix, half-open TLS) must not hold the task.
const REPORT_ACCEPT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Real TCP transport for reporting tree messages.
///
/// Uses length-prefixed framing: 4-byte big-endian length + bincode payload.
/// Server mode accepts incoming connections (council members).
/// Client mode connects to the target for each send (workers).
/// Supports a runtime blocklist for chaos testing.
pub struct TcpReportingTransport {
    address: SocketAddr,
    inbound_rx: Mutex<mpsc::Receiver<InboundReport>>,
    blocklist: std::sync::Arc<tokio::sync::RwLock<std::collections::HashSet<SocketAddr>>>,
    /// When set, peers are dialled over mTLS. `None` keeps plaintext TCP.
    tls_connector: Option<tokio_rustls::TlsConnector>,
}

impl TcpReportingTransport {
    /// Create a plaintext TCP reporting transport bound to the given address.
    pub async fn bind(
        addr: SocketAddr,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<Self, ReportingError> {
        Self::bind_tls(addr, shutdown, None, None).await
    }

    /// Create a TCP reporting transport, optionally over mTLS.
    ///
    /// When `acceptor` is set the accept loop requires a client certificate;
    /// when `connector` is set, sends dial peers over TLS. Spawns a background
    /// accept loop to receive inbound messages.
    pub async fn bind_tls(
        addr: SocketAddr,
        shutdown: tokio_util::sync::CancellationToken,
        acceptor: Option<tokio_rustls::TlsAcceptor>,
        connector: Option<tokio_rustls::TlsConnector>,
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
        tokio::spawn(Self::accept_loop(listener, inbound_tx, shutdown, acceptor));

        Ok(Self {
            address: bound_addr,
            inbound_rx: Mutex::new(inbound_rx),
            blocklist: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashSet::new(),
            )),
            tls_connector: connector,
        })
    }

    /// The local address this transport is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// Get a handle to the blocklist for chaos injection.
    pub fn blocklist(
        &self,
    ) -> std::sync::Arc<tokio::sync::RwLock<std::collections::HashSet<SocketAddr>>> {
        std::sync::Arc::clone(&self.blocklist)
    }

    /// Background task: accept connections and read framed messages.
    async fn accept_loop(
        listener: tokio::net::TcpListener,
        tx: mpsc::Sender<InboundReport>,
        shutdown: tokio_util::sync::CancellationToken,
        acceptor: Option<tokio_rustls::TlsAcceptor>,
    ) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer)) => {
                            let tx = tx.clone();
                            match acceptor.clone() {
                                Some(acceptor) => {
                                    tokio::spawn(async move {
                                        // A refused/failed handshake is dropped
                                        // silently; a rejected peer learns nothing.
                                        // One deadline covers the handshake and
                                        // the framed read so a stalled peer can't
                                        // pin the task (CP11).
                                        let _ = tokio::time::timeout(
                                            REPORT_ACCEPT_DEADLINE,
                                            async {
                                                if let Ok(tls) = acceptor.accept(stream).await {
                                                    // The verified client cert
                                                    // binds the peer's identity
                                                    // (C6); extract it before
                                                    // reading the message.
                                                    let peer_id = peer_node_id_from_tls(&tls);
                                                    Self::handle_connection(tls, peer, peer_id, tx)
                                                        .await;
                                                }
                                            },
                                        )
                                        .await;
                                    });
                                }
                                None => {
                                    tokio::spawn(async move {
                                        let _ = tokio::time::timeout(
                                            REPORT_ACCEPT_DEADLINE,
                                            Self::handle_connection(stream, peer, None, tx),
                                        )
                                        .await;
                                    });
                                }
                            }
                        }
                        Err(_) => continue,
                    }
                }
            }
        }
    }

    /// Read one framed message from any byte stream (plain TCP or TLS).
    async fn handle_connection<S: tokio::io::AsyncRead + Unpin>(
        mut stream: S,
        peer: SocketAddr,
        peer_node_id: Option<NodeId>,
        tx: mpsc::Sender<InboundReport>,
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
            let _ = tx.send((peer, peer_node_id, msg)).await;
        }
    }

    /// Send a length-prefixed bincode message over a new connection.
    async fn send_framed(
        target: SocketAddr,
        message: &ReportingMessage,
        connector: Option<&tokio_rustls::TlsConnector>,
    ) -> Result<(), ReportingError> {
        let payload = bincode::serialize(message)
            .map_err(|e| ReportingError::Serialisation(e.to_string()))?;
        if payload.len() > MAX_REPORT_SIZE {
            return Err(ReportingError::ReportTooLarge {
                size: payload.len(),
                max: MAX_REPORT_SIZE,
            });
        }

        let tcp = tokio::time::timeout(
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

        match connector {
            Some(connector) => {
                // The pinned server verifier ignores the name; rustls still
                // requires a valid one.
                let name = rustls::pki_types::ServerName::IpAddress(target.ip().into());
                let tls =
                    connector
                        .connect(name, tcp)
                        .await
                        .map_err(|e| ReportingError::SendFailed {
                            reason: format!("TLS connect to {target}: {e}"),
                        })?;
                Self::write_framed(tls, &payload, target).await
            }
            None => Self::write_framed(tcp, &payload, target).await,
        }
    }

    /// Write a length-prefixed payload over an established stream.
    async fn write_framed<S: tokio::io::AsyncWrite + Unpin>(
        mut stream: S,
        payload: &[u8],
        target: SocketAddr,
    ) -> Result<(), ReportingError> {
        use tokio::io::AsyncWriteExt;

        let len_bytes = (payload.len() as u32).to_be_bytes();
        stream
            .write_all(&len_bytes)
            .await
            .map_err(|e| ReportingError::SendFailed {
                reason: format!("TCP write to {target}: {e}"),
            })?;
        stream
            .write_all(payload)
            .await
            .map_err(|e| ReportingError::SendFailed {
                reason: format!("TCP write to {target}: {e}"),
            })?;
        // TLS buffers until flushed; harmless on plain TCP.
        stream
            .flush()
            .await
            .map_err(|e| ReportingError::SendFailed {
                reason: format!("TCP flush to {target}: {e}"),
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
        if self.blocklist.read().await.contains(&target) {
            return Ok(()); // silently drop for chaos testing
        }
        Self::send_framed(target, message, self.tls_connector.as_ref()).await
    }

    async fn recv(&self) -> Option<InboundReport> {
        let mut rx = self.inbound_rx.lock().await;
        rx.recv().await
    }
}

/// Extract the peer's node id from its verified TLS client certificate.
///
/// The accept path only reaches here after the client verifier accepted the
/// chain, so the leaf certificate is trusted; we read the node SPIFFE id from
/// its URI SAN. Returns `None` if the peer presented no certificate or none
/// carrying a node identity (in which case the aggregator applies no binding).
fn peer_node_id_from_tls(
    tls: &tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
) -> Option<NodeId> {
    let (_, connection) = tls.get_ref();
    let leaf = connection.peer_certificates()?.first()?;
    let uris = crate::sesame::cert::subject_uri_sans(leaf.as_ref()).ok()?;
    uris.iter()
        .find_map(|uri| crate::sesame::ca::node_id_from_spiffe_uri(uri))
        .map(NodeId::new)
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
            has_buildah: false,
            node_id: NodeId::new(name),
            timestamp: SystemTime::now(),
            running_apps: vec![],
            cached_specs: vec![],
            resource_usage: ResourceUsage::default(),
            event_log: vec![],
        })
    }

    #[tokio::test]
    async fn handle_connection_never_completes_on_a_stalled_half_frame() {
        // CP11: a peer that sends a partial length prefix then stalls would
        // hold handle_connection open forever. Under a short timeout (standing
        // in for REPORT_ACCEPT_DEADLINE) it is abandoned instead.
        use tokio::io::AsyncWriteExt;

        let (mut client, server) = tokio::io::duplex(64);
        client.write_all(&[0u8, 0u8]).await.unwrap();

        let (tx, _rx) = mpsc::channel(4);
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            TcpReportingTransport::handle_connection(server, addr(1), None, tx),
        )
        .await;
        assert!(
            result.is_err(),
            "a stalled half-frame must be cut off by the deadline, not return"
        );
    }

    #[tokio::test]
    async fn send_and_receive_between_two_nodes() {
        let net = InMemoryReportingNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        t1.send(addr(2), &sample_msg("w1")).await.unwrap();

        let (from, _, msg) = t2.recv().await.unwrap();
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
        let (from, _, _) = t2.recv().await.unwrap();
        assert_eq!(from, addr(1));
    }
}

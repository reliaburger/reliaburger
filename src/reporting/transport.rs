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
    /// Send a report, returning an error when transport admission is refused
    /// or unacknowledged. Success does not promise durable processing.
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
            return Err(ReportingError::SendFailed {
                reason: "reporting target is partitioned".into(),
            });
        }

        if let Some(tx) = inner.inboxes.get(&target) {
            tx.try_send((self.address, self.authenticated_as.clone(), message.clone()))
                .map_err(|error| ReportingError::SendFailed {
                    reason: error.to_string(),
                })
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
const MAX_REPORT_CONNECTIONS: usize = 16;
const MAX_QUEUED_REPORTS: usize = 16;
const ADMITTED: u8 = 1;
/// Fixed event admission limit for protocol generation three.
pub(crate) const MAX_EVENTS_PER_REPORT: usize = 100;

/// How long the accept side waits for a peer to complete its handshake and
/// deliver a full framed message before dropping the connection (CP11). A
/// stalled peer (partial length prefix, half-open TLS) must not hold the task.
const REPORT_ACCEPT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Outbound-only reporting transport for worker snapshots and rollups.
/// It owns no listening socket or accept task, so workers cannot occupy a
/// service port while that service is restarting.
pub struct TcpReportingSender {
    connector: Option<tokio_rustls::TlsConnector>,
    node_gate: crate::smoker::node_fault::NodeTransportGate,
}

impl TcpReportingSender {
    /// Create a sender retaining the cluster's TLS identity and fault gate.
    pub fn new(
        connector: Option<tokio_rustls::TlsConnector>,
        node_gate: crate::smoker::node_fault::NodeTransportGate,
    ) -> Self {
        Self {
            connector,
            node_gate,
        }
    }
}

impl ReportingTransport for TcpReportingSender {
    async fn send(
        &self,
        target: SocketAddr,
        message: &ReportingMessage,
    ) -> Result<(), ReportingError> {
        if self.node_gate.is_quiesced() {
            return Err(ReportingError::SendFailed {
                reason: "reporting is quiesced".into(),
            });
        }
        TcpReportingTransport::send_framed(target, message, self.connector.as_ref()).await
    }

    async fn recv(&self) -> Option<InboundReport> {
        None
    }
}

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
    node_gate: crate::smoker::node_fault::NodeTransportGate,
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

    /// Create a plaintext transport sharing a reversible node-fault gate.
    pub async fn bind_with_node_gate(
        addr: SocketAddr,
        shutdown: tokio_util::sync::CancellationToken,
        node_gate: crate::smoker::node_fault::NodeTransportGate,
    ) -> Result<Self, ReportingError> {
        Self::bind_tls_with_node_gate(addr, shutdown, None, None, node_gate).await
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
        Self::bind_tls_with_node_gate(
            addr,
            shutdown,
            acceptor,
            connector,
            crate::smoker::node_fault::NodeTransportGate::new(),
        )
        .await
    }

    /// Create a TCP reporting transport sharing the cluster-wide node gate.
    pub async fn bind_tls_with_node_gate(
        addr: SocketAddr,
        shutdown: tokio_util::sync::CancellationToken,
        acceptor: Option<tokio_rustls::TlsAcceptor>,
        connector: Option<tokio_rustls::TlsConnector>,
        node_gate: crate::smoker::node_fault::NodeTransportGate,
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

        let (inbound_tx, inbound_rx) = mpsc::channel(MAX_QUEUED_REPORTS);

        // Spawn accept loop
        tokio::spawn(Self::accept_loop(
            listener,
            inbound_tx,
            shutdown,
            acceptor,
            node_gate.clone(),
        ));

        Ok(Self {
            address: bound_addr,
            inbound_rx: Mutex::new(inbound_rx),
            blocklist: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashSet::new(),
            )),
            node_gate,
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
        node_gate: crate::smoker::node_fault::NodeTransportGate,
    ) {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            // Completed tasks also occupy JoinSet storage until reaped.
            while connections.try_join_next().is_some() {}
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = connections.join_next(), if !connections.is_empty() => {},
                result = listener.accept() => {
                    let Ok((stream, peer)) = result else { continue };
                    if node_gate.is_quiesced() { continue; }
                    if connections.len() >= MAX_REPORT_CONNECTIONS {
                        // Refuse immediately rather than spawn a task waiting for capacity.
                        continue;
                    }
                    let tx = tx.clone();
                    let acceptor = acceptor.clone();
                    let connection_gate = node_gate.clone();
                    connections.spawn(async move {
                        let _ = tokio::time::timeout(REPORT_ACCEPT_DEADLINE, async {
                            match acceptor {
                                Some(acceptor) => {
                                    if let Ok(tls) = acceptor.accept(stream).await
                                        && !connection_gate.is_quiesced()
                                    {
                                        let peer_id = peer_node_id_from_tls(&tls);
                                        Self::handle_connection(tls, peer, peer_id, tx).await;
                                    }
                                }
                                None => {
                                    if !connection_gate.is_quiesced() {
                                        Self::handle_connection(stream, peer, None, tx).await;
                                    }
                                }
                            }
                        }).await;
                    });
                }
            }
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }

    /// Read one framed message from any byte stream (plain TCP or TLS).
    async fn handle_connection<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        mut stream: S,
        peer: SocketAddr,
        peer_node_id: Option<NodeId>,
        tx: mpsc::Sender<InboundReport>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

        if let Ok(msg) = decode_report(&payload)
            && tx.try_send((peer, peer_node_id, msg)).is_ok()
        {
            // Admission is volatile, not a durable processing receipt. A lost
            // ACK can cause a retry; snapshots and owned rollups are idempotent.
            let _ = stream.write_all(&[ADMITTED]).await;
            let _ = stream.flush().await;
        }
    }

    /// Send a length-prefixed bincode message over a new connection.
    async fn send_framed(
        target: SocketAddr,
        message: &ReportingMessage,
        connector: Option<&tokio_rustls::TlsConnector>,
    ) -> Result<(), ReportingError> {
        let payload = encode_report(message)?;

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

        // Bound the handshake and write too (O8): a peer that completes the TCP
        // connect then stalls during the TLS handshake or the body write would
        // otherwise hang the sender on TCP defaults. Wrap the remainder in one
        // overall deadline.
        let deadline = std::time::Duration::from_secs(10);
        let write = async {
            match connector {
                Some(connector) => {
                    // The pinned server verifier ignores the name; rustls still
                    // requires a valid one.
                    let name = rustls::pki_types::ServerName::IpAddress(target.ip().into());
                    let tls = connector.connect(name, tcp).await.map_err(|e| {
                        ReportingError::SendFailed {
                            reason: format!("TLS connect to {target}: {e}"),
                        }
                    })?;
                    Self::write_framed(tls, &payload, target).await
                }
                None => Self::write_framed(tcp, &payload, target).await,
            }
        };
        tokio::time::timeout(deadline, write)
            .await
            .map_err(|_| ReportingError::SendFailed {
                reason: format!("send to {target} timed out"),
            })?
    }

    /// Write a length-prefixed payload over an established stream.
    async fn write_framed<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        mut stream: S,
        payload: &[u8],
        target: SocketAddr,
    ) -> Result<(), ReportingError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        let admission = stream
            .read_u8()
            .await
            .map_err(|error| ReportingError::SendFailed {
                reason: format!("report admission at {target} was not acknowledged: {error}"),
            })?;
        if admission != ADMITTED {
            return Err(ReportingError::SendFailed {
                reason: format!("report admission at {target} was refused"),
            });
        }
        Ok(())
    }
}

impl ReportingTransport for TcpReportingTransport {
    async fn send(
        &self,
        target: SocketAddr,
        message: &ReportingMessage,
    ) -> Result<(), ReportingError> {
        if self.node_gate.is_quiesced() {
            return Err(ReportingError::SendFailed {
                reason: "reporting is quiesced".into(),
            });
        }
        if self.blocklist.read().await.contains(&target) {
            return Err(ReportingError::SendFailed {
                reason: "reporting target is partitioned".into(),
            });
        }
        Self::send_framed(target, message, self.tls_connector.as_ref()).await
    }

    async fn recv(&self) -> Option<InboundReport> {
        let mut rx = self.inbound_rx.lock().await;
        loop {
            let report = rx.recv().await?;
            if !self.node_gate.is_quiesced() {
                return Some(report);
            }
        }
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

// The fixed header is inspected before decoding any peer-controlled collections.
// Development frames have no header and are refused without interpreting them.
fn report_header() -> [u8; 12] {
    let mut header = [0; 12];
    header[..4].copy_from_slice(b"RBRP");
    header[4..8].copy_from_slice(&crate::compatibility::CURRENT.protocol.to_be_bytes());
    header[8..].copy_from_slice(&crate::compatibility::CURRENT.state.to_be_bytes());
    header
}

fn check_event_admission(message: &ReportingMessage) -> Result<(), String> {
    let too_many = match message {
        ReportingMessage::Report(report) => report.event_log.len() > MAX_EVENTS_PER_REPORT,
        ReportingMessage::AggregatedReport { reports } => reports
            .values()
            .any(|report| report.event_log.len() > MAX_EVENTS_PER_REPORT),
        _ => false,
    };
    if too_many {
        return Err(format!(
            "report exceeds the {MAX_EVENTS_PER_REPORT}-event admission limit; no events admitted"
        ));
    }
    Ok(())
}

fn encode_report(message: &ReportingMessage) -> Result<Vec<u8>, ReportingError> {
    check_event_admission(message).map_err(|reason| ReportingError::SendFailed { reason })?;
    // The size pass traverses borrowed data without allocating an encoded copy.
    let body_size = bincode::serialized_size(message)
        .map_err(|error| ReportingError::Serialisation(error.to_string()))?;
    let size = body_size.saturating_add(report_header().len() as u64);
    if size > MAX_REPORT_SIZE as u64 {
        return Err(ReportingError::ReportTooLarge {
            size: usize::try_from(size).unwrap_or(usize::MAX),
            max: MAX_REPORT_SIZE,
        });
    }
    let mut bytes = Vec::with_capacity(size as usize);
    bytes.extend(report_header());
    bincode::serialize_into(&mut bytes, message)
        .map_err(|error| ReportingError::Serialisation(error.to_string()))?;
    Ok(bytes)
}

fn decode_report(payload: &[u8]) -> Result<ReportingMessage, bincode::Error> {
    use bincode::Options;
    let body = payload.strip_prefix(&report_header()).ok_or_else(|| {
        Box::new(bincode::ErrorKind::Custom(
            "incompatible reporting formats".into(),
        ))
    })?;
    let message = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(body.len() as u64)
        .reject_trailing_bytes()
        .deserialize(body)?;
    check_event_admission(&message)
        .map_err(|reason| Box::new(bincode::ErrorKind::Custom(reason)))?;
    Ok(message)
}

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
    async fn oversized_events_and_metrics_refuse_without_partial_delivery() {
        use crate::mayo::rollup::{NodeRollup, RollupAggregate, RollupEntry};
        use crate::reporting::{EventKind, NodeEvent};
        let shutdown = tokio_util::sync::CancellationToken::new();
        let receiver = TcpReportingTransport::bind(addr(0), shutdown.clone())
            .await
            .unwrap();
        let sender = TcpReportingSender::new(None, Default::default());
        let ReportingMessage::Report(mut events) = sample_msg("worker") else {
            unreachable!()
        };
        events.event_log.push(NodeEvent {
            timestamp: SystemTime::now(),
            kind: EventKind::ContainerStart,
            detail: "e".repeat(MAX_REPORT_SIZE),
        });
        let metrics = ReportingMessage::MetricsRollup(NodeRollup {
            node_id: NodeId::new("worker"),
            timestamp: 60,
            entries: vec![RollupEntry {
                metric_name: "m".repeat(MAX_REPORT_SIZE),
                labels: Default::default(),
                aggregate: RollupAggregate {
                    min: 1.0,
                    max: 1.0,
                    sum: 1.0,
                    count: 1,
                },
            }],
        });
        for message in [ReportingMessage::Report(events.clone()), metrics] {
            assert!(matches!(
                sender.send(receiver.local_addr(), &message).await,
                Err(ReportingError::ReportTooLarge { .. })
            ));
        }
        events.event_log[0].detail = "small".into();
        events.event_log = vec![events.event_log[0].clone(); 101];
        assert!(
            sender
                .send(receiver.local_addr(), &ReportingMessage::Report(events))
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), receiver.recv())
                .await
                .is_err()
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn exact_byte_limit_is_admitted_and_one_extra_byte_is_refused() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let receiver = TcpReportingTransport::bind(addr(0), shutdown.clone())
            .await
            .unwrap();
        let sender = TcpReportingSender::new(None, Default::default());
        let empty_size = bincode::serialized_size(&sample_msg("")).unwrap() as usize + 12;
        let name = "w".repeat(MAX_REPORT_SIZE - empty_size);
        sender
            .send(receiver.local_addr(), &sample_msg(&name))
            .await
            .unwrap();
        let (_, _, received) = receiver.recv().await.unwrap();
        let ReportingMessage::Report(report) = received else {
            panic!("missing report")
        };
        assert_eq!(report.node_id, NodeId::new(&name));
        assert!(matches!(
            sender
                .send(receiver.local_addr(), &sample_msg(&(name + "x")))
                .await,
            Err(ReportingError::ReportTooLarge { .. })
        ));
        shutdown.cancel();
    }

    #[tokio::test]
    async fn stalled_connections_cannot_create_unbounded_receiver_tasks() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let shutdown = tokio_util::sync::CancellationToken::new();
        let receiver = TcpReportingTransport::bind(addr(0), shutdown.clone())
            .await
            .unwrap();
        let mut clients = Vec::new();
        for _ in 0..MAX_REPORT_CONNECTIONS {
            let mut client = tokio::net::TcpStream::connect(receiver.local_addr())
                .await
                .unwrap();
            client.write_all(&[0, 0]).await.unwrap();
            clients.push(client);
        }
        let mut excess = tokio::net::TcpStream::connect(receiver.local_addr())
            .await
            .unwrap();
        let mut byte = [0];
        let refused = tokio::time::timeout(Duration::from_secs(1), excess.read(&mut byte)).await;
        assert!(
            matches!(refused, Ok(Ok(0)) | Ok(Err(_))),
            "capacity was not refused: {refused:?}"
        );
        shutdown.cancel();
        drop(clients);
    }

    #[tokio::test]
    async fn full_receiver_refuses_admission_instead_of_acknowledging_loss() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let receiver = TcpReportingTransport::bind(addr(0), shutdown.clone())
            .await
            .unwrap();
        let sender = TcpReportingSender::new(None, Default::default());
        for _ in 0..16 {
            sender
                .send(receiver.local_addr(), &sample_msg("worker"))
                .await
                .unwrap();
        }
        assert!(
            sender
                .send(receiver.local_addr(), &sample_msg("overflow"))
                .await
                .is_err()
        );
        assert!(receiver.recv().await.is_some());
        sender
            .send(receiver.local_addr(), &sample_msg("retry"))
            .await
            .unwrap();
        shutdown.cancel();
    }

    #[tokio::test]
    async fn completed_write_without_admission_ack_is_a_failure() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind(addr(0)).await.unwrap();
        let target = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let length = stream.read_u32().await.unwrap();
            let mut payload = vec![0; length as usize];
            stream.read_exact(&mut payload).await.unwrap();
            // Simulate a receiver dying before admitting the message.
        });
        let sender = TcpReportingSender::new(None, Default::default());
        assert!(sender.send(target, &sample_msg("worker")).await.is_err());
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_closes_stalled_reporting_connections() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let shutdown = tokio_util::sync::CancellationToken::new();
        let receiver = TcpReportingTransport::bind(addr(0), shutdown.clone())
            .await
            .unwrap();
        let mut client = tokio::net::TcpStream::connect(receiver.local_addr())
            .await
            .unwrap();
        client.write_all(&[0, 0]).await.unwrap();
        tokio::task::yield_now().await;
        shutdown.cancel();
        let mut byte = [0];
        let result = tokio::time::timeout(Duration::from_secs(1), client.read(&mut byte)).await;
        assert!(
            matches!(result, Ok(Ok(0)) | Ok(Err(_))),
            "stalled peer survived shutdown: {result:?}"
        );
    }

    #[test]
    fn reporting_rejects_either_format_mismatch_and_oversized_collection_claims() {
        let valid = encode_report(&sample_msg("current")).unwrap();
        assert!(decode_report(&valid).is_ok());
        for offset in [7, 11] {
            let mut wrong = valid.clone();
            wrong[offset] = 99;
            assert!(decode_report(&wrong).is_err());
        }
        // Report enum discriminant, then an impossible node-id string length.
        let mut malicious = report_header().to_vec();
        malicious.extend(0u32.to_le_bytes());
        malicious.push(0); // has_buildah
        malicious.extend(u64::MAX.to_le_bytes());
        assert!(decode_report(&malicious).is_err());
    }

    #[tokio::test]
    async fn legacy_reporting_frame_never_reaches_the_inbox() {
        let payload = bincode::serialize(&sample_msg("legacy")).unwrap();
        let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
        frame.extend(payload);
        let (tx, mut rx) = mpsc::channel(1);
        use tokio::io::AsyncWriteExt;
        let (mut client, server) = tokio::io::duplex(frame.len());
        client.write_all(&frame).await.unwrap();
        TcpReportingTransport::handle_connection(server, addr(1234), None, tx).await;
        assert!(rx.recv().await.is_none());
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

        assert!(t1.send(addr(2), &sample_msg("w1")).await.is_err());

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

    #[tokio::test]
    async fn node_transport_gate_drops_and_then_restores_reports() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let gate = crate::smoker::node_fault::NodeTransportGate::new();
        let t1 = TcpReportingSender::new(None, gate.clone());
        assert!(
            t1.recv().await.is_none(),
            "send-only workers have no inbound listener"
        );
        let t2 = TcpReportingTransport::bind(addr(0), shutdown.clone())
            .await
            .unwrap();

        gate.quiesce();
        assert!(t1.send(t2.local_addr(), &sample_msg("w1")).await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), t2.recv())
                .await
                .is_err()
        );

        gate.restore();
        t1.send(t2.local_addr(), &sample_msg("w1")).await.unwrap();
        let (_, _, message) = tokio::time::timeout(Duration::from_millis(500), t2.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(message, ReportingMessage::Report(_)));
        shutdown.cancel();
    }
}

/// Network transport for reporting tree messages.
///
/// Follows the same pattern as `MustardTransport`: a trait for
/// dependency injection with an in-memory implementation for testing.
/// The TCP transport optionally runs over mTLS: when the node has an
/// identity, the accept loop requires a client certificate and sends dial
/// peers over TLS, using the same verifiers and CRL handle as the Raft RPC
/// transport.
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
/// Connections that have delivered a whole report and are being admitted:
/// waiting for room in the queue, then writing the acknowledgement. Past
/// this, connections wait rather than being refused (#1411): refusing them
/// cost 15,310 lost reports at 500 nodes.
const MAX_REPORT_CONNECTIONS: usize = 64;
/// Connections that haven't yet delivered a whole report: the TLS handshake
/// (under mTLS) and the framed message. They have proved nothing, so they
/// get a budget of their own, and can't spend the one above (#2380). The
/// listener doesn't accept past this; later connections wait in the
/// kernel's listen backlog.
const MAX_PENDING_CONNECTIONS: usize = 64;
/// Pending connections one source address may hold. A node sends its
/// reports one after another, so it needs one. Loopback is exempt: the
/// hollow-node harness runs a thousand nodes on 127.0.0.1.
const MAX_PENDING_PER_SOURCE: usize = 4;
/// How long a pending connection keeps its slot while a newer one waits
/// for it. An honest sender finishes its handshake and frame in
/// milliseconds, so the oldest pending connection is the likeliest to be
/// stalled, and it is closed to make room.
const PENDING_EVICTION_AGE: std::time::Duration = std::time::Duration::from_secs(1);
/// Reports admitted but not yet taken by the aggregator. A connection whose
/// report finds the queue full waits for room, inside the accept deadline,
/// rather than being refused.
const MAX_QUEUED_REPORTS: usize = 256;
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
/// Uses length-prefixed framing: 4-byte big-endian length + postcard payload.
/// Server mode accepts incoming connections (council members).
/// Client mode connects to the target for each send (workers).
/// Supports a runtime blocklist for chaos testing.
pub struct TcpReportingTransport {
    address: SocketAddr,
    inbound_rx: Mutex<mpsc::Receiver<InboundReport>>,
    /// Connection tasks alive right now, pending and admitting together.
    connection_tasks: Arc<AtomicUsize>,
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
        let connection_tasks = Arc::new(AtomicUsize::new(0));

        // Spawn accept loop
        tokio::spawn(Self::accept_loop(
            listener,
            inbound_tx,
            shutdown,
            acceptor,
            node_gate.clone(),
            Arc::clone(&connection_tasks),
        ));

        Ok(Self {
            address: bound_addr,
            inbound_rx: Mutex::new(inbound_rx),
            connection_tasks,
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

    /// How many connection tasks the listener has running: never more than
    /// its pending and admission budgets together.
    pub fn connection_tasks(&self) -> usize {
        self.connection_tasks.load(Ordering::SeqCst)
    }

    /// Get a handle to the blocklist for chaos injection.
    pub fn blocklist(
        &self,
    ) -> std::sync::Arc<tokio::sync::RwLock<std::collections::HashSet<SocketAddr>>> {
        std::sync::Arc::clone(&self.blocklist)
    }

    /// Background task: accept connections and read framed messages.
    ///
    /// Two budgets, so a socket that proves nothing can't hold a slot a
    /// node's report needs (#2380). A connection is *pending* from accept
    /// until its whole frame has arrived (with mTLS, that includes the
    /// handshake that checks its certificate), and takes one of
    /// `MAX_PENDING_CONNECTIONS` slots. With a whole frame in, it waits for
    /// one of `MAX_REPORT_CONNECTIONS` admission slots, then gives its
    /// pending slot back. Each task holds at least one slot for its whole
    /// life, so there are never more than the two budgets together.
    async fn accept_loop(
        listener: tokio::net::TcpListener,
        tx: mpsc::Sender<InboundReport>,
        shutdown: tokio_util::sync::CancellationToken,
        acceptor: Option<tokio_rustls::TlsAcceptor>,
        node_gate: crate::smoker::node_fault::NodeTransportGate,
        connection_tasks: Arc<AtomicUsize>,
    ) {
        let mut connections = tokio::task::JoinSet::new();
        let pending_slots = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_CONNECTIONS));
        let report_slots = Arc::new(tokio::sync::Semaphore::new(MAX_REPORT_CONNECTIONS));
        // Pending connections, oldest first.
        let mut pending: VecDeque<PendingConnection> = VecDeque::new();
        'accept: loop {
            // Completed tasks also occupy JoinSet storage until reaped.
            while connections.try_join_next().is_some() {}
            let (stream, peer) = tokio::select! {
                _ = shutdown.cancelled() => break,
                result = listener.accept() => match result {
                    Ok(accepted) => accepted,
                    Err(_) => continue,
                },
            };
            if node_gate.is_quiesced() {
                continue;
            }
            pending.retain_mut(PendingConnection::is_pending);
            if source_is_full(peer.ip(), &pending) {
                continue;
            }
            // A slot for the newcomer: a free one, or the oldest pending
            // connection's once it has stalled for PENDING_EVICTION_AGE.
            // Until then the newcomer waits here, and the connections behind
            // it wait in the listen backlog: backpressure, not refusal.
            let pending_slot = loop {
                let eviction_due = pending
                    .front()
                    .map(|oldest| oldest.since + PENDING_EVICTION_AGE);
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break 'accept,
                    permit = Arc::clone(&pending_slots).acquire_owned() => match permit {
                        Ok(permit) => break permit,
                        Err(_) => break 'accept,
                    },
                    _ = sleep_until_due(eviction_due) => {
                        if let Some(oldest) = pending.pop_front() {
                            oldest.evict.cancel();
                            // Wait for it to go, which gives its slot back,
                            // so one newcomer closes one connection, not
                            // every old one at once.
                            let _ = oldest.still_pending.await;
                        }
                        pending.retain_mut(PendingConnection::is_pending);
                    }
                }
            };
            let (entry, slot) =
                PendingConnection::start(peer.ip(), pending_slot, Arc::clone(&report_slots));
            pending.push_back(entry);
            let tx = tx.clone();
            let acceptor = acceptor.clone();
            let connection_gate = node_gate.clone();
            let task_count = TaskCount::start(&connection_tasks);
            connections.spawn(async move {
                let _task_count = task_count;
                let _ = tokio::time::timeout(REPORT_ACCEPT_DEADLINE, async {
                    match acceptor {
                        Some(acceptor) => {
                            let handshake = tokio::select! {
                                biased;
                                _ = slot.evict.cancelled() => return,
                                handshake = acceptor.accept(stream) => handshake,
                            };
                            if let Ok(tls) = handshake
                                && !connection_gate.is_quiesced()
                            {
                                let peer_id = peer_node_id_from_tls(&tls);
                                Self::handle_connection(tls, peer, peer_id, tx, slot).await;
                            }
                        }
                        None => {
                            if !connection_gate.is_quiesced() {
                                Self::handle_connection(stream, peer, None, tx, slot).await;
                            }
                        }
                    }
                })
                .await;
            });
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }

    /// Read one framed message from any byte stream (plain TCP or TLS).
    ///
    /// The connection is pending, and can be evicted, until the whole frame
    /// is in; then it waits for an admission slot.
    async fn handle_connection<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        mut stream: S,
        peer: SocketAddr,
        peer_node_id: Option<NodeId>,
        tx: mpsc::Sender<InboundReport>,
        slot: PendingSlot,
    ) {
        use tokio::io::AsyncWriteExt;

        let payload = tokio::select! {
            biased;
            _ = slot.evict.cancelled() => return,
            payload = read_frame(&mut stream) => payload,
        };
        let Some(payload) = payload else { return };
        let Some(_admission) = slot.into_admission().await else {
            return;
        };

        // Waiting for room is bounded by the caller's accept deadline; a
        // report that never fits is never acknowledged, so its sender knows.
        if let Ok(msg) = decode_report(&payload)
            && tx.send((peer, peer_node_id, msg)).await.is_ok()
        {
            // Admission is volatile, not a durable processing receipt. A lost
            // ACK can cause a retry; snapshots and owned rollups are idempotent.
            let _ = stream.write_all(&[ADMITTED]).await;
            let _ = stream.flush().await;
        }
    }

    /// Send a length-prefixed postcard message over a new connection.
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
/// Read a 4-byte big-endian length, then that many bytes. `None` when the
/// peer closes early or claims more than `MAX_REPORT_SIZE`, which is checked
/// before anything is allocated.
async fn read_frame<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_REPORT_SIZE {
        return None;
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.ok()?;
    Some(payload)
}

/// The accept loop's record of one pending connection.
struct PendingConnection {
    source: IpAddr,
    since: tokio::time::Instant,
    /// Cancelled to close the connection while it is still pending.
    evict: tokio_util::sync::CancellationToken,
    /// Its sender lives in the connection's `PendingSlot` and is dropped,
    /// never sent on, once the connection stops being pending. A oneshot
    /// used only for its drop is a cheap one-way "done" flag.
    still_pending: tokio::sync::oneshot::Receiver<()>,
}

/// What a connection task holds while it is pending.
struct PendingSlot {
    evict: tokio_util::sync::CancellationToken,
    still_pending: tokio::sync::oneshot::Sender<()>,
    pending_slot: tokio::sync::OwnedSemaphorePermit,
    report_slots: Arc<tokio::sync::Semaphore>,
}

impl PendingConnection {
    /// Record a newly accepted connection, and hand its task the other half.
    fn start(
        source: IpAddr,
        pending_slot: tokio::sync::OwnedSemaphorePermit,
        report_slots: Arc<tokio::sync::Semaphore>,
    ) -> (Self, PendingSlot) {
        let evict = tokio_util::sync::CancellationToken::new();
        let (still_pending_tx, still_pending_rx) = tokio::sync::oneshot::channel();
        (
            Self {
                source,
                since: tokio::time::Instant::now(),
                evict: evict.clone(),
                still_pending: still_pending_rx,
            },
            PendingSlot {
                evict,
                still_pending: still_pending_tx,
                pending_slot,
                report_slots,
            },
        )
    }

    /// Whether the connection is still handshaking or reading its frame.
    fn is_pending(&mut self) -> bool {
        matches!(
            self.still_pending.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        )
    }
}

impl PendingSlot {
    /// Leave the pending budget for the admission one. The pending slot is
    /// given back only once the admission slot is held, so the number of
    /// tasks stays bounded while they wait. `self` is taken by value: the
    /// slot can't be used again afterwards, and the compiler checks that.
    async fn into_admission(self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let PendingSlot {
            still_pending,
            pending_slot,
            report_slots,
            ..
        } = self;
        drop(still_pending);
        let admission = report_slots.acquire_owned().await.ok()?;
        drop(pending_slot);
        Some(admission)
    }
}

/// Whether `source` already holds its share of the pending budget.
fn source_is_full(source: IpAddr, pending: &VecDeque<PendingConnection>) -> bool {
    let source = source.to_canonical();
    !source.is_loopback()
        && pending
            .iter()
            .filter(|connection| connection.source.to_canonical() == source)
            .count()
            >= MAX_PENDING_PER_SOURCE
}

/// Sleep until `due`; with nothing due, never wake.
async fn sleep_until_due(due: Option<tokio::time::Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(due).await,
        None => std::future::pending().await,
    }
}

/// Counts a connection task for as long as it lives, dropped or aborted.
struct TaskCount(Arc<AtomicUsize>);

impl TaskCount {
    fn start(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(count))
    }
}

impl Drop for TaskCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

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
    let body_size = postcard::experimental::serialized_size(message)
        .map_err(|error| ReportingError::Serialisation(error.to_string()))?;
    let size = body_size.saturating_add(report_header().len());
    if size > MAX_REPORT_SIZE {
        return Err(ReportingError::ReportTooLarge {
            size,
            max: MAX_REPORT_SIZE,
        });
    }
    let mut bytes = Vec::with_capacity(size);
    bytes.extend(report_header());
    postcard::to_extend(message, bytes)
        .map_err(|error| ReportingError::Serialisation(error.to_string()))
}

fn decode_report(payload: &[u8]) -> Result<ReportingMessage, ReportingError> {
    // The receiver never reads a longer frame, but the decoder doesn't lean
    // on that: no sender may produce one, so it isn't decoded.
    if payload.len() > MAX_REPORT_SIZE {
        return Err(ReportingError::ReportTooLarge {
            size: payload.len(),
            max: MAX_REPORT_SIZE,
        });
    }
    let body = payload
        .strip_prefix(&report_header())
        .ok_or_else(|| ReportingError::Deserialisation("incompatible reporting formats".into()))?;
    // postcard reads only the slice it's given and checks each length prefix
    // against the bytes left, so a hostile prefix is a decode error rather
    // than an allocation. `take_from_bytes` returns what the message didn't
    // use, and a frame holds exactly one message, so anything left over is
    // refused.
    let (message, rest) = postcard::take_from_bytes::<ReportingMessage>(body)
        .map_err(|error| ReportingError::Deserialisation(error.to_string()))?;
    if !rest.is_empty() {
        return Err(ReportingError::Deserialisation(format!(
            "{} trailing bytes after the report",
            rest.len()
        )));
    }
    check_event_admission(&message).map_err(ReportingError::Deserialisation)?;
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

    /// A pending slot for driving `handle_connection` without a listener.
    fn slot() -> PendingSlot {
        let pending = Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap();
        let reports = Arc::new(tokio::sync::Semaphore::new(1));
        PendingConnection::start(addr(0).ip(), pending, reports).1
    }

    fn pending_from(source: IpAddr) -> PendingConnection {
        let permit = Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap();
        let (connection, slot) =
            PendingConnection::start(source, permit, Arc::new(tokio::sync::Semaphore::new(1)));
        // Keep the task's half alive, so the connection stays pending.
        std::mem::forget(slot);
        connection
    }

    /// A fixed timestamp, so a message's encoded size depends only on its
    /// name: postcard writes the seconds and nanoseconds as varints.
    fn sample_msg(name: &str) -> ReportingMessage {
        ReportingMessage::Report(StateReport {
            has_buildah: false,
            node_id: NodeId::new(name),
            timestamp: SystemTime::UNIX_EPOCH + Duration::new(1_800_000_000, 500_000_000),
            running_apps: vec![],
            cached_specs: vec![],
            rollouts: Vec::new(),
            resource_usage: ResourceUsage::default(),
            event_log: vec![],
            job_reservations: Default::default(),
            capacity_refusals: Vec::new(),
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
        // postcard writes the name's length as a varint, so a longer name
        // costs a byte or two more than its characters: shrink it until the
        // frame lands on the limit exactly.
        let frame_size = |name: &str| encode_report(&sample_msg(name)).map(|frame| frame.len());
        let mut name = "w".repeat(MAX_REPORT_SIZE);
        while frame_size(&name).is_err() {
            name.pop();
        }
        assert_eq!(frame_size(&name).unwrap(), MAX_REPORT_SIZE);
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
    async fn connections_past_the_limit_wait_for_a_slot_instead_of_being_refused() {
        use tokio::io::AsyncWriteExt;
        let shutdown = tokio_util::sync::CancellationToken::new();
        let receiver = TcpReportingTransport::bind(addr(0), shutdown.clone())
            .await
            .unwrap();
        // Slow peers hold every pending slot: half a length prefix, then
        // nothing yet. They are younger than PENDING_EVICTION_AGE, so none
        // is closed to make room.
        let mut stalled = Vec::new();
        for _ in 0..MAX_PENDING_CONNECTIONS {
            let mut client = tokio::net::TcpStream::connect(receiver.local_addr())
                .await
                .unwrap();
            client.write_all(&[0, 0]).await.unwrap();
            stalled.push(client);
        }
        // One more report waits rather than spawning a task or being refused.
        let target = receiver.local_addr();
        let waiting = tokio::spawn(async move {
            TcpReportingSender::new(None, Default::default())
                .send(target, &sample_msg("waiting"))
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(300), receiver.recv())
                .await
                .is_err(),
            "a report was read past the connection limit"
        );
        // A slot frees up, and the waiting report is admitted.
        drop(stalled.pop());
        let (_, _, message) = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("the waiting report was never read")
            .unwrap();
        let ReportingMessage::Report(report) = message else {
            panic!("expected a report")
        };
        assert_eq!(report.node_id, NodeId::new("waiting"));
        waiting.await.unwrap().unwrap();
        shutdown.cancel();
    }

    #[tokio::test]
    async fn stalled_connections_do_not_stop_a_complete_report_being_admitted() {
        // #2380: sockets that connect and send nothing must not hold the
        // listener's report budget. More of them than there are slots, so a
        // budget spent at accept time can't hide behind a short queue.
        let shutdown = tokio_util::sync::CancellationToken::new();
        let receiver = TcpReportingTransport::bind(addr(0), shutdown.clone())
            .await
            .unwrap();
        let mut idle = Vec::new();
        for _ in 0..MAX_PENDING_CONNECTIONS + 32 {
            idle.push(
                tokio::net::TcpStream::connect(receiver.local_addr())
                    .await
                    .unwrap(),
            );
        }
        let sender = TcpReportingSender::new(None, Default::default());
        let sent = tokio::time::timeout(
            Duration::from_secs(5),
            sender.send(receiver.local_addr(), &sample_msg("worker")),
        )
        .await;
        assert!(
            matches!(sent, Ok(Ok(()))),
            "idle sockets kept a whole report out: {sent:?}"
        );
        let (_, _, message) = receiver.recv().await.unwrap();
        let ReportingMessage::Report(report) = message else {
            panic!("expected a report")
        };
        assert_eq!(report.node_id, NodeId::new("worker"));
        drop(idle);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn stalled_connections_cannot_create_unbounded_receiver_tasks() {
        // Idle sockets well past the pending budget, held while the listener
        // closes the oldest to make room for the newest: the tasks never
        // outnumber the pending budget, because none of them delivers a
        // frame. Reports being admitted add at most MAX_REPORT_CONNECTIONS.
        let shutdown = tokio_util::sync::CancellationToken::new();
        let receiver = TcpReportingTransport::bind(addr(0), shutdown.clone())
            .await
            .unwrap();
        let mut idle = Vec::new();
        for _ in 0..MAX_PENDING_CONNECTIONS + 40 {
            idle.push(
                tokio::net::TcpStream::connect(receiver.local_addr())
                    .await
                    .unwrap(),
            );
        }
        let mut most = 0;
        for _ in 0..25 {
            most = most.max(receiver.connection_tasks());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(most > 0, "the listener never accepted a connection");
        assert!(
            most <= MAX_PENDING_CONNECTIONS,
            "{most} connection tasks for idle sockets"
        );
        drop(idle);
        shutdown.cancel();
    }

    #[test]
    fn one_source_address_holds_at_most_its_share_of_pending_slots() {
        let remote: IpAddr = "10.0.1.7".parse().unwrap();
        let other: IpAddr = "10.0.1.8".parse().unwrap();
        let mut pending: VecDeque<PendingConnection> = VecDeque::new();
        for _ in 0..MAX_PENDING_PER_SOURCE - 1 {
            pending.push_back(pending_from(remote));
        }
        assert!(!source_is_full(remote, &pending));
        pending.push_back(pending_from(remote));
        assert!(source_is_full(remote, &pending));
        // The same address written as IPv4-mapped IPv6 is the same source.
        let mapped: IpAddr = "::ffff:10.0.1.7".parse().unwrap();
        assert!(source_is_full(mapped, &pending));
        assert!(!source_is_full(other, &pending));
        // Loopback is exempt: the hollow-node harness puts every node there.
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        for _ in 0..MAX_PENDING_PER_SOURCE * 2 {
            pending.push_back(pending_from(loopback));
        }
        assert!(!source_is_full(loopback, &pending));
    }

    #[tokio::test]
    async fn a_connection_stops_being_pending_once_its_frame_is_in() {
        use tokio::io::AsyncWriteExt;
        let frame = encode_report(&sample_msg("worker")).unwrap();
        let (mut client, server) = tokio::io::duplex(frame.len() + 8);
        let permit = Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap();
        let (mut connection, slot) = PendingConnection::start(
            addr(1).ip(),
            permit,
            Arc::new(tokio::sync::Semaphore::new(1)),
        );
        let (tx, mut rx) = mpsc::channel(1);
        let handle = tokio::spawn(TcpReportingTransport::handle_connection(
            server,
            addr(1),
            None,
            tx,
            slot,
        ));
        assert!(connection.is_pending());
        client
            .write_all(&(frame.len() as u32).to_be_bytes())
            .await
            .unwrap();
        client.write_all(&frame).await.unwrap();
        assert!(rx.recv().await.is_some());
        assert!(!connection.is_pending());
        // Evicting it now does nothing: it has proved itself.
        connection.evict.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn full_receiver_holds_the_sender_until_there_is_room() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let receiver = TcpReportingTransport::bind(addr(0), shutdown.clone())
            .await
            .unwrap();
        let sender = TcpReportingSender::new(None, Default::default());
        for _ in 0..MAX_QUEUED_REPORTS {
            sender
                .send(receiver.local_addr(), &sample_msg("worker"))
                .await
                .unwrap();
        }
        // The queue is full: the next report isn't acknowledged yet...
        let target = receiver.local_addr();
        let mut overflow = tokio::spawn(async move {
            TcpReportingSender::new(None, Default::default())
                .send(target, &sample_msg("overflow"))
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut overflow)
                .await
                .is_err(),
            "a report was acknowledged while the queue was full"
        );
        // ...and is admitted once the aggregator takes one.
        assert!(receiver.recv().await.is_some());
        tokio::time::timeout(Duration::from_secs(5), overflow)
            .await
            .expect("the held report was never admitted")
            .unwrap()
            .unwrap();
        shutdown.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn a_report_that_never_finds_room_is_not_acknowledged() {
        // The receiving half of the accept path: a queue nobody drains.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send((addr(1), None, sample_msg("filler"))).unwrap();
        let frame = encode_report(&sample_msg("worker")).unwrap();
        let (mut client, server) = tokio::io::duplex(frame.len() + 8);
        client
            .write_all(&(frame.len() as u32).to_be_bytes())
            .await
            .unwrap();
        client.write_all(&frame).await.unwrap();
        let _ = tokio::time::timeout(
            REPORT_ACCEPT_DEADLINE,
            TcpReportingTransport::handle_connection(server, addr(2), None, tx, slot()),
        )
        .await;
        let mut ack = [0];
        assert!(
            !matches!(client.read(&mut ack).await, Ok(1)),
            "a report that was never queued was acknowledged"
        );
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
        // The Report variant and `has_buildah`, then a node-id length that
        // claims 2^64 - 1 bytes: a decode error, not an allocation.
        let unnamed = encode_report(&sample_msg("")).unwrap();
        let mut malicious = unnamed[..report_header().len() + 2].to_vec();
        malicious.extend([0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]);
        assert!(decode_report(&malicious).is_err());
    }

    #[test]
    fn every_reporting_message_round_trips_through_a_frame() {
        use crate::mayo::rollup::{NodeRollup, RollupAggregate, RollupEntry};
        let rollup = ReportingMessage::MetricsRollup(NodeRollup {
            node_id: NodeId::new("worker"),
            timestamp: 60,
            entries: vec![RollupEntry {
                metric_name: "cpu".into(),
                labels: [("app".to_string(), "web".to_string())].into(),
                aggregate: RollupAggregate {
                    min: 0.5,
                    max: 9.5,
                    sum: 20.0,
                    count: 4,
                },
            }],
        });
        for message in [sample_msg("worker"), rollup] {
            let decoded = decode_report(&encode_report(&message).unwrap()).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                serde_json::to_value(&message).unwrap()
            );
        }
    }

    #[test]
    fn a_frame_with_trailing_bytes_is_refused() {
        let mut frame = encode_report(&sample_msg("worker")).unwrap();
        assert!(decode_report(&frame).is_ok());
        frame.push(0);
        assert!(decode_report(&frame).is_err());
    }

    #[test]
    fn a_frame_over_the_size_limit_is_refused_before_decoding() {
        let mut frame = encode_report(&sample_msg("worker")).unwrap();
        frame.resize(MAX_REPORT_SIZE + 1, 0);
        assert!(matches!(
            decode_report(&frame),
            Err(ReportingError::ReportTooLarge { .. })
        ));
    }

    /// A bincode frame from the generation before postcard: its header names
    /// protocol 52 and state 69, so it's refused before the body is read.
    #[test]
    fn a_bincode_frame_from_the_previous_generation_is_refused() {
        let mut old = b"RBRP".to_vec();
        old.extend(52u32.to_be_bytes());
        old.extend(69u32.to_be_bytes());
        old.extend(0u32.to_le_bytes()); // Report
        old.push(0); // has_buildah
        old.extend(1u64.to_le_bytes()); // node-id length
        old.push(b'w');
        assert!(decode_report(&old).is_err());
    }

    #[tokio::test]
    async fn unversioned_reporting_frame_never_reaches_the_inbox() {
        let payload = postcard::to_stdvec(&sample_msg("unversioned")).unwrap();
        let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
        frame.extend(payload);
        let (tx, mut rx) = mpsc::channel(1);
        use tokio::io::AsyncWriteExt;
        let (mut client, server) = tokio::io::duplex(frame.len());
        client.write_all(&frame).await.unwrap();
        TcpReportingTransport::handle_connection(server, addr(1234), None, tx, slot()).await;
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
            TcpReportingTransport::handle_connection(server, addr(1), None, tx, slot()),
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

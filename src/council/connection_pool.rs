//! Reused Raft transport connections (#928).
//!
//! Every Raft RPC used to open a fresh TCP connection and run a full mTLS
//! handshake. A leader confirming its leadership gives each heartbeat only
//! `heartbeat_interval` (150 ms) to come back, so the handshake alone could
//! push a heartbeat past its window on a debug build or a loaded host.
//!
//! [`RaftConnectionPool`] keeps a few established connections per peer. An
//! RPC takes one out (or dials a new one), uses it for one request and one
//! response, and puts it back. Because a connection is *owned* by the RPC
//! while it is in use, an RPC that is cancelled half-way (openraft drops the
//! future at its own deadline) drops the connection with it: a connection
//! with a request in flight can never be handed to the next RPC.
//!
//! A pooled connection is retired rather than reused when:
//!
//! - it is older than [`CONNECTION_MAX_AGE`] or has sat idle longer than
//!   [`CONNECTION_IDLE_TIMEOUT`];
//! - this node's identity changed since it was dialled (a renewed
//!   certificate or a new trust set), so the next handshake presents the
//!   new certificate and re-verifies the peer against the new trust;
//! - the certificate chain the peer presented is now on the CRL.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::CertificateDer;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::sesame::identity_store::NodeIdentity;

/// How many idle connections the pool keeps per peer.
///
/// openraft sends to one peer from its replication stream and, during an
/// election, from vote tasks, so a peer rarely needs more than two at once.
/// A burst beyond this still works; the extra connections are closed after
/// use instead of being kept.
pub(crate) const MAX_IDLE_PER_PEER: usize = 4;

/// The oldest a pooled connection may be before it is retired.
///
/// Shorter than the server's [`SERVER_CONNECTION_MAX_AGE`], so the client
/// retires a connection before the server closes it under the client.
pub(crate) const CONNECTION_MAX_AGE: Duration = Duration::from_secs(30);

/// How long a pooled connection may sit unused before it is retired.
///
/// Shorter than the server's [`SERVER_IDLE_TIMEOUT`], for the same reason.
pub(crate) const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the server keeps a connection open with no request arriving.
pub(crate) const SERVER_IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// The oldest a connection may be before the server closes it after its
/// current response. This bounds how long a connection authenticated with an
/// old certificate lasts, whatever the client does.
pub(crate) const SERVER_CONNECTION_MAX_AGE: Duration = Duration::from_secs(60);

// The client must retire a connection before the server closes it, or a
// reused connection would often be dead on arrival. Checked at compile time:
// a `const` item is evaluated by the compiler, and a failed `assert!` in it
// is a build error.
const _: () = assert!(CONNECTION_MAX_AGE.as_secs() < SERVER_CONNECTION_MAX_AGE.as_secs());
const _: () = assert!(CONNECTION_IDLE_TIMEOUT.as_secs() < SERVER_IDLE_TIMEOUT.as_secs());

/// A byte stream a Raft connection runs over: plain TCP or mTLS.
///
/// This trait has no methods of its own. It names the combination of traits
/// a connection needs so that `Box<dyn RaftIo>` can hold either kind of
/// stream. The blanket `impl` below gives it to every type that already has
/// them, so `TcpStream` and `TlsStream<TcpStream>` both qualify.
pub(crate) trait RaftIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> RaftIo for T {}

/// One established connection to a Raft peer.
pub(crate) struct RaftConnection {
    /// The stream, after any TLS handshake.
    pub(crate) stream: Box<dyn RaftIo>,
    established: Instant,
    last_used: Instant,
    /// The identity this node held when it dialled. `None` without mTLS.
    presented: Option<Arc<NodeIdentity>>,
    /// The chain the peer presented at the handshake. Empty without mTLS.
    peer_chain: Vec<CertificateDer<'static>>,
}

impl RaftConnection {
    /// Wrap a freshly dialled stream.
    pub(crate) fn new(
        stream: Box<dyn RaftIo>,
        presented: Option<Arc<NodeIdentity>>,
        peer_chain: Vec<CertificateDer<'static>>,
    ) -> Self {
        let now = Instant::now();
        Self {
            stream,
            established: now,
            last_used: now,
            presented,
            peer_chain,
        }
    }
}

/// What a pooled connection must still match to be reused.
pub(crate) struct ReuseCheck<'a> {
    /// This node's identity now, if it has a live one.
    pub(crate) current_identity: Option<Arc<NodeIdentity>>,
    /// The live CRL, if the transport has one.
    pub(crate) crl: Option<&'a crate::sesame::mtls::CrlHandle>,
}

impl ReuseCheck<'_> {
    fn allows(&self, connection: &RaftConnection, now: Instant) -> bool {
        if now.duration_since(connection.established) >= CONNECTION_MAX_AGE
            || now.duration_since(connection.last_used) >= CONNECTION_IDLE_TIMEOUT
        {
            return false;
        }
        // `Arc::ptr_eq` compares addresses, not contents. A rotation or a new
        // trust set publishes a new `Arc`, so any change at all retires the
        // connection.
        let same_identity = match (&self.current_identity, &connection.presented) {
            (Some(current), Some(presented)) => Arc::ptr_eq(current, presented),
            (None, None) => true,
            _ => false,
        };
        let peer_admitted = match self.crl {
            Some(crl) => crl.admits(&connection.peer_chain),
            None => true,
        };
        same_identity && peer_admitted
    }
}

/// Which peer a connection reaches: its address and the node name its
/// certificate was checked against.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) struct PeerKey {
    pub(crate) address: SocketAddr,
    pub(crate) node_name: String,
}

/// Idle Raft connections, shared by every client a factory hands out.
///
/// Cloning shares the same pool (the map sits behind an `Arc`). The lock is
/// held only to push or pop, never across network I/O.
#[derive(Clone, Default)]
pub(crate) struct RaftConnectionPool {
    idle: Arc<Mutex<HashMap<PeerKey, Vec<RaftConnection>>>>,
}

impl RaftConnectionPool {
    /// Take the most recently used connection to `peer` that `check` still
    /// allows, closing any it doesn't. `None` means dial a new one.
    pub(crate) async fn take(
        &self,
        peer: &PeerKey,
        check: &ReuseCheck<'_>,
    ) -> Option<RaftConnection> {
        let now = Instant::now();
        let mut idle = self.idle.lock().await;
        let connections = idle.get_mut(peer)?;
        // The most recently used connection sits at the end. Retire the
        // stale ones on the way; dropping a connection closes it.
        while let Some(connection) = connections.pop() {
            if check.allows(&connection, now) {
                return Some(connection);
            }
        }
        None
    }

    /// Return a connection after a completed request and response.
    ///
    /// Only call this with a connection that has no request in flight. Past
    /// [`MAX_IDLE_PER_PEER`] the connection is closed instead.
    pub(crate) async fn put(&self, peer: &PeerKey, mut connection: RaftConnection) {
        connection.last_used = Instant::now();
        let mut idle = self.idle.lock().await;
        let connections = idle.entry(peer.clone()).or_default();
        if connections.len() < MAX_IDLE_PER_PEER {
            connections.push(connection);
        }
    }

    /// How many idle connections the pool holds for `peer`.
    #[cfg(test)]
    pub(crate) async fn idle_count(&self, peer: &PeerKey) -> usize {
        self.idle.lock().await.get(peer).map_or(0, Vec::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> PeerKey {
        PeerKey {
            address: "127.0.0.1:9".parse().unwrap(),
            node_name: "n2".into(),
        }
    }

    fn connection() -> RaftConnection {
        let (stream, _) = tokio::io::duplex(64);
        RaftConnection::new(Box::new(stream), None, Vec::new())
    }

    fn plain() -> ReuseCheck<'static> {
        ReuseCheck {
            current_identity: None,
            crl: None,
        }
    }

    #[tokio::test]
    async fn a_returned_connection_is_taken_again() {
        let pool = RaftConnectionPool::default();
        assert!(pool.take(&peer(), &plain()).await.is_none());
        pool.put(&peer(), connection()).await;
        assert!(pool.take(&peer(), &plain()).await.is_some());
        assert!(pool.take(&peer(), &plain()).await.is_none());
    }

    #[tokio::test]
    async fn the_pool_keeps_at_most_max_idle_per_peer() {
        let pool = RaftConnectionPool::default();
        for _ in 0..MAX_IDLE_PER_PEER + 3 {
            pool.put(&peer(), connection()).await;
        }
        assert_eq!(pool.idle_count(&peer()).await, MAX_IDLE_PER_PEER);
    }

    #[tokio::test]
    async fn connections_are_kept_per_peer() {
        let pool = RaftConnectionPool::default();
        pool.put(&peer(), connection()).await;
        let other = PeerKey {
            node_name: "n3".into(),
            ..peer()
        };
        assert!(pool.take(&other, &plain()).await.is_none());
        assert!(pool.take(&peer(), &plain()).await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_connection_is_retired() {
        let pool = RaftConnectionPool::default();
        pool.put(&peer(), connection()).await;
        tokio::time::advance(CONNECTION_IDLE_TIMEOUT).await;
        assert!(pool.take(&peer(), &plain()).await.is_none());
        assert_eq!(pool.idle_count(&peer()).await, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_old_connection_is_retired_even_when_busy() {
        let pool = RaftConnectionPool::default();
        let mut busy = connection();
        // Keep it in use until it passes its maximum age.
        let mut elapsed = Duration::ZERO;
        while elapsed < CONNECTION_MAX_AGE {
            pool.put(&peer(), busy).await;
            tokio::time::advance(Duration::from_secs(1)).await;
            elapsed += Duration::from_secs(1);
            match pool.take(&peer(), &plain()).await {
                Some(connection) => busy = connection,
                None => return,
            }
        }
        panic!("a connection past its maximum age must not be reused");
    }
}

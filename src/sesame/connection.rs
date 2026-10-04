//! Connection deadlines for Bun's HTTP listeners (API, registry, ingress):
//! handshake, idle, stall and lifetime limits, plus the shared accept loops.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Maximum lifetime of an authenticated API or ingress TLS connection.
pub const MAX_TLS_CONNECTION_LIFETIME: Duration = Duration::from_secs(3600);

/// Time reserved for HTTP requests to drain before the lifetime limit.
/// A connection that lives for less than twice this drains for half its life.
pub const TLS_CONNECTION_DRAIN_GRACE: Duration = Duration::from_secs(30);

/// How long a TLS connection whose client presented `peer_certificate` may
/// stay open, counted from `now`.
///
/// TLS checks the client's certificate once, at the handshake, and a pooled
/// client connection that never goes idle can outlive that certificate by
/// hours. Renewal installs a new leaf, but only new handshakes present it
/// (#509). So the lifetime is [`MAX_TLS_CONNECTION_LIFETIME`], cut short to
/// the instant the client's leaf expires. A client without a certificate gets
/// the full lifetime; a certificate this function can't read gets none.
pub fn tls_connection_lifetime(peer_certificate: Option<&[u8]>, now: SystemTime) -> Duration {
    let Some(certificate_der) = peer_certificate else {
        return MAX_TLS_CONNECTION_LIFETIME;
    };
    let Ok((_, leaf)) = x509_parser::parse_x509_certificate(certificate_der) else {
        return Duration::ZERO;
    };
    let expires = u64::try_from(leaf.validity().not_after.timestamp())
        .ok()
        .and_then(|seconds| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds)));
    let Some(expires) = expires else {
        return Duration::ZERO;
    };
    expires
        .duration_since(now)
        .unwrap_or(Duration::ZERO)
        .min(MAX_TLS_CONNECTION_LIFETIME)
}

/// An I/O stream whose lifetime limit follows it through HTTP upgrades.
///
/// Limiting only the HTTP serving future misses WebSockets: Hyper hands their
/// stream to a different task after the upgrade response.
pub struct LifetimeLimitedIo<S> {
    inner: S,
    expires: Pin<Box<tokio::time::Sleep>>,
}

impl<S> LifetimeLimitedIo<S> {
    /// Keep the stream usable only for the supplied lifetime from now.
    pub fn new(inner: S, lifetime: Duration) -> Self {
        Self {
            inner,
            expires: Box::pin(tokio::time::sleep(lifetime)),
        }
    }

    fn check_deadline(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if self.expires.as_mut().poll(cx).is_ready() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "tls connection lifetime exceeded",
            ));
        }
        Ok(())
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for LifetimeLimitedIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.check_deadline(cx)?;
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for LifetimeLimitedIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.check_deadline(cx)?;
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_deadline(cx)?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_deadline(cx)?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Request extension proving the request arrived on a TLS connection.
///
/// [`serve_router_over_tls`] attaches it to every request it serves; a plain
/// listener never does. Handlers that must refuse secrets sent in the clear
/// (HTTP Basic credentials at the registry, for one) check for this marker
/// rather than trusting configuration that says TLS *should* be on: a node
/// whose TLS setup failed falls back to plaintext, and the marker follows what
/// actually happened on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlsTransport;

/// Request extension that resolves once the connection carrying the request
/// has finished: every answer on it written and flushed, and the socket
/// closed.
///
/// [`serve_http_connection`] attaches it to every request. A handler whose
/// answer must reach the client before the process execs (a self-upgrade)
/// answers with `Connection: close` and hands this to whoever execs, so
/// the exec waits for the answer instead of guessing how long it takes.
#[derive(Debug, Clone)]
pub struct ConnectionClosed(tokio_util::sync::CancellationToken);

impl ConnectionClosed {
    /// One that resolves when `token` is cancelled, for callers (and tests)
    /// that stand in for a connection.
    pub fn from_token(token: tokio_util::sync::CancellationToken) -> Self {
        Self(token)
    }

    /// Wait until the connection has closed.
    pub async fn wait(&self) {
        self.0.cancelled().await
    }
}

/// Deadlines every HTTP listener in Bun applies to the connections it accepts.
///
/// Hyper and axum switch none of these on by default: an idle keep-alive
/// connection, or one whose peer silently went away, lives until the process
/// exits. Lima's port forwarder produces exactly that. It never closes the
/// guest side of a forwarded connection when the host client disconnects, so
/// every poll through a forwarded port used to leave one more socket (and its
/// TLS and HTTP buffers) behind in Bun.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionTimeouts {
    /// How long a TLS handshake may take before the socket is dropped.
    pub tls_handshake: Duration,
    /// How long a connection may wait for the first byte of a request, and
    /// then for the complete HTTP/1 request head. Hyper restarts this clock
    /// whenever an HTTP/1 connection goes idle between requests, so it is also
    /// the keep-alive idle timeout. It never runs while a response streams.
    pub request_head: Duration,
    /// How often an HTTP/2 connection pings its peer.
    pub http2_ping_interval: Duration,
    /// How long an HTTP/2 connection waits for a ping acknowledgement before
    /// closing.
    pub http2_ping_timeout: Duration,
    /// How long a write may make no progress at all before the connection is
    /// treated as dead. Covers streaming responses and upgraded WebSockets
    /// whose reader vanished without closing the socket.
    pub write_stall: Duration,
    /// Idle time before the kernel starts sending TCP keepalive probes.
    pub tcp_keepalive_idle: Duration,
    /// Gap between TCP keepalive probes once they start.
    pub tcp_keepalive_interval: Duration,
}

impl ConnectionTimeouts {
    /// The deadlines Bun's API, registry and ingress listeners use.
    ///
    /// `request_head` is 75 seconds (nginx's keep-alive default): shorter than
    /// reqwest's 90-second pool idle time, and a pooled client that sees the
    /// server's FIN drops the connection instead of reusing it. The write-stall
    /// limit is generous because someone piping `relish logs -f` into a pager
    /// legitimately stops reading for a while.
    pub const PRODUCTION: Self = Self {
        tls_handshake: Duration::from_secs(10),
        request_head: Duration::from_secs(75),
        http2_ping_interval: Duration::from_secs(30),
        http2_ping_timeout: Duration::from_secs(20),
        write_stall: Duration::from_secs(300),
        tcp_keepalive_idle: Duration::from_secs(60),
        tcp_keepalive_interval: Duration::from_secs(15),
    };
}

/// Switch on TCP keepalive for an accepted socket.
///
/// Keepalive only notices peers that vanished off the network (a crashed host,
/// a dropped NAT mapping). A live proxy holding a dead session open still
/// answers the probes, which is why the HTTP-level deadlines exist as well.
pub fn configure_accepted_socket(
    tcp: &tokio::net::TcpStream,
    timeouts: &ConnectionTimeouts,
) -> io::Result<()> {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(timeouts.tcp_keepalive_idle)
        .with_interval(timeouts.tcp_keepalive_interval);
    socket2::SockRef::from(tcp).set_tcp_keepalive(&keepalive)
}

/// An I/O stream that fails when its peer stops cooperating.
///
/// It enforces two deadlines Hyper has no setting for:
///
/// - **First byte.** Hyper's auto-detecting server reads the connection
///   preface before it knows whether to speak HTTP/1 or HTTP/2, and that read
///   has no timeout. A client that connects and sends nothing would sit there
///   forever.
/// - **Write stall.** A write that stays pending (the peer's receive window is
///   full and nobody drains it) fails once it has made no progress for the
///   stall limit. Every completed write resets the clock, so a slow but live
///   reader is fine.
pub struct StallGuardedIo<S> {
    inner: S,
    first_byte: Option<Pin<Box<tokio::time::Sleep>>>,
    write_stall_limit: Duration,
    write_stall: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S> StallGuardedIo<S> {
    /// Guard `inner`, expecting its first byte within `first_byte` and never
    /// letting a write sit without progress for longer than `write_stall`.
    pub fn new(inner: S, first_byte: Duration, write_stall: Duration) -> Self {
        Self {
            inner,
            first_byte: Some(Box::pin(tokio::time::sleep(first_byte))),
            write_stall_limit: write_stall,
            write_stall: None,
        }
    }

    /// Track a pending or finished write, failing once it stalled too long.
    fn track_write<T>(
        &mut self,
        cx: &mut Context<'_>,
        poll: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        if poll.is_ready() {
            self.write_stall = None;
            return poll;
        }
        let limit = self.write_stall_limit;
        let stall = self
            .write_stall
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
        if stall.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "peer stopped reading",
            )));
        }
        Poll::Pending
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for StallGuardedIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        let Some(deadline) = self.first_byte.as_mut() else {
            return poll;
        };
        match poll {
            Poll::Ready(Ok(())) if buf.filled().len() > before => self.first_byte = None,
            Poll::Pending if deadline.as_mut().poll(cx).is_ready() => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no request arrived on the connection",
                )));
            }
            _ => {}
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for StallGuardedIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.track_write(cx, poll)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_flush(cx);
        self.track_write(cx, poll)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.track_write(cx, poll)
    }
}

/// Build the Hyper connection builder with every deadline switched on.
///
/// Hyper's timers only run when the builder has a timer to drive them; without
/// one, `header_read_timeout` and HTTP/2 keep-alive are silently ignored. That
/// is the state `axum::serve` leaves them in.
fn http_builder(
    timeouts: &ConnectionTimeouts,
) -> hyper_util::server::conn::auto::Builder<hyper_util::rt::TokioExecutor> {
    let mut builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    builder
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(timeouts.request_head);
    builder
        .http2()
        .timer(hyper_util::rt::TokioTimer::new())
        .keep_alive_interval(timeouts.http2_ping_interval)
        .keep_alive_timeout(timeouts.http2_ping_timeout);
    builder
}

/// Serve one accepted connection through `router` until it finishes.
///
/// When `shutdown` fires, or `lifetime` (if any) is about to run out, the
/// connection is asked to finish gracefully: an idle connection closes at
/// once, a busy one gets [`TLS_CONNECTION_DRAIN_GRACE`] to complete its
/// in-flight requests.
pub async fn serve_http_connection<I>(
    io: I,
    router: axum::Router,
    timeouts: ConnectionTimeouts,
    lifetime: Option<Duration>,
    shutdown: tokio_util::sync::CancellationToken,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let closed = tokio_util::sync::CancellationToken::new();
    // Cancelled however this function returns, after the connection is done.
    let _closed_on_return = closed.clone().drop_guard();
    let router = router.layer(axum::Extension(ConnectionClosed(closed)));
    let io = StallGuardedIo::new(io, timeouts.request_head, timeouts.write_stall);
    let hyper_service = hyper_util::service::TowerToHyperService::new(router);
    let builder = http_builder(&timeouts);
    let connection =
        builder.serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(io), hyper_service);
    tokio::pin!(connection);
    let drain_at = async {
        match lifetime {
            Some(lifetime) => {
                // A short lifetime (a client leaf about to expire) still gets
                // half of it to serve requests before draining starts.
                let grace = TLS_CONNECTION_DRAIN_GRACE.min(lifetime / 2);
                tokio::time::sleep(lifetime - grace).await
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = &mut connection => return,
        _ = shutdown.cancelled() => {},
        _ = drain_at => {},
    }
    connection.as_mut().graceful_shutdown();
    let _ = tokio::time::timeout(TLS_CONNECTION_DRAIN_GRACE, connection).await;
}

/// Serve an axum router over plain TCP with the given deadlines.
///
/// This replaces `axum::serve`, which has no way to set Hyper's timers. Every
/// request carries the client's address as
/// `axum::extract::ConnectInfo<SocketAddr>`. Runs until `shutdown` is
/// cancelled.
pub async fn serve_router_plain(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    timeouts: ConnectionTimeouts,
    shutdown: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            accepted = listener.accept() => {
                // Transient accept errors (EMFILE, resets) must not stop the
                // listener; skip and keep accepting.
                let Ok((tcp, remote)) = accepted else { continue };
                let _ = configure_accepted_socket(&tcp, &timeouts);
                let router = router
                    .clone()
                    .layer(axum::Extension(axum::extract::ConnectInfo(remote)));
                tokio::spawn(serve_http_connection(
                    tcp,
                    router,
                    timeouts,
                    None,
                    shutdown.clone(),
                ));
            }
        }
    }
}

/// Serve an axum router over TLS, handshaking each connection in its own task
/// (a slow handshaker never blocks the accept loop). Runs until `shutdown` is
/// cancelled.
///
/// Every request carries [`TlsTransport`], the client's address as
/// `axum::extract::ConnectInfo<SocketAddr>`, and the client's leaf certificate
/// as [`super::renewal::TlsPeerCertificate`] when it presented one.
/// Connections retire after [`MAX_TLS_CONNECTION_LIFETIME`], or when the
/// client's leaf certificate expires if that comes first
/// ([`tls_connection_lifetime`]).
pub async fn serve_router_over_tls(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    router: axum::Router,
    timeouts: ConnectionTimeouts,
    shutdown: tokio_util::sync::CancellationToken,
) {
    serve_router_over_tls_with_clock(
        listener,
        acceptor,
        router,
        timeouts,
        shutdown,
        SystemTime::now,
    )
    .await;
}

// Inject only the lifetime calculation clock. TLS certificate verification
// continues to use its real clock, including in the renewal regression.
async fn serve_router_over_tls_with_clock<F>(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    router: axum::Router,
    timeouts: ConnectionTimeouts,
    shutdown: tokio_util::sync::CancellationToken,
    lifetime_clock: F,
) where
    F: Fn() -> SystemTime + Clone + Send + Sync + 'static,
{
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            accepted = listener.accept() => {
                let Ok((tcp, remote)) = accepted else { continue };
                let _ = configure_accepted_socket(&tcp, &timeouts);
                tokio::spawn(serve_tls_connection(
                    tcp,
                    remote,
                    acceptor.clone(),
                    router.clone(),
                    timeouts,
                    shutdown.clone(),
                    lifetime_clock.clone(),
                ));
            }
        }
    }
}

async fn serve_tls_connection<F>(
    tcp: tokio::net::TcpStream,
    remote: std::net::SocketAddr,
    acceptor: tokio_rustls::TlsAcceptor,
    router: axum::Router,
    timeouts: ConnectionTimeouts,
    shutdown: tokio_util::sync::CancellationToken,
    lifetime_clock: F,
) where
    F: Fn() -> SystemTime + Send + Sync + 'static,
{
    let Ok(Ok(tls)) = tokio::time::timeout(timeouts.tls_handshake, acceptor.accept(tcp)).await
    else {
        return;
    };
    let router = router
        .layer(axum::Extension(TlsTransport))
        .layer(axum::Extension(axum::extract::ConnectInfo(remote)));
    let peer_certificate = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .cloned();
    let lifetime = tls_connection_lifetime(
        peer_certificate
            .as_ref()
            .map(|certificate| certificate.as_ref()),
        lifetime_clock(),
    );
    let router = match peer_certificate {
        Some(certificate) => router.layer(axum::Extension(super::renewal::TlsPeerCertificate(
            certificate,
        ))),
        None => router,
    };
    let tls = LifetimeLimitedIo::new(tls, lifetime);
    serve_http_connection(tls, router, timeouts, Some(lifetime), shutdown).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::{
        ca, cert, credentials::LiveNodeIdentity, identity_store, identity_store::NodeIdentity,
        mtls, renewal::TlsPeerCertificate, types::SerialNumber,
    };
    use std::time::SystemTime;

    /// Bun execs straight after it answers an upgrade, and exec closes every
    /// socket, so the answer has to be on the wire first (#526). The
    /// extension resolves only once Hyper has written a closing answer in
    /// full and ended the connection.
    #[tokio::test]
    async fn connection_closed_waits_until_a_closing_answer_is_written() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let router = axum::Router::new().route(
            "/",
            axum::routing::post(
                move |axum::Extension(closed): axum::Extension<ConnectionClosed>| {
                    let seen_tx = seen_tx.clone();
                    async move {
                        let _ = seen_tx.send(closed);
                        (
                            [(axum::http::header::CONNECTION, "close")],
                            "x".repeat(4096),
                        )
                    }
                },
            ),
        );
        // A small pipe: the answer can't all be written until the client reads.
        let (client, server) = tokio::io::duplex(64);
        tokio::spawn(serve_http_connection(
            server,
            router,
            ConnectionTimeouts::PRODUCTION,
            None,
            tokio_util::sync::CancellationToken::new(),
        ));
        let (mut read, mut write) = tokio::io::split(client);
        write
            .write_all(b"POST / HTTP/1.1\r\nhost: bun\r\ncontent-length: 0\r\n\r\n")
            .await
            .unwrap();
        let closed = seen.recv().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), closed.wait())
                .await
                .is_err(),
            "resolved while the answer was still unread"
        );
        let mut answer = Vec::new();
        read.read_to_end(&mut answer).await.unwrap();
        assert!(answer.ends_with("x".repeat(4096).as_bytes()));
        tokio::time::timeout(Duration::from_secs(5), closed.wait())
            .await
            .expect("still open after the whole answer was read");
    }

    /// A node identity for `node` whose leaf lives for `lifetime`.
    fn node_identity(
        hierarchy: &ca::CaHierarchy,
        node: &str,
        serial: u64,
        lifetime: Duration,
    ) -> NodeIdentity {
        let (csr, private_key_der) = ca::create_node_csr(node).unwrap();
        let (certificate_der, serial) = ca::sign_node_csr(
            &csr,
            node,
            SerialNumber(serial),
            lifetime,
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        NodeIdentity {
            node_id: node.into(),
            certificate_der,
            private_key_der,
            serial,
            ca_generation: 0,
            node_ca_der: hierarchy.node.ca.certificate_der.clone(),
            root_ca_der: hierarchy.root.ca.certificate_der.clone(),
            not_before: SystemTime::UNIX_EPOCH,
            not_after: SystemTime::UNIX_EPOCH,
        }
    }

    fn leaf_not_after(certificate_der: &[u8]) -> SystemTime {
        let (_, leaf) = x509_parser::parse_x509_certificate(certificate_der).unwrap();
        SystemTime::UNIX_EPOCH + Duration::from_secs(leaf.validity().not_after.timestamp() as u64)
    }

    #[test]
    fn a_connection_without_a_client_certificate_gets_the_full_lifetime() {
        assert_eq!(
            tls_connection_lifetime(None, SystemTime::now()),
            MAX_TLS_CONNECTION_LIFETIME
        );
    }

    #[test]
    fn a_connection_ends_when_its_client_leaf_expires() {
        let hierarchy = ca::generate_ca_hierarchy("leaf-lifetime", b"test-ikm").unwrap();
        let leaf = node_identity(&hierarchy, "client", 11, Duration::from_secs(600));
        let expires = leaf_not_after(&leaf.certificate_der);
        let at = |before_expiry| expires - Duration::from_secs(before_expiry);
        assert_eq!(
            tls_connection_lifetime(Some(&leaf.certificate_der), at(120)),
            Duration::from_secs(120)
        );
        assert_eq!(
            tls_connection_lifetime(Some(&leaf.certificate_der), expires),
            Duration::ZERO
        );
        assert_eq!(
            tls_connection_lifetime(
                Some(&leaf.certificate_der),
                expires + Duration::from_secs(5)
            ),
            Duration::ZERO,
            "an expired leaf gets no lifetime at all"
        );
    }

    #[test]
    fn a_long_lived_client_leaf_still_retires_at_the_maximum_lifetime() {
        let hierarchy = ca::generate_ca_hierarchy("leaf-lifetime-max", b"test-ikm").unwrap();
        let leaf = node_identity(&hierarchy, "client", 11, Duration::from_secs(86_400));
        assert_eq!(
            tls_connection_lifetime(Some(&leaf.certificate_der), SystemTime::now()),
            MAX_TLS_CONNECTION_LIFETIME
        );
    }

    #[test]
    fn an_unreadable_client_certificate_gets_no_lifetime() {
        assert_eq!(
            tls_connection_lifetime(Some(b"not a certificate"), SystemTime::now()),
            Duration::ZERO
        );
    }

    // Keep the real TLS accept loop, live certificate resolver and pooled
    // client, but decouple TLS handshake validity from the retirement clock.
    async fn pooled_renewal_fixture(retire_connection: bool) -> Result<(), String> {
        let hierarchy = ca::generate_ca_hierarchy("pooled-renewal", b"test-ikm").unwrap();
        let server = node_identity(&hierarchy, "server", 10, Duration::from_secs(3600));
        let old_leaf = node_identity(&hierarchy, "client", 11, Duration::from_secs(120));
        let old_leaf_expires = leaf_not_after(&old_leaf.certificate_der);
        let lifetime = Duration::from_millis(400);
        let directory = tempfile::tempdir().unwrap();
        identity_store::save(directory.path(), &old_leaf).unwrap();
        let live = LiveNodeIdentity::load(directory.path()).unwrap();
        let deadline = std::sync::Arc::new(std::sync::OnceLock::<tokio::time::Instant>::new());
        let expired_request = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler_deadline = deadline.clone();
        let handler_expired = expired_request.clone();
        let router = axum::Router::new().route(
            "/peer",
            axum::routing::get(move |peer: axum::Extension<TlsPeerCertificate>| {
                let deadline = handler_deadline.clone();
                let expired = handler_expired.clone();
                async move {
                    let serial = cert::serial_from_der(&peer.0.0).unwrap().0;
                    if serial == 11
                        && deadline
                            .get()
                            .is_some_and(|at| tokio::time::Instant::now() >= *at)
                    {
                        expired.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    serial.to_string()
                }
            }),
        );
        let config = mtls::build_api_server_config(&server, mtls::CrlHandle::default()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}/peer", listener.local_addr().unwrap());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let clock_deadline = deadline.clone();
        let serving = tokio::spawn(serve_router_over_tls_with_clock(
            listener,
            tokio_rustls::TlsAcceptor::from(config),
            router,
            ConnectionTimeouts::PRODUCTION,
            shutdown.clone(),
            move || {
                let _ = clock_deadline.set(tokio::time::Instant::now() + lifetime);
                if retire_connection {
                    old_leaf_expires - lifetime
                } else {
                    SystemTime::UNIX_EPOCH
                }
            },
        ));
        let http =
            mtls::build_live_cluster_http_client(&live, mtls::CrlHandle::default(), None).unwrap();
        let presented = || async {
            http.get(&url)
                .timeout(Duration::from_secs(2))
                .send()
                .await?
                .text()
                .await
        };
        let result = async {
            let first = presented().await.map_err(|error| error.to_string())?;
            if first != "11" {
                return Err(format!("first connection presented {first}"));
            }
            let expires = *deadline
                .get()
                .expect("TLS lifetime clock was read after handshake");
            let renewed = node_identity(&hierarchy, "client", 12, Duration::from_secs(3600));
            live.replace(renewed).await.unwrap();
            let finish = expires + Duration::from_secs(5);
            let mut observed_renewal = false;
            while tokio::time::Instant::now() < finish {
                let sent_at = tokio::time::Instant::now();
                // Graceful/hard retirement can race a pooled request. A
                // bounded transport error is allowed, but a successful old
                // identity after expiry is always a failure.
                if let Ok(serial) = presented().await {
                    if serial == "11" && sent_at >= expires {
                        return Err(
                            "server accepted the expired leaf over a pooled connection".into()
                        );
                    }
                    if serial != "11" && serial != "12" {
                        return Err(format!("unexpected leaf serial {serial}"));
                    }
                    if serial == "12" && sent_at >= expires {
                        observed_renewal = true;
                    }
                }
                if expired_request.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err("server accepted the expired leaf over a pooled connection".into());
                }
                if observed_renewal {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err("pooled client never reconnected with its renewed leaf".into())
        }
        .await;
        shutdown.cancel();
        serving.await.unwrap();
        result
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_opened_before_renewal_never_presents_the_expired_leaf() {
        pooled_renewal_fixture(true).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pooled_renewal_fixture_detects_disabled_connection_retirement() {
        assert_eq!(
            pooled_renewal_fixture(false).await.unwrap_err(),
            "server accepted the expired leaf over a pooled connection"
        );
    }
}

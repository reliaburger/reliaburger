//! Connection deadlines for Bun's HTTP listeners (API, registry, ingress):
//! handshake, idle, stall and lifetime limits, plus the shared accept loops.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Maximum lifetime of an authenticated API or ingress TLS connection.
pub const MAX_TLS_CONNECTION_LIFETIME: Duration = Duration::from_secs(3600);

/// Time reserved for HTTP requests to drain before the lifetime limit.
pub const TLS_CONNECTION_DRAIN_GRACE: Duration = Duration::from_secs(30);

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
    let io = StallGuardedIo::new(io, timeouts.request_head, timeouts.write_stall);
    let hyper_service = hyper_util::service::TowerToHyperService::new(router);
    let builder = http_builder(&timeouts);
    let connection =
        builder.serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(io), hyper_service);
    tokio::pin!(connection);
    let drain_at = async {
        match lifetime {
            Some(lifetime) => {
                tokio::time::sleep(lifetime.saturating_sub(TLS_CONNECTION_DRAIN_GRACE)).await
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
/// Connections retire after [`MAX_TLS_CONNECTION_LIFETIME`].
pub async fn serve_router_over_tls(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    router: axum::Router,
    timeouts: ConnectionTimeouts,
    shutdown: tokio_util::sync::CancellationToken,
) {
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
                ));
            }
        }
    }
}

async fn serve_tls_connection(
    tcp: tokio::net::TcpStream,
    remote: std::net::SocketAddr,
    acceptor: tokio_rustls::TlsAcceptor,
    router: axum::Router,
    timeouts: ConnectionTimeouts,
    shutdown: tokio_util::sync::CancellationToken,
) {
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
    let router = match peer_certificate {
        Some(certificate) => router.layer(axum::Extension(super::renewal::TlsPeerCertificate(
            certificate,
        ))),
        None => router,
    };
    let tls = LifetimeLimitedIo::new(tls, MAX_TLS_CONNECTION_LIFETIME);
    serve_http_connection(
        tls,
        router,
        timeouts,
        Some(MAX_TLS_CONNECTION_LIFETIME),
        shutdown,
    )
    .await;
}

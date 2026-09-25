//! Connection lifetime limits for TLS HTTP listeners and upgraded streams.

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
/// `axum::serve` listener never does. Handlers that must refuse secrets sent
/// in the clear (HTTP Basic credentials at the registry, for one) check for
/// this marker rather than trusting configuration that says TLS *should* be on:
/// a node whose TLS setup failed falls back to plaintext, and the marker
/// follows what actually happened on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlsTransport;

/// Serve an axum router over TLS, handshaking each connection in its own task
/// (a slow handshaker never blocks the accept loop). Mirrors the wrapper's
/// ingress TLS loop; runs until `shutdown` is cancelled.
///
/// Every request carries [`TlsTransport`], plus the client's leaf certificate
/// as [`super::renewal::TlsPeerCertificate`] when it presented one.
pub async fn serve_router_over_tls(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    router: axum::Router,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use tower::Service;
    let mut make_service = router.into_make_service();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            accepted = listener.accept() => {
                let Ok((tcp, _peer)) = accepted else { continue };
                let acceptor = acceptor.clone();
                let connection_shutdown = shutdown.clone();
                let service = match make_service.call(()).await {
                    Ok(service) => service,
                    Err(infallible) => match infallible {},
                };
                tokio::spawn(async move {
                    let Ok(Ok(tls)) = tokio::time::timeout(
                        Duration::from_secs(10), acceptor.accept(tcp),
                    ).await else { return };
                    let service = service.layer(axum::Extension(TlsTransport));
                    let service = match tls.get_ref().1.peer_certificates()
                        .and_then(|certificates| certificates.first()) {
                        Some(certificate) => service.layer(axum::Extension(
                            super::renewal::TlsPeerCertificate(certificate.clone()),
                        )),
                        None => service,
                    };
                    let tls = LifetimeLimitedIo::new(tls, MAX_TLS_CONNECTION_LIFETIME);
                    let hyper_service = hyper_util::service::TowerToHyperService::new(service);
                    let builder = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    );
                    let connection = builder.serve_connection_with_upgrades(
                        hyper_util::rt::TokioIo::new(tls), hyper_service,
                    );
                    tokio::pin!(connection);
                    let drain_after = MAX_TLS_CONNECTION_LIFETIME.saturating_sub(TLS_CONNECTION_DRAIN_GRACE);
                    tokio::select! {
                        _ = &mut connection => return,
                        _ = connection_shutdown.cancelled() => {},
                        _ = tokio::time::sleep(drain_after) => {},
                    }
                    connection.as_mut().graceful_shutdown();
                    let _ = tokio::time::timeout(TLS_CONNECTION_DRAIN_GRACE, connection).await;
                });
            }
        }
    }
}

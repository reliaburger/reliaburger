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

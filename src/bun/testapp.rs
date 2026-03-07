/// Built-in test app for integration tests.
///
/// A configurable HTTP server whose behaviour is controlled by
/// constructor args. Returns the bound port so callers know where
/// to probe. Runs as a `tokio::spawn`ed task.
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// What behaviour the test app should exhibit.
#[derive(Debug, Clone)]
pub enum TestAppMode {
    /// Always returns 200 on any path.
    Healthy,
    /// Returns 200 for the first `n` requests, then 500.
    UnhealthyAfter(u32),
    /// Accepts connections but never responds.
    Hang,
    /// Exits cleanly after `n` requests.
    ExitAfter(u32),
    /// Responds after a delay.
    Slow(Duration),
}

/// A test HTTP server for integration tests.
pub struct TestApp {
    port: u16,
    shutdown: CancellationToken,
}

impl TestApp {
    /// Start the test app on an ephemeral port. Returns immediately.
    pub async fn start(mode: TestAppMode) -> Self {
        Self::start_on_port(mode, 0).await
    }

    /// Start the test app on a specific port (0 = ephemeral). Returns immediately.
    pub async fn start_on_port(mode: TestAppMode, port: u16) -> Self {
        let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let request_count = Arc::new(AtomicU32::new(0));

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    accept = listener.accept() => {
                        match accept {
                            Ok((mut socket, _)) => {
                                let count = request_count.fetch_add(1, Ordering::SeqCst);
                                let mode = mode.clone();
                                let token = token.clone();

                                tokio::spawn(async move {
                                    // Read the request (must consume it before responding)
                                    let mut buf = vec![0u8; 4096];
                                    let _ = socket.read(&mut buf).await;

                                    let response = match &mode {
                                        TestAppMode::Healthy => {
                                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                                                .to_string()
                                        }
                                        TestAppMode::UnhealthyAfter(n) => {
                                            if count < *n {
                                                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                                                    .to_string()
                                            } else {
                                                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\n\r\nerror"
                                                    .to_string()
                                            }
                                        }
                                        TestAppMode::Hang => {
                                            // Never respond
                                            tokio::time::sleep(Duration::from_secs(3600)).await;
                                            return;
                                        }
                                        TestAppMode::ExitAfter(n) => {
                                            if count >= *n {
                                                token.cancel();
                                            }
                                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                                                .to_string()
                                        }
                                        TestAppMode::Slow(delay) => {
                                            tokio::time::sleep(*delay).await;
                                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                                                .to_string()
                                        }
                                    };

                                    let _ = socket.write_all(response.as_bytes()).await;
                                });
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        Self { port, shutdown }
    }

    /// The port the test app is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Shut down the test app.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

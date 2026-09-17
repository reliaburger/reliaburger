/// HTTP health probing.
///
/// Performs an HTTP GET against a health endpoint and classifies the
/// response into a `HealthStatus`. Uses `reqwest` with configurable
/// timeout. Separates effectful I/O (this module) from pure decision
/// logic (`health.rs`).
use std::sync::OnceLock;

use super::health::{HealthCheckConfig, HealthStatus};

/// A local failure which must not be attributed to the workload.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProbeError {
    /// The node could not construct its shared HTTP client.
    #[error("health probe client unavailable: {0}")]
    Client(String),
    /// The configured probe cannot form a valid HTTP request.
    #[error("invalid health probe request: {0}")]
    Request(String),
}

fn probe_client() -> &'static Result<reqwest::Client, ProbeError> {
    static CLIENT: OnceLock<Result<reqwest::Client, ProbeError>> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .pool_max_idle_per_host(1)
            .build()
            .map_err(|error| ProbeError::Client(error.to_string()))
    })
}

/// Probe the local workload with a shared HTTP pool and a per-request deadline.
/// Local configuration errors are distinct from workload health failures.
pub async fn probe_health(
    config: &HealthCheckConfig,
    host: &str,
) -> Result<HealthStatus, ProbeError> {
    let url = format!(
        "{}://{}:{}{}",
        config.protocol, host, config.port, config.path
    );
    let client = probe_client().as_ref().map_err(Clone::clone)?;
    match tokio::time::timeout(
        config.timeout,
        client.get(&url).timeout(config.timeout).send(),
    )
    .await
    {
        Ok(Ok(response)) => Ok(if response.status().is_success() {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unhealthy
        }),
        Ok(Err(error)) if error.is_builder() => Err(ProbeError::Request(error.to_string())),
        Ok(Err(error)) => Ok(if error.is_timeout() {
            HealthStatus::Timeout
        } else {
            HealthStatus::ConnectionRefused
        }),
        Err(_) => Ok(HealthStatus::Timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::health::HealthCheckConfig;
    use crate::config::app::HealthProtocol;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn test_config(port: u16) -> HealthCheckConfig {
        HealthCheckConfig {
            path: "/healthz".to_string(),
            port,
            protocol: HealthProtocol::Http,
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(2),
            threshold_unhealthy: 3,
            threshold_healthy: 1,
            initial_delay: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn malformed_probe_target_is_a_local_error_not_connection_refused() {
        assert!(
            probe_health(&test_config(8080), "invalid host")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn consecutive_probes_reuse_an_http_connection() {
        use tokio::io::AsyncReadExt;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            for _ in 0..2 {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let byte = tokio::time::timeout(Duration::from_secs(3), socket.read_u8())
                        .await
                        .unwrap()
                        .unwrap();
                    request.push(byte);
                }
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n",
                    )
                    .await
                    .unwrap();
            }
        });
        let config = test_config(port);
        for _ in 0..2 {
            assert_eq!(
                probe_health(&config, "127.0.0.1").await.unwrap(),
                HealthStatus::Healthy
            );
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn redirect_is_not_a_successful_health_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        });
        assert_eq!(
            probe_health(&test_config(port), "127.0.0.1").await.unwrap(),
            HealthStatus::Unhealthy
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn healthy_on_200() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
            // Read the request first
            let mut buf = vec![0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let config = test_config(port);
        let status = probe_health(&config, "127.0.0.1").await.unwrap();
        assert_eq!(status, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn unhealthy_on_500() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\n\r\nerror";
            let mut buf = vec![0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let config = test_config(port);
        let status = probe_health(&config, "127.0.0.1").await.unwrap();
        assert_eq!(status, HealthStatus::Unhealthy);
    }

    #[tokio::test]
    async fn timeout_on_hung_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            // Accept but never respond
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let mut config = test_config(port);
        config.timeout = Duration::from_millis(200);
        let status = probe_health(&config, "127.0.0.1").await.unwrap();
        assert!(
            status == HealthStatus::Timeout || status == HealthStatus::ConnectionRefused,
            "expected timeout or connection refused, got {status:?}"
        );
    }

    #[tokio::test]
    async fn connection_refused_on_closed_port() {
        // Use a port that nothing is listening on
        let config = test_config(1);
        let status = probe_health(&config, "127.0.0.1").await.unwrap();
        assert_eq!(status, HealthStatus::ConnectionRefused);
    }

    #[tokio::test]
    async fn uses_correct_path_and_port() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let n = tokio::io::AsyncReadExt::read(&mut socket, &mut buf)
                .await
                .unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            // Verify the path is correct
            assert!(
                request.contains("GET /healthz"),
                "expected /healthz in request, got: {request}"
            );
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let config = test_config(port);
        let status = probe_health(&config, "127.0.0.1").await.unwrap();
        assert_eq!(status, HealthStatus::Healthy);
    }
}

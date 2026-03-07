/// HTTP health probing.
///
/// Performs an HTTP GET against a health endpoint and classifies the
/// response into a `HealthStatus`. Uses `reqwest` with configurable
/// timeout. Separates effectful I/O (this module) from pure decision
/// logic (`health.rs`).
use std::time::Duration;

use super::health::{HealthCheckConfig, HealthStatus};

/// Probe a health endpoint and return the result.
///
/// Performs an HTTP GET to `http://{host}:{config.port}{config.path}`
/// with a timeout of `config.timeout`.
pub async fn probe_health(config: &HealthCheckConfig, host: &str) -> HealthStatus {
    let url = format!(
        "{}://{}:{}{}",
        config.protocol, host, config.port, config.path
    );

    let client = reqwest::Client::builder()
        .timeout(config.timeout)
        .danger_accept_invalid_certs(true)
        .build();

    let client = match client {
        Ok(c) => c,
        Err(_) => return HealthStatus::ConnectionRefused,
    };

    match tokio::time::timeout(
        config.timeout + Duration::from_secs(1),
        client.get(&url).send(),
    )
    .await
    {
        Ok(Ok(response)) => {
            if response.status().is_success() {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy
            }
        }
        Ok(Err(e)) => {
            if e.is_timeout() {
                HealthStatus::Timeout
            } else {
                HealthStatus::ConnectionRefused
            }
        }
        Err(_) => HealthStatus::Timeout,
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
        let status = probe_health(&config, "127.0.0.1").await;
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
        let status = probe_health(&config, "127.0.0.1").await;
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
        let status = probe_health(&config, "127.0.0.1").await;
        assert!(
            status == HealthStatus::Timeout || status == HealthStatus::ConnectionRefused,
            "expected timeout or connection refused, got {status:?}"
        );
    }

    #[tokio::test]
    async fn connection_refused_on_closed_port() {
        // Use a port that nothing is listening on
        let config = test_config(1);
        let status = probe_health(&config, "127.0.0.1").await;
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
        let status = probe_health(&config, "127.0.0.1").await;
        assert_eq!(status, HealthStatus::Healthy);
    }
}

//! Bounded startup checks shared by setup and managed clusters.

use std::time::Duration;

use crate::bun::readiness::SubsystemState;
use crate::relish::{RelishError, client::BunClient};

/// Wait for valid version and critical-subsystem evidence under one total deadline.
/// Returns the last observed error when the deadline expires.
pub async fn wait_for_node(client: &BunClient, timeout: Duration) -> Result<(), RelishError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_error = "no response".to_string();
    loop {
        let probe = async {
            client.health().await?;
            let version = client.node_version().await?;
            version.parse::<crate::upgrade::BinaryVersion>()?;
            let evidence = client.readiness().await?;
            let critical: Vec<_> = evidence.subsystems.iter().filter(|s| s.critical).collect();
            if !evidence.ready
                || critical.is_empty()
                || critical.iter().any(|s| s.state != SubsystemState::Ready)
            {
                return Err(RelishError::InitFailed(
                    "critical subsystems are not ready".to_string(),
                ));
            }
            Ok(())
        };
        match tokio::time::timeout_at(deadline, probe).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => last_error = error.to_string(),
            Err(_) => break,
        }
        if tokio::time::timeout_at(deadline, tokio::time::sleep(Duration::from_millis(200)))
            .await
            .is_err()
        {
            break;
        }
    }
    Err(RelishError::InitFailed(format!(
        "node at {} did not become ready within {}s: {last_error}",
        client.base_url(),
        timeout.as_secs_f64()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::client::BunClient;
    use axum::{Json, Router, routing::get};
    use std::time::Duration;

    async fn server(
        ready: bool,
        version: &'static str,
    ) -> (BunClient, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/v1/health", get(|| async { Json(serde_json::json!({"status":"ok"})) }))
            .route("/v1/version", get(move || async move { Json(serde_json::json!({"version":version})) }))
            .route("/v1/readiness", get(move || async move {
                Json(serde_json::json!({"ready":ready,"observed_at_unix_ms":0,"subsystems":[{
                    "name":"agent","critical":true,"state":if ready {"ready"} else {"starting"},
                    "state_since_unix_ms":0,"last_error":null,"last_error_unix_ms":null,"restart_count":0
                }]}))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            BunClient::new_with_token(&format!("http://{address}"), None),
            task,
        )
    }

    #[tokio::test]
    async fn startup_rejects_liveness_without_ready_subsystems() {
        let (client, task) = server(false, "v0.1.0").await;
        let result = wait_for_node(&client, Duration::from_millis(100)).await;
        task.abort();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn startup_rejects_invalid_version_evidence() {
        let (client, task) = server(true, "not bun").await;
        let result = wait_for_node(&client, Duration::from_millis(100)).await;
        task.abort();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn startup_accepts_a_ready_node_with_a_valid_version() {
        let (client, task) = server(true, "v0.1.0").await;
        let result = wait_for_node(&client, Duration::from_secs(1)).await;
        task.abort();
        result.unwrap();
    }

    #[tokio::test]
    async fn startup_checks_share_one_deadline() {
        let app = Router::new().route(
            "/v1/health",
            get(|| async { std::future::pending::<String>().await }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = BunClient::new_with_token(&format!("http://{address}"), None);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_node(&client, Duration::from_millis(100)),
        )
        .await;
        task.abort();
        assert!(result.unwrap().is_err());
    }
}

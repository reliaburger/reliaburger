use axum::Router;
/// Bun local HTTP API.
///
/// An axum server on `127.0.0.1:9117` that bridges HTTP requests to
/// the agent's command channel. Handlers are thin — they construct an
/// `AgentCommand`, send it over the `mpsc` channel, and await the
/// `oneshot` response.
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;

use super::agent::{AgentCommand, InstanceStatus};

/// Shared state for API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub cmd_tx: mpsc::Sender<AgentCommand>,
}

/// Build the API router.
pub fn router(cmd_tx: mpsc::Sender<AgentCommand>) -> Router {
    let state = ApiState { cmd_tx };

    Router::new()
        .route("/v1/health", get(health_handler))
        .route("/v1/apply", post(apply_handler))
        .route("/v1/status", get(status_handler))
        .route("/v1/status/{app}/{namespace}", get(status_app_handler))
        .route("/v1/stop/{app}/{namespace}", post(stop_handler))
        .route("/v1/logs/{app}/{namespace}", get(logs_handler))
        .with_state(state)
}

/// Liveness check.
async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

/// Deploy workloads from JSON config.
async fn apply_handler(State(state): State<ApiState>, body: String) -> Response {
    let config = match Config::parse(&body) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    if let Err(e) = config.validate() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Deploy {
            config,
            response: resp_tx,
        })
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent unavailable" })),
        )
            .into_response();
    }

    match resp_rx.await {
        Ok(Ok(result)) => (StatusCode::OK, Json(serde_json::json!(result))).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
    }
}

/// List all instances.
async fn status_handler(State(state): State<ApiState>) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Status { response: resp_tx })
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent unavailable" })),
        )
            .into_response();
    }

    match resp_rx.await {
        Ok(statuses) => Json(serde_json::json!(statuses)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
    }
}

/// Status for a specific app.
async fn status_app_handler(
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Status { response: resp_tx })
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent unavailable" })),
        )
            .into_response();
    }

    match resp_rx.await {
        Ok(statuses) => {
            let filtered: Vec<&InstanceStatus> = statuses
                .iter()
                .filter(|s| s.app_name == app && s.namespace == namespace)
                .collect();
            if filtered.is_empty() {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": format!("app {app} not found in {namespace}") })),
                )
                    .into_response()
            } else {
                Json(serde_json::json!(filtered)).into_response()
            }
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
    }
}

/// Stop an app.
async fn stop_handler(
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Stop {
            app_name: app,
            namespace,
            response: resp_tx,
        })
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent unavailable" })),
        )
            .into_response();
    }

    match resp_rx.await {
        Ok(Ok(())) => Json(serde_json::json!({ "status": "stopped" })).into_response(),
        Ok(Err(e)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
    }
}

/// Get logs for an app.
async fn logs_handler(
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Logs {
            app_name: app,
            namespace,
            response: resp_tx,
        })
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent unavailable" })),
        )
            .into_response();
    }

    match resp_rx.await {
        Ok(Ok(logs)) => Json(serde_json::json!({ "logs": logs })).into_response(),
        Ok(Err(e)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::bun::agent::BunAgent;
    use crate::grill::mock::MockGrill;
    use crate::grill::port::PortAllocator;
    use tokio_util::sync::CancellationToken;

    /// Start a test agent and return the router and shutdown handle.
    fn test_setup() -> (Router, CancellationToken) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());

        tokio::spawn(async move {
            agent.run().await;
        });

        let app = router(cmd_tx);
        (app, shutdown)
    }

    #[tokio::test]
    async fn health_endpoint_returns_200() {
        let (app, shutdown) = test_setup();

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn apply_deploys_workloads() {
        let (app, shutdown) = test_setup();

        let config_toml = r#"
            [app.web]
            image = "myapp:v1"
            port = 8080
        "#;

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/apply")
                    .header("content-type", "text/plain")
                    .body(Body::from(config_toml))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["created"], 1);

        shutdown.cancel();
    }

    #[tokio::test]
    async fn apply_invalid_config_returns_400() {
        let (app, shutdown) = test_setup();

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/apply")
                    .body(Body::from("this is not valid toml [[["))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn status_returns_instances() {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());

        tokio::spawn(async move {
            agent.run().await;
        });

        let app = router(cmd_tx.clone());

        // Deploy first via channel
        let (resp_tx, resp_rx) = oneshot::channel();
        cmd_tx
            .send(AgentCommand::Deploy {
                config: crate::config::Config::parse(
                    r#"
                    [app.web]
                    image = "myapp:v1"
                    port = 8080
                "#,
                )
                .unwrap(),
                response: resp_tx,
            })
            .await
            .unwrap();
        resp_rx.await.unwrap().unwrap();

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(!json.as_array().unwrap().is_empty());

        shutdown.cancel();
    }

    #[tokio::test]
    async fn status_nonexistent_app_returns_404() {
        let (app, shutdown) = test_setup();

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/status/nope/default")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn stop_nonexistent_app_returns_404() {
        let (app, shutdown) = test_setup();

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/stop/nope/default")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        shutdown.cancel();
    }
}

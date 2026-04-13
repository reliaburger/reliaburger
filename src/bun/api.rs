/// Bun local HTTP API.
///
/// An axum server on `127.0.0.1:9117` that bridges HTTP requests to
/// the agent's command channel. Handlers are thin — they construct an
/// `AgentCommand`, send it over the `mpsc` channel, and await the
/// `oneshot` response. The `apply` endpoint streams progress events
/// via Server-Sent Events (SSE).
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::brioche::dashboard::{DashboardApp, DashboardData, render_dashboard};
use crate::config::Config;
use crate::ketchup::log_store::LogStore;
use crate::ketchup::store::KetchupStore;
use crate::mayo::alert::AlertEvaluator;
use crate::mayo::store::MayoStore;
use crate::meat::deploy_types::DeployHistoryEntry;
use crate::pickle::types::ManifestCatalog;

use super::agent::{AgentCommand, ApplyEvent, InstanceStatus};

/// Shared state for API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub cmd_tx: mpsc::Sender<AgentCommand>,
    /// Shared metrics store (read-heavy, queries don't block the agent).
    pub mayo: Option<Arc<RwLock<MayoStore>>>,
    /// Shared log store (flat files for follow mode).
    pub ketchup: Option<Arc<RwLock<KetchupStore>>>,
    /// Shared log store (Arrow/DataFusion for SQL queries).
    pub log_store: Option<Arc<RwLock<LogStore>>>,
    /// Alert evaluator.
    pub alerts: Option<Arc<RwLock<AlertEvaluator>>>,
    /// Deploy history (shared with agent).
    pub deploy_history: Option<Arc<RwLock<Vec<DeployHistoryEntry>>>>,
    /// Pickle image catalog (shared with registry).
    pub pickle_catalog: Option<Arc<RwLock<ManifestCatalog>>>,
    /// GitOps webhook signal channel (signals the Lettuce sync loop).
    pub gitops_webhook_tx: Option<mpsc::Sender<()>>,
}

/// Build the API router.
pub fn router(
    cmd_tx: mpsc::Sender<AgentCommand>,
    mayo: Option<Arc<RwLock<MayoStore>>>,
    log_store: Option<Arc<RwLock<LogStore>>>,
    deploy_history: Option<Arc<RwLock<Vec<DeployHistoryEntry>>>>,
    pickle_catalog: Option<Arc<RwLock<ManifestCatalog>>>,
) -> Router {
    let alerts = mayo
        .as_ref()
        .map(|_| Arc::new(RwLock::new(AlertEvaluator::with_defaults())));
    let state = ApiState {
        cmd_tx,
        mayo,
        ketchup: None,
        log_store,
        alerts,
        deploy_history,
        pickle_catalog,
        gitops_webhook_tx: None,
    };

    Router::new()
        .route("/", get(dashboard_handler))
        .route("/v1/health", get(health_handler))
        .route("/v1/apply", post(apply_handler))
        .route("/v1/status", get(status_handler))
        .route("/v1/status/{app}/{namespace}", get(status_app_handler))
        .route("/v1/stop/{app}/{namespace}", post(stop_handler))
        .route("/v1/logs/{app}/{namespace}", get(logs_handler))
        .route("/v1/exec/{app}/{namespace}", post(exec_handler))
        .route("/v1/cluster/nodes", get(nodes_handler))
        .route("/v1/cluster/council", get(council_handler))
        .route("/v1/cluster/join", post(join_handler))
        .route("/v1/chaos/partition", post(chaos_partition_handler))
        .route("/v1/chaos/heal", post(chaos_heal_handler))
        .route("/v1/chaos/status", get(chaos_status_handler))
        .route("/v1/fault", post(fault_inject_handler))
        .route("/v1/fault", axum::routing::delete(fault_clear_all_handler))
        .route("/v1/fault", get(fault_list_handler))
        .route("/v1/fault/{id}", axum::routing::delete(fault_clear_handler))
        .route("/v1/resolve", get(resolve_all_handler))
        .route("/v1/resolve/{name}", get(resolve_handler))
        .route("/v1/routes", get(routes_handler))
        .route("/v1/metrics", get(metrics_query_handler))
        .route("/v1/metrics/summary", get(metrics_summary_handler))
        .route("/v1/metrics/keys", get(metrics_keys_handler))
        .route("/v1/alerts", get(alerts_handler))
        .route("/v1/logs/sql", get(logs_sql_handler))
        .route("/v1/deploys/active", get(deploys_active_handler))
        .route("/v1/deploys/history/{app}", get(deploys_history_handler))
        .route("/v1/images", get(images_handler))
        .route("/v1/batch", post(batch_submit_handler))
        .route("/v1/build", post(build_submit_handler))
        .route("/v1/gitops/webhook", post(gitops_webhook_handler))
        .with_state(state)
}

/// Liveness check.
async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

/// Deploy workloads, streaming progress via SSE.
///
/// Returns a Server-Sent Events stream. Each event's `data` field
/// contains a JSON-serialised `ApplyEvent`. The stream ends after
/// the `Complete` or `Error` event.
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

    let (event_tx, event_rx) = mpsc::channel::<ApplyEvent>(32);
    if state
        .cmd_tx
        .send(AgentCommand::Deploy {
            config,
            events: event_tx,
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

    let stream = ReceiverStream::new(event_rx).map(|apply_event| {
        let json = serde_json::to_string(&apply_event).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(Event::default().data(json))
    });

    Sse::new(stream).into_response()
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

/// Query parameters for the logs endpoint.
#[derive(Deserialize)]
struct LogsQuery {
    tail: Option<usize>,
    follow: Option<bool>,
}

/// Get logs for an app.
///
/// Supports `?tail=N` to return only the last N lines, and
/// `?follow=true` to stream new lines as an SSE stream.
async fn logs_handler(
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
) -> Response {
    let follow = query.follow.unwrap_or(false);

    if follow {
        let (lines_tx, lines_rx) = mpsc::channel::<String>(64);
        if state
            .cmd_tx
            .send(AgentCommand::FollowLogs {
                app_name: app,
                namespace,
                tail: query.tail,
                lines: lines_tx,
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

        let stream = ReceiverStream::new(lines_rx)
            .map(|line| Ok::<_, std::convert::Infallible>(Event::default().data(line)));
        return Sse::new(stream).into_response();
    }

    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Logs {
            app_name: app,
            namespace,
            tail: query.tail,
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

/// Request body for the exec endpoint.
#[derive(Deserialize)]
struct ExecRequest {
    command: Vec<String>,
}

/// Execute a command inside a running instance.
async fn exec_handler(
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Json(body): Json<ExecRequest>,
) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Exec {
            app_name: app,
            namespace,
            command: body.command,
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
        Ok(Ok(output)) => Json(serde_json::json!({ "output": output })).into_response(),
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

/// List cluster nodes.
async fn nodes_handler(State(state): State<ApiState>) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Nodes { response: resp_tx })
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
        Ok(nodes) => Json(serde_json::json!(nodes)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
    }
}

/// Show council (Raft) status.
async fn council_handler(State(state): State<ApiState>) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Council { response: resp_tx })
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
        Ok(council) => Json(serde_json::json!(council)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
    }
}

/// Request body for cluster join.
#[derive(Deserialize)]
struct JoinRequest {
    token: String,
    addr: String,
}

/// Join an existing cluster.
async fn join_handler(State(state): State<ApiState>, Json(body): Json<JoinRequest>) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Join {
            token: body.token,
            addr: body.addr,
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
        Ok(Ok(msg)) => Json(serde_json::json!({ "message": msg })).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
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

// ---------------------------------------------------------------------------
// Chaos testing endpoints
// ---------------------------------------------------------------------------

/// Request body for partition injection.
#[derive(Deserialize)]
struct ChaosPartitionRequest {
    peers: Vec<String>,
    duration_secs: u64,
}

/// Inject a network partition.
async fn chaos_partition_handler(
    State(state): State<ApiState>,
    Json(body): Json<ChaosPartitionRequest>,
) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::InjectPartition {
            peers: body.peers,
            duration_secs: body.duration_secs,
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
        Ok(Ok(msg)) => Json(serde_json::json!({ "message": msg })).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
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

/// Remove all network partitions.
async fn chaos_heal_handler(State(state): State<ApiState>) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::HealPartition { response: resp_tx })
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
        Ok(Ok(msg)) => Json(serde_json::json!({ "message": msg })).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
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

/// Query chaos status.
async fn chaos_status_handler(State(state): State<ApiState>) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::ChaosStatus { response: resp_tx })
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
        Ok(status) => Json(serde_json::json!(status)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
    }
}

/// Inject a fault (Smoker).
async fn fault_inject_handler(
    State(state): State<ApiState>,
    Json(request): Json<crate::smoker::types::FaultRequest>,
) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::InjectFault {
            request,
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
        Ok(Ok(summary)) => Json(serde_json::json!(summary)).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
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

/// Clear a specific fault by ID.
async fn fault_clear_handler(State(state): State<ApiState>, Path(id): Path<u64>) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::ClearFault {
            fault_id: id,
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
        Ok(Ok(msg)) => Json(serde_json::json!({ "message": msg })).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
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

/// Clear all active faults.
async fn fault_clear_all_handler(State(state): State<ApiState>) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::ClearAllFaults { response: resp_tx })
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
        Ok(Ok(msg)) => Json(serde_json::json!({ "message": msg })).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
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

/// List all active faults.
async fn fault_list_handler(State(state): State<ApiState>) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::ListFaults { response: resp_tx })
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
        Ok(summaries) => Json(serde_json::json!(summaries)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
    }
}

/// Resolve a service name to its VIP and backends.
async fn resolve_handler(State(state): State<ApiState>, Path(name): Path<String>) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Resolve {
            app_name: name.clone(),
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
        Ok(Some(info)) => Json(serde_json::json!(info)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("service {name:?} not found") })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
    }
}

/// List all registered services.
async fn resolve_all_handler(State(state): State<ApiState>) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::ResolveAll { response: resp_tx })
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
        Ok(entries) => Json(serde_json::json!(entries)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
    }
}

/// List all ingress routes.
async fn routes_handler(State(state): State<ApiState>) -> Response {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Routes { response: resp_tx })
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
        Ok(routes) => Json(serde_json::json!(routes)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Metrics endpoints (Mayo)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MetricsQueryParams {
    name: Option<String>,
    start: Option<u64>,
    end: Option<u64>,
}

/// `GET /v1/metrics?name=X&start=S&end=E` — query time-series data.
async fn metrics_query_handler(
    State(state): State<ApiState>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    let Some(mayo) = &state.mayo else {
        return Json(serde_json::json!({"error": "metrics not enabled"})).into_response();
    };

    let store = mayo.read().await;
    let name = params.name.as_deref().unwrap_or("*");
    let start = params.start.unwrap_or(0);
    let end = params.end.unwrap_or(u64::MAX);

    if name == "*" {
        let sql = format!(
            "SELECT timestamp, metric_name, labels, value FROM metrics \
             WHERE timestamp >= {start} AND timestamp <= {end} \
             ORDER BY timestamp LIMIT 10000"
        );
        match store.query_sql(&sql).await {
            Ok(results) => {
                let data: Vec<serde_json::Value> = results
                    .iter()
                    .map(|(ts, name, labels, val)| {
                        serde_json::json!({"timestamp": ts, "metric_name": name, "labels": labels, "value": val})
                    })
                    .collect();
                Json(data).into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        }
    } else {
        match store.query(name, start, end).await {
            Ok(results) => {
                let data: Vec<serde_json::Value> = results
                    .iter()
                    .map(|(ts, name, labels, val)| {
                        serde_json::json!({"timestamp": ts, "metric_name": name, "labels": labels, "value": val})
                    })
                    .collect();
                Json(data).into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        }
    }
}

/// `GET /v1/metrics/summary` — latest value for each metric.
async fn metrics_summary_handler(State(state): State<ApiState>) -> Response {
    let Some(mayo) = &state.mayo else {
        return Json(serde_json::json!([])).into_response();
    };

    let store = mayo.read().await;
    match store.metric_names().await {
        Ok(names) => {
            // Return the list of known metrics (full summary requires more complex SQL)
            Json(serde_json::json!({"metrics": names})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /` — serve the Brioche cluster overview dashboard.
async fn dashboard_handler(State(state): State<ApiState>) -> Response {
    // Gather app statuses
    let (tx, rx) = oneshot::channel();
    let _ = state
        .cmd_tx
        .send(AgentCommand::Status { response: tx })
        .await;
    let statuses = rx.await.unwrap_or_default();

    let apps: Vec<DashboardApp> = statuses
        .iter()
        .map(|s| DashboardApp {
            name: s.app_name.clone(),
            namespace: s.namespace.clone(),
            instances_running: 1,
            instances_desired: 1,
            state: s.state.clone(),
        })
        .collect();

    let alert_count = if let Some(ref alerts) = state.alerts {
        alerts.read().await.firing_alerts().len()
    } else {
        0
    };

    let data = DashboardData {
        cluster_name: String::new(),
        node_count: 1,
        app_count: apps.len(),
        alert_count,
        apps,
        nodes: vec![],
        alerts: vec![],
    };

    let html = render_dashboard(&data);
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/html; charset=utf-8".parse().unwrap());
    (StatusCode::OK, headers, html).into_response()
}

/// `GET /v1/logs/sql?q=SELECT...` — query logs via DataFusion SQL.
async fn logs_sql_handler(
    State(state): State<ApiState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(log_store) = &state.log_store else {
        return Json(serde_json::json!({"error": "log store not enabled"})).into_response();
    };

    let Some(sql) = params.get("q") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing 'q' query parameter"})),
        )
            .into_response();
    };

    let store = log_store.read().await;
    match store.query_sql_json(sql).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/alerts` — list all alert statuses.
async fn alerts_handler(State(state): State<ApiState>) -> impl IntoResponse {
    let Some(alerts) = &state.alerts else {
        return Json(serde_json::json!({"alerts": []}));
    };
    let evaluator = alerts.read().await;
    let statuses = evaluator.all_statuses();
    Json(serde_json::json!({"alerts": statuses}))
}

/// `GET /v1/metrics/keys` — list all distinct metric names.
async fn metrics_keys_handler(State(state): State<ApiState>) -> Response {
    let Some(mayo) = &state.mayo else {
        return Json(serde_json::json!({"keys": []})).into_response();
    };

    let store = mayo.read().await;
    match store.metric_names().await {
        Ok(names) => Json(serde_json::json!({"keys": names})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Deploy endpoints
// ---------------------------------------------------------------------------

/// `GET /v1/deploys/active` — list active deploys.
async fn deploys_active_handler() -> impl IntoResponse {
    // Deploys run synchronously in the agent task, so there's no
    // persistent "active" state outside the SSE stream.
    Json(serde_json::json!({"active_deploys": []}))
}

/// `GET /v1/deploys/history/{app}` — deploy history for an app.
async fn deploys_history_handler(
    State(state): State<ApiState>,
    Path(app): Path<String>,
) -> impl IntoResponse {
    let Some(history) = &state.deploy_history else {
        return Json(serde_json::json!({"app": app, "history": []}));
    };
    let all = history.read().await;
    let filtered: Vec<&DeployHistoryEntry> = all.iter().filter(|e| e.app_id.name == app).collect();
    Json(serde_json::json!({"app": app, "history": filtered}))
}

/// `GET /v1/images` — list images in the local Pickle registry.
async fn images_handler(State(state): State<ApiState>) -> impl IntoResponse {
    let Some(catalog) = &state.pickle_catalog else {
        return Json(serde_json::json!({"images": []}));
    };
    let catalog = catalog.read().await;
    let images: Vec<serde_json::Value> = catalog
        .manifests
        .iter()
        .map(|(digest, m)| {
            let tags: Vec<&str> = m.tags.iter().map(|t| t.as_str()).collect();
            let layers = m.layers.len();
            serde_json::json!({
                "repository": m.repository,
                "digest": digest,
                "tags": tags,
                "layers": layers,
                "total_size": m.total_size,
            })
        })
        .collect();
    Json(serde_json::json!({"images": images}))
}

/// Submit a batch of jobs.
async fn batch_submit_handler(
    State(state): State<ApiState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let job_names: Vec<String> = body["jobs"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::SubmitBatch {
            job_names,
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
        Ok(Ok(msg)) => Json(serde_json::json!({ "message": msg })).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
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

/// Submit a build job.
async fn build_submit_handler(
    State(state): State<ApiState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let name = body["name"].as_str().unwrap_or("").to_string();
    let context_digest = body["context_digest"].as_str().unwrap_or("").to_string();
    let destination = body["destination"].as_str().unwrap_or("").to_string();

    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::SubmitBuild {
            name,
            context_digest,
            destination,
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
        Ok(Ok(msg)) => Json(serde_json::json!({ "message": msg })).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
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

/// GitOps webhook handler.
///
/// Accepts POST from git hosting providers (GitHub, GitLab, Gitea).
/// Signals the Lettuce sync loop to trigger an immediate sync.
/// Returns 202 Accepted on success, 503 if GitOps is not configured.
async fn gitops_webhook_handler(State(state): State<ApiState>) -> Response {
    match &state.gitops_webhook_tx {
        Some(tx) => {
            let _ = tx.send(()).await;
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "message": "sync triggered" })),
            )
                .into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "gitops not configured" })),
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

        let app = router(cmd_tx, None, None, None, None);
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

    /// Parse SSE events from a response body. Each event is a line
    /// starting with "data:" followed by JSON.
    fn parse_sse_events(body: &[u8]) -> Vec<super::ApplyEvent> {
        let text = String::from_utf8_lossy(body);
        text.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str(data.trim()).ok())
            .collect()
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
        let events = parse_sse_events(&body);

        // Should end with a Complete event
        let last = events.last().expect("no SSE events in response");
        match last {
            super::ApplyEvent::Complete { created, .. } => assert_eq!(*created, 1),
            other => panic!("expected Complete event, got {other:?}"),
        }

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

        let app = router(cmd_tx.clone(), None, None, None, None);

        // Deploy first via channel
        let (event_tx, mut event_rx) = mpsc::channel(64);
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
                events: event_tx,
            })
            .await
            .unwrap();
        while event_rx.recv().await.is_some() {}

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

    #[tokio::test]
    async fn exec_nonexistent_app_returns_404() {
        let (app, shutdown) = test_setup();

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/exec/nope/default")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"command":["echo","hi"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn nodes_endpoint_returns_empty_list() {
        let (app, shutdown) = test_setup();

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/cluster/nodes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(json.is_empty());
        shutdown.cancel();
    }

    #[tokio::test]
    async fn council_endpoint_returns_default_status() {
        let (app, shutdown) = test_setup();

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/cluster/council")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["term"], 0);
        assert!(json["leader"].is_null());
        assert_eq!(json["app_count"], 0);
        assert!(json["members"].as_array().unwrap().is_empty());
        shutdown.cancel();
    }

    #[tokio::test]
    async fn join_endpoint_returns_success() {
        let (app, shutdown) = test_setup();

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/cluster/join")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"token":"abc123","addr":"10.0.1.5:9443"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["message"].as_str().unwrap().contains("10.0.1.5:9443"));
        shutdown.cancel();
    }
}

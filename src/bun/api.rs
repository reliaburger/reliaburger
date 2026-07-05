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

use crate::brioche::app_detail::render_app_detail;
use crate::brioche::assets::static_asset_handler;
use crate::brioche::dashboard::{DashboardApp, DashboardData, DashboardNode, render_dashboard};
use crate::brioche::fragments;
use crate::brioche::node_detail::render_node_detail;
use crate::brioche::types::{AppDetailData, ChartConfig, NodeDetailData, safe_env};
use crate::config::Config;
use crate::ketchup::log_store::LogStore;
use crate::ketchup::query::fan_out_query;
use crate::ketchup::store::KetchupStore;
use crate::ketchup::types::{LogEntry, LogQuery, LogQueryResult, LogQueryWarning};
use crate::mayo::alert::AlertEvaluator;
use crate::mayo::rollup::{MetricsQueryResult, MetricsQueryRow, QueryWarning};
use crate::mayo::rollup_store::RollupStore;
use crate::mayo::store::MayoStore;
use crate::meat::deploy_types::DeployHistoryEntry;
use crate::pickle::types::ManifestCatalog;

use super::agent::{AgentCommand, ApplyEvent, InstanceStatus};

/// Lightweight node membership info for cross-node queries.
///
/// Extracted from gossip `NodeMembership` to avoid pulling in
/// `Instant` fields which are not Clone-friendly across API state.
#[derive(Debug, Clone)]
pub struct NodeMembershipInfo {
    pub node_id: crate::meat::NodeId,
    pub address: std::net::SocketAddr,
}

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
    /// Council node reference (for JWKS and signing endpoints).
    pub council: Option<Arc<crate::council::CouncilNode>>,
    /// Council-side rollup store for cluster-wide metrics queries.
    pub rollup_store: Option<Arc<RwLock<RollupStore>>>,
    /// Cluster membership for cross-node queries (populated from gossip).
    pub membership: Option<Arc<RwLock<Vec<NodeMembershipInfo>>>>,
    /// API token store, seeded from the council's `SecurityState` and refreshed
    /// live. Read by the auth middleware.
    pub token_store: Option<crate::sesame::auth::TokenStore>,
    /// The cluster's internal service token, presented on cross-node fan-out
    /// calls so peers accept them as the system principal. `None` single-node.
    pub service_token: Option<String>,
    /// HTTP client for cross-node fan-out queries.
    pub http_client: reqwest::Client,
}

/// Build the API router.
#[allow(clippy::too_many_arguments)]
pub fn router(
    cmd_tx: mpsc::Sender<AgentCommand>,
    mayo: Option<Arc<RwLock<MayoStore>>>,
    log_store: Option<Arc<RwLock<LogStore>>>,
    deploy_history: Option<Arc<RwLock<Vec<DeployHistoryEntry>>>>,
    pickle_catalog: Option<Arc<RwLock<ManifestCatalog>>>,
    alerts: Option<Arc<RwLock<AlertEvaluator>>>,
    council: Option<Arc<crate::council::CouncilNode>>,
    token_store: Option<crate::sesame::auth::TokenStore>,
    service_token: Option<String>,
) -> Router {
    let state = ApiState {
        cmd_tx,
        mayo,
        ketchup: None,
        log_store,
        alerts,
        deploy_history,
        pickle_catalog,
        gitops_webhook_tx: None,
        council,
        rollup_store: None,
        membership: None,
        token_store: token_store.clone(),
        service_token: service_token.clone(),
        http_client: reqwest::Client::new(),
    };

    let auth_state = crate::sesame::auth::AuthState::new(
        token_store.unwrap_or_else(crate::sesame::auth::new_token_store),
        service_token,
    );

    // Public routes need no token: liveness, the browser dashboard + UI, and
    // the JWKS endpoint (public keys are meant to be readable).
    let public = Router::new()
        .route("/", get(dashboard_handler))
        .route("/v1/health", get(health_handler))
        .route("/v1/identity/jwks", get(identity_jwks_handler))
        .route("/ui/app/{app}/{namespace}", get(app_detail_handler))
        .route("/ui/node/{name}", get(node_detail_handler))
        .route("/ui/fragment/apps", get(fragment_apps_handler))
        .route("/ui/fragment/nodes", get(fragment_nodes_handler))
        .route("/ui/fragment/alerts", get(fragment_alerts_handler))
        .route(
            "/ui/fragment/app/{app}/{namespace}/instances",
            get(fragment_instances_handler),
        )
        .route("/ui/app/{app}/{namespace}/env", get(app_env_handler))
        .route("/ui/static/{*path}", get(static_asset_handler))
        .with_state(state.clone());

    // Everything else sits behind the auth layer. `route_layer` runs the
    // middleware only for matched routes, so a 404 doesn't demand a token.
    let protected = Router::new()
        .route("/v1/apply", post(apply_handler))
        .route("/v1/status", get(status_handler))
        .route("/v1/status/{app}/{namespace}", get(status_app_handler))
        .route("/v1/stop/{app}/{namespace}", post(stop_handler))
        .route("/v1/logs/{app}/{namespace}", get(logs_handler))
        .route(
            "/v1/logs/entries/{app}/{namespace}",
            get(logs_entries_handler),
        )
        .route(
            "/v1/logs/query/{app}/{namespace}",
            get(logs_cross_node_handler),
        )
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
        .route("/v1/metrics/rollup", get(metrics_rollup_handler))
        .route("/v1/metrics/cluster", get(metrics_cluster_handler))
        .route(
            "/v1/metrics/app/{app}/{namespace}",
            get(metrics_app_handler),
        )
        .route("/v1/alerts", get(alerts_handler))
        .route("/v1/logs/sql", get(logs_sql_handler))
        .route("/v1/deploys/active", get(deploys_active_handler))
        .route("/v1/deploys/history/{app}", get(deploys_history_handler))
        .route("/v1/images", get(images_handler))
        .route("/v1/batch", post(batch_submit_handler))
        .route("/v1/build", post(build_submit_handler))
        .route("/v1/gitops/webhook", post(gitops_webhook_handler))
        .route("/v1/identity/sign", post(identity_sign_handler))
        .route("/v1/token/create", post(token_create_handler))
        .route("/v1/token/list", get(token_list_handler))
        .route("/v1/token/revoke", post(token_revoke_handler))
        .route("/v1/secret/rotate", post(secret_rotate_handler))
        .route_layer(axum::middleware::from_fn_with_state(
            auth_state,
            crate::sesame::auth::auth_middleware,
        ))
        .with_state(state);

    public.merge(protected)
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
async fn apply_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
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
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
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
    start: Option<u64>,
    end: Option<u64>,
    grep: Option<String>,
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

/// `GET /v1/logs/entries/{app}/{namespace}?start=S&end=E&grep=G&tail=N`
///
/// Internal structured log query endpoint. Returns `Vec<LogEntry>` as
/// JSON. Called by `fan_out_query` on each node during cross-node queries.
async fn logs_entries_handler(
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
) -> Response {
    let Some(log_store) = &state.log_store else {
        return Json(Vec::<LogEntry>::new()).into_response();
    };

    let store = log_store.read().await;
    match store
        .query(
            &app,
            &namespace,
            query.start,
            query.end,
            query.grep.as_deref(),
            query.tail,
        )
        .await
    {
        Ok(entries) => Json(entries).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/logs/query/{app}/{namespace}?start=S&end=E&grep=G&tail=N`
///
/// Cross-node log query. Looks up which nodes run the app from the
/// council placement state, fans out the query to those nodes, and
/// merge-sorts results by timestamp.
async fn logs_cross_node_handler(
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
) -> Response {
    use crate::meat::types::AppId;

    // Build a LogQuery from request params
    let log_query = LogQuery {
        app: app.clone(),
        namespace: namespace.clone(),
        start: query.start,
        end: query.end,
        grep: query.grep.clone(),
        json_field: None,
        tail: None, // apply tail after merge
    };

    // If we have council + membership, do cross-node fan-out
    if let (Some(council), Some(membership)) = (&state.council, &state.membership) {
        let desired = council.desired_state().await;
        let app_id = AppId::new(&app, &namespace);

        // Find which nodes run this app
        let node_ids: Vec<crate::meat::NodeId> = desired
            .scheduling
            .get(&app_id)
            .map(|placements| placements.iter().map(|p| p.node_id.clone()).collect())
            .unwrap_or_default();

        if node_ids.is_empty() {
            return Json(LogQueryResult {
                entries: vec![],
                node_count: 0,
                warnings: vec![],
            })
            .into_response();
        }

        // Resolve NodeIds to HTTP URLs via membership table
        let members = membership.read().await;
        let mut node_urls = Vec::new();
        let mut warnings = Vec::new();

        for node_id in &node_ids {
            if let Some(info) = members.iter().find(|m| m.node_id == *node_id) {
                node_urls.push(format!("http://{}", info.address));
            } else {
                warnings.push(LogQueryWarning::NodeUnresponsive {
                    node_id: node_id.0.clone(),
                });
            }
        }
        drop(members);

        let node_count = node_urls.len() + warnings.len();

        // Fan out to all nodes
        let timeout = std::time::Duration::from_secs(10);
        match fan_out_query(&log_query, &node_urls, &state.http_client, timeout).await {
            Ok(mut entries) => {
                // Apply tail after merge (fan_out already merge-sorted)
                if let Some(tail) = query.tail
                    && entries.len() > tail
                {
                    entries = entries.split_off(entries.len() - tail);
                }
                Json(LogQueryResult {
                    entries,
                    node_count,
                    warnings,
                })
                .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        }
    } else {
        // Single-node mode: query local log store
        let Some(log_store) = &state.log_store else {
            return Json(LogQueryResult {
                entries: vec![],
                node_count: 1,
                warnings: vec![],
            })
            .into_response();
        };

        let store = log_store.read().await;
        match store
            .query(
                &app,
                &namespace,
                query.start,
                query.end,
                query.grep.as_deref(),
                query.tail,
            )
            .await
        {
            Ok(entries) => Json(LogQueryResult {
                entries,
                node_count: 1,
                warnings: vec![],
            })
            .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        }
    }
}

/// Request body for the exec endpoint.
#[derive(Deserialize)]
struct ExecRequest {
    command: Vec<String>,
}

/// Execute a command inside a running instance.
async fn exec_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Json(body): Json<ExecRequest>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
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
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(body): Json<ChaosPartitionRequest>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
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
async fn chaos_heal_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
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
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(request): Json<crate::smoker::types::FaultRequest>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
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
async fn fault_clear_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(id): Path<u64>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
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
async fn fault_clear_all_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
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

/// Gather instance statuses from the agent.
async fn gather_statuses(state: &ApiState) -> Vec<InstanceStatus> {
    let (tx, rx) = oneshot::channel();
    let _ = state
        .cmd_tx
        .send(AgentCommand::Status { response: tx })
        .await;
    rx.await.unwrap_or_default()
}

/// Build dashboard app rows from instance statuses.
fn statuses_to_dashboard_apps(statuses: &[InstanceStatus]) -> Vec<DashboardApp> {
    // Group by (app_name, namespace) to get correct instance counts.
    let mut app_map: std::collections::HashMap<(String, String), (usize, String)> =
        std::collections::HashMap::new();
    for s in statuses {
        let key = (s.app_name.clone(), s.namespace.clone());
        let entry = app_map.entry(key).or_insert((0, s.state.clone()));
        entry.0 += 1;
        // If any instance is not running, show the worst state.
        if s.state != "running" {
            entry.1 = s.state.clone();
        }
    }
    app_map
        .into_iter()
        .map(|((name, namespace), (count, state))| DashboardApp {
            name,
            namespace,
            instances_running: count,
            instances_desired: count,
            state,
        })
        .collect()
}

/// Build the dashboard data from current agent state.
async fn gather_dashboard_data(state: &ApiState) -> DashboardData {
    let statuses = gather_statuses(state).await;
    let apps = statuses_to_dashboard_apps(&statuses);

    let (alert_count, alerts) = if let Some(ref evaluator) = state.alerts {
        let eval = evaluator.read().await;
        let firing = eval.firing_alerts();
        let count = firing.len();
        let alert_rows = firing
            .iter()
            .map(|a| crate::brioche::dashboard::DashboardAlert {
                name: a.rule_name.clone(),
                severity: format!("{:?}", a.severity),
                description: a.description.clone(),
            })
            .collect();
        (count, alert_rows)
    } else {
        (0, vec![])
    };

    DashboardData {
        cluster_name: String::new(),
        node_count: 1,
        app_count: apps.len(),
        alert_count,
        apps,
        nodes: vec![],
        alerts,
    }
}

/// Return an HTML response.
fn html_response(html: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/html; charset=utf-8".parse().unwrap());
    (StatusCode::OK, headers, html).into_response()
}

/// `GET /` — serve the Brioche cluster overview dashboard.
async fn dashboard_handler(State(state): State<ApiState>) -> Response {
    let data = gather_dashboard_data(&state).await;
    html_response(render_dashboard(&data))
}

/// `GET /ui/app/{app}/{namespace}` — app detail page.
async fn app_detail_handler(
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    let statuses = gather_statuses(&state).await;
    let instances: Vec<InstanceStatus> = statuses
        .into_iter()
        .filter(|s| s.app_name == app && s.namespace == namespace)
        .collect();

    let overall_state = if instances.is_empty() {
        "unknown".to_string()
    } else if instances.iter().all(|i| i.state == "running") {
        "running".to_string()
    } else {
        instances
            .iter()
            .find(|i| i.state != "running")
            .map(|i| i.state.clone())
            .unwrap_or_else(|| "unknown".to_string())
    };

    // Get env vars from deployed spec
    let (env_tx, env_rx) = oneshot::channel();
    let _ = state
        .cmd_tx
        .send(AgentCommand::AppConfig {
            app_name: app.clone(),
            namespace: namespace.clone(),
            response: env_tx,
        })
        .await;
    let env = match env_rx.await {
        Ok(Some(spec)) => safe_env(&spec.env),
        _ => vec![],
    };

    // Get deploy history
    let deploy_history = if let Some(ref history) = state.deploy_history {
        let h = history.read().await;
        h.iter().filter(|e| e.app_id.name == app).cloned().collect()
    } else {
        vec![]
    };

    let charts = vec![
        ChartConfig {
            endpoint: format!("/v1/metrics/app/{app}/{namespace}?name=process_cpu_percent"),
            title: "CPU Usage".to_string(),
            y_label: "%".to_string(),
            refresh_secs: 10,
            range_secs: 3600,
        },
        ChartConfig {
            endpoint: format!("/v1/metrics/app/{app}/{namespace}?name=process_memory_bytes"),
            title: "Memory Usage".to_string(),
            y_label: "bytes".to_string(),
            refresh_secs: 10,
            range_secs: 3600,
        },
    ];

    let data = AppDetailData {
        app_name: app,
        namespace,
        state: overall_state,
        instances,
        env,
        deploy_history,
        charts,
    };

    html_response(render_app_detail(&data))
}

/// `GET /ui/node/{name}` — node detail page.
async fn node_detail_handler(State(state): State<ApiState>, Path(name): Path<String>) -> Response {
    let statuses = gather_statuses(&state).await;

    let data = NodeDetailData {
        name,
        state: "alive".to_string(),
        app_count: statuses.len(),
        apps: statuses,
        charts: vec![
            ChartConfig {
                endpoint: "/v1/metrics?name=node_cpu_usage_percent".to_string(),
                title: "CPU Usage".to_string(),
                y_label: "%".to_string(),
                refresh_secs: 10,
                range_secs: 3600,
            },
            ChartConfig {
                endpoint: "/v1/metrics?name=node_memory_used_bytes".to_string(),
                title: "Memory Usage".to_string(),
                y_label: "bytes".to_string(),
                refresh_secs: 10,
                range_secs: 3600,
            },
        ],
    };

    html_response(render_node_detail(&data))
}

/// `GET /ui/fragment/apps` — apps table HTML fragment for HTMX swap.
async fn fragment_apps_handler(State(state): State<ApiState>) -> Response {
    let statuses = gather_statuses(&state).await;
    let apps = statuses_to_dashboard_apps(&statuses);
    html_response(fragments::render_apps_table_fragment(&apps))
}

/// `GET /ui/fragment/nodes` — nodes table HTML fragment for HTMX swap.
async fn fragment_nodes_handler(State(_state): State<ApiState>) -> Response {
    // In single-node mode, the nodes list is empty.
    let nodes: Vec<DashboardNode> = vec![];
    html_response(fragments::render_nodes_table_fragment(&nodes))
}

/// `GET /ui/fragment/alerts` — alerts table HTML fragment for HTMX swap.
async fn fragment_alerts_handler(State(state): State<ApiState>) -> Response {
    let alerts = if let Some(ref evaluator) = state.alerts {
        let eval = evaluator.read().await;
        eval.firing_alerts()
            .iter()
            .map(|a| crate::brioche::dashboard::DashboardAlert {
                name: a.rule_name.clone(),
                severity: format!("{:?}", a.severity),
                description: a.description.clone(),
            })
            .collect()
    } else {
        vec![]
    };
    html_response(fragments::render_alerts_table_fragment(&alerts))
}

/// `GET /ui/fragment/app/{app}/{namespace}/instances` — instance table fragment.
async fn fragment_instances_handler(
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    let statuses = gather_statuses(&state).await;
    let instances: Vec<InstanceStatus> = statuses
        .into_iter()
        .filter(|s| s.app_name == app && s.namespace == namespace)
        .collect();
    html_response(fragments::render_instance_table_fragment(&instances))
}

/// `GET /ui/app/{app}/{namespace}/env` — safe environment variables (JSON).
///
/// Encrypted values are replaced with `"[encrypted]"`. The raw
/// ciphertext never reaches the browser.
async fn app_env_handler(
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    let (tx, rx) = oneshot::channel();
    let _ = state
        .cmd_tx
        .send(AgentCommand::AppConfig {
            app_name: app,
            namespace,
            response: tx,
        })
        .await;

    match rx.await {
        Ok(Some(spec)) => Json(safe_env(&spec.env)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "app not found"})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "agent unavailable"})),
        )
            .into_response(),
    }
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

/// `GET /v1/metrics/rollup?name=X&start=S&end=E` — query local rollup store.
///
/// Internal endpoint used by cluster-wide query fan-out. Each council
/// member evaluates this against its own rollup data.
async fn metrics_rollup_handler(
    State(state): State<ApiState>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    let Some(rollup_store) = &state.rollup_store else {
        return Json(Vec::<MetricsQueryRow>::new()).into_response();
    };

    let store = rollup_store.read().await;
    let start = params.start.unwrap_or(0);
    let end = params.end.unwrap_or(u64::MAX);

    let result = match &params.name {
        Some(name) => store.query_cluster_metric(name, start, end).await,
        None => {
            let sql = format!(
                "SELECT timestamp, metric_name, labels, SUM(sum_val) as total_sum \
                 FROM rollups \
                 WHERE timestamp >= {start} AND timestamp <= {end} \
                 GROUP BY timestamp, metric_name, labels \
                 ORDER BY timestamp LIMIT 10000"
            );
            store.query_sql(&sql).await
        }
    };

    match result {
        Ok(rows) => {
            let data: Vec<MetricsQueryRow> = rows
                .into_iter()
                .map(|(ts, name, labels, val)| MetricsQueryRow {
                    timestamp: ts,
                    metric_name: name,
                    labels,
                    value: val,
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

/// `GET /v1/metrics/cluster?name=X&start=S&end=E` — cluster-wide query.
///
/// Fans out to all council aggregators' `/v1/metrics/rollup` endpoints,
/// merges results, and returns the combined data with any warnings
/// about unresponsive aggregators.
async fn metrics_cluster_handler(
    State(state): State<ApiState>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    let Some(rollup_store) = &state.rollup_store else {
        return Json(MetricsQueryResult {
            data: vec![],
            warnings: vec![QueryWarning::NodeUnresponsive {
                node_id: "no rollup store configured".to_string(),
            }],
        })
        .into_response();
    };

    // Query local rollup store directly (this council member's data)
    let store = rollup_store.read().await;
    let start = params.start.unwrap_or(0);
    let end = params.end.unwrap_or(u64::MAX);

    let result = match &params.name {
        Some(name) => store.query_cluster_metric(name, start, end).await,
        None => {
            let sql = format!(
                "SELECT timestamp, metric_name, labels, SUM(sum_val) as total_sum \
                 FROM rollups \
                 WHERE timestamp >= {start} AND timestamp <= {end} \
                 GROUP BY timestamp, metric_name, labels \
                 ORDER BY timestamp LIMIT 10000"
            );
            store.query_sql(&sql).await
        }
    };

    match result {
        Ok(rows) => {
            let data: Vec<MetricsQueryRow> = rows
                .into_iter()
                .map(|(ts, name, labels, val)| MetricsQueryRow {
                    timestamp: ts,
                    metric_name: name,
                    labels,
                    value: val,
                })
                .collect();
            Json(MetricsQueryResult {
                data,
                warnings: vec![],
            })
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/metrics/app/{app}/{namespace}?name=X&start=S&end=E` — single-app query.
///
/// Queries the local metrics store filtered by the specified app. In a
/// full cluster deployment, this would fan out to nodes running the app
/// using the Meat placement map.
async fn metrics_app_handler(
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    let Some(mayo) = &state.mayo else {
        return Json(MetricsQueryResult {
            data: vec![],
            warnings: vec![],
        })
        .into_response();
    };

    let store = mayo.read().await;
    let start = params.start.unwrap_or(0);
    let end = params.end.unwrap_or(u64::MAX);

    // Filter by app label in the local store
    let app_filter = format!("{namespace}/{app}");
    let sql = match &params.name {
        Some(name) => format!(
            "SELECT timestamp, metric_name, labels, value FROM metrics \
             WHERE metric_name = '{name}' \
             AND labels LIKE '%\"{app_filter}\"%' \
             AND timestamp >= {start} AND timestamp <= {end} \
             ORDER BY timestamp LIMIT 10000"
        ),
        None => format!(
            "SELECT timestamp, metric_name, labels, value FROM metrics \
             WHERE labels LIKE '%\"{app_filter}\"%' \
             AND timestamp >= {start} AND timestamp <= {end} \
             ORDER BY timestamp LIMIT 10000"
        ),
    };

    match store.query_sql(&sql).await {
        Ok(rows) => {
            let data: Vec<MetricsQueryRow> = rows
                .into_iter()
                .map(|(ts, name, labels, val)| MetricsQueryRow {
                    timestamp: ts,
                    metric_name: name,
                    labels,
                    value: val,
                })
                .collect();
            Json(MetricsQueryResult {
                data,
                warnings: vec![],
            })
            .into_response()
        }
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
///
/// Not yet wired into the live agent. The batch scheduler (`schedule_batch`)
/// and `BatchTracker` exist and are unit-tested, but resolving job specs,
/// dispatching across the cluster, and tracking completion via the reporting
/// tree are deferred to Phase 12. Returns 501 rather than pretending to accept.
async fn batch_submit_handler() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "batch dispatch is not yet wired into the agent (deferred to Phase 12)"
        })),
    )
        .into_response()
}

/// Submit a build job.
///
/// Not yet wired into the live agent. `execute_build` constructs the buildah
/// commands (and is unit-tested), but downloading the context blob, spawning
/// buildah, and pushing the result are deferred to Phase 12. Returns 501
/// rather than pretending to accept.
async fn build_submit_handler() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "build execution is not yet wired into the agent (deferred to Phase 12)"
        })),
    )
        .into_response()
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

// ---------------------------------------------------------------------------
// Identity endpoints
// ---------------------------------------------------------------------------

/// JWKS endpoint — publishes the OIDC Ed25519 public key for JWT verification.
async fn identity_jwks_handler(State(state): State<ApiState>) -> Response {
    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };

    let security_state = council.security_state().await;
    let Some(ref oidc_config) = security_state.oidc_signing_config else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no OIDC signing config" })),
        )
            .into_response();
    };

    Json(crate::sesame::oidc::jwks_response(oidc_config)).into_response()
}

/// Sign an image manifest digest and attach the signature via Raft.
async fn identity_sign_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    #[derive(serde::Deserialize)]
    struct SignRequest {
        digest: String,
    }

    let req: SignRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid JSON: {e}") })),
            )
                .into_response();
        }
    };

    let (tx, rx) = oneshot::channel();
    let _ = state
        .cmd_tx
        .send(AgentCommand::SignImage {
            manifest_digest: req.digest,
            response: tx,
        })
        .await;

    match rx.await {
        Ok(Ok(msg)) => Json(serde_json::json!({ "message": msg })).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent channel closed" })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Token management endpoints
// ---------------------------------------------------------------------------

/// List API tokens from SecurityState in Raft.
async fn token_list_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };

    let security_state = council.security_state().await;
    let tokens: Vec<serde_json::Value> = security_state
        .api_tokens
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "role": t.role.to_string(),
                "expires_at": t.expires_at.map(|e| e.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()),
                "created_at": t.created_at.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
            })
        })
        .collect();

    Json(serde_json::json!({ "tokens": tokens })).into_response()
}

/// Revoke an API token by name via Raft.
async fn token_revoke_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    #[derive(serde::Deserialize)]
    struct RevokeRequest {
        name: String,
    }

    let req: RevokeRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid JSON: {e}") })),
            )
                .into_response();
        }
    };

    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };

    match council
        .write(crate::council::RaftRequest::RevokeApiToken {
            name: req.name.clone(),
        })
        .await
    {
        Ok(_) => Json(serde_json::json!({ "message": format!("token {} revoked", req.name) }))
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Create an API token and persist it via Raft.
///
/// The token is minted server-side (Argon2 hashing) and written to the
/// SecurityState in one step, so the stored hash always matches the plaintext
/// returned to the caller. The plaintext is shown once and never stored.
async fn token_create_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    #[derive(serde::Deserialize)]
    struct CreateRequest {
        name: String,
        role: String,
        #[serde(default)]
        apps: Option<Vec<String>>,
        #[serde(default)]
        namespaces: Option<Vec<String>>,
        #[serde(default)]
        ttl_days: Option<u64>,
    }

    let req: CreateRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid JSON: {e}") })),
            )
                .into_response();
        }
    };

    let role = match req.role.as_str() {
        "admin" => crate::sesame::types::ApiRole::Admin,
        "deployer" => crate::sesame::types::ApiRole::Deployer,
        "read-only" | "readonly" => crate::sesame::types::ApiRole::ReadOnly,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("unknown role: {other} (expected admin, deployer, or read-only)")
                })),
            )
                .into_response();
        }
    };

    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };

    let scope = crate::sesame::types::TokenScope {
        apps: req.apps.clone(),
        namespaces: req.namespaces.clone(),
    };
    let expires_at = req
        .ttl_days
        .map(|d| std::time::SystemTime::now() + std::time::Duration::from_secs(d * 86400));

    let created = match crate::sesame::token::create_token(&req.name, role, scope, expires_at) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    match council
        .write(crate::council::RaftRequest::CreateApiToken(created.token))
        .await
    {
        Ok(_) => Json(serde_json::json!({
            "name": req.name,
            "role": req.role,
            "token": created.plaintext,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Secret rotation endpoint
// ---------------------------------------------------------------------------

/// Rotate or finalise secret encryption key via Raft.
async fn secret_rotate_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    #[derive(serde::Deserialize)]
    struct RotateRequest {
        #[serde(default)]
        finalize: bool,
    }

    let req: RotateRequest =
        serde_json::from_str(&body).unwrap_or(RotateRequest { finalize: false });

    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };

    let scope = crate::sesame::types::AgeKeyScope::ClusterWide;

    if req.finalize {
        match council
            .write(crate::council::RaftRequest::FinalizeSecretRotation { scope })
            .await
        {
            Ok(_) => Json(
                serde_json::json!({ "message": "secret rotation finalised, old keys removed" }),
            )
            .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
        }
    } else {
        // Generate a new age keypair
        let ikm = match council.wrapping_ikm() {
            Some(ikm) => ikm,
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({ "error": "no wrapping IKM" })),
                )
                    .into_response();
            }
        };

        let security_state = council.security_state().await;
        let current_gen = security_state
            .age_keypairs
            .iter()
            .filter(|kp| kp.scope == scope)
            .map(|kp| kp.generation)
            .max()
            .unwrap_or(0);

        let new_gen = current_gen + 1;
        let (new_keypair, _identity) =
            match crate::sesame::secret::generate_age_keypair(scope.clone(), ikm, new_gen) {
                Ok(pair) => pair,
                Err(e) => {
                    return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("keypair generation failed: {e}") })),
                )
                    .into_response();
                }
            };

        let new_pubkey = new_keypair.public_key.clone();

        match council
            .write(crate::council::RaftRequest::RotateSecretKey { scope, new_keypair })
            .await
        {
            Ok(_) => Json(serde_json::json!({
                "message": format!("secret key rotated to generation {new_gen}"),
                "new_public_key": new_pubkey,
            }))
            .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
        }
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

        let app = router(cmd_tx, None, None, None, None, None, None, None, None);
        (app, shutdown)
    }

    /// Build a single-node council, initialised as leader and seeded with a
    /// real `SecurityState` (four CAs, an age keypair, an OIDC config). `tag`
    /// disambiguates the temp dir so concurrent tests don't collide.
    async fn seeded_council(tag: &str) -> Arc<crate::council::CouncilNode> {
        use std::collections::BTreeMap;

        use crate::council::log_store::MemLogStore;
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
        use crate::council::state_machine::CouncilStateMachine;
        use crate::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest};

        let raft_router = InMemoryRaftRouter::new();
        let network = InMemoryRaftNetworkFactory::new(1, raft_router.clone());
        let node = crate::council::CouncilNode::new(
            1,
            CouncilConfig::default(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap();
        raft_router.register(1, node.raft().clone()).await;
        let mut members = BTreeMap::new();
        members.insert(
            1,
            CouncilNodeInfo {
                addr: "127.0.0.1:9444".parse().unwrap(),
                name: "node-1".into(),
            },
        );
        node.initialize(members).await.unwrap();

        let dir = std::env::temp_dir().join(format!("rb-api-seeded-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        let init = crate::sesame::init::initialize_cluster("apitest", "node-1", &dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        // Retry while leadership settles after initialize.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let req = RaftRequest::SecurityStateInit(Box::new(init.security_state.clone()));
            if node.write(req).await.is_ok() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("seeding SecurityState timed out");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Arc::new(node)
    }

    /// Send a GET to `uri` against `app` and return (status, body bytes).
    async fn get(app: Router, uri: &str) -> (StatusCode, Vec<u8>) {
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, body.to_vec())
    }

    /// GET `uri` with an optional Authorization header; return the status.
    async fn get_status(app: Router, uri: &str, bearer: Option<&str>) -> StatusCode {
        let mut req = axum::http::Request::builder().uri(uri);
        if let Some(b) = bearer {
            req = req.header("authorization", format!("Bearer {b}"));
        }
        app.oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// Build a router (with a running MockGrill agent) whose token store holds
    /// `tokens` and whose auth layer knows `service_token`.
    async fn setup_with_auth(
        tokens: Vec<crate::sesame::types::ApiToken>,
        service_token: Option<String>,
    ) -> (Router, CancellationToken) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());
        tokio::spawn(async move {
            agent.run().await;
        });
        let store = crate::sesame::auth::new_token_store();
        *store.write().await = tokens;
        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(store),
            service_token,
        );
        (app, shutdown)
    }

    fn a_user_token(
        role: crate::sesame::types::ApiRole,
    ) -> (crate::sesame::types::ApiToken, String) {
        let created = crate::sesame::token::create_token(
            "u",
            role,
            crate::sesame::types::TokenScope::default(),
            None,
        )
        .unwrap();
        (created.token, created.plaintext)
    }

    #[tokio::test]
    async fn router_stays_open_when_no_user_tokens_exist() {
        let (app, shutdown) = setup_with_auth(vec![], None).await;
        assert_eq!(get_status(app, "/v1/status", None).await, StatusCode::OK);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn protected_route_returns_401_without_a_token_once_a_user_token_exists() {
        let (token, _pt) = a_user_token(crate::sesame::types::ApiRole::ReadOnly);
        let (app, shutdown) = setup_with_auth(vec![token], None).await;
        assert_eq!(
            get_status(app, "/v1/status", None).await,
            StatusCode::UNAUTHORIZED
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn protected_route_returns_200_with_a_valid_token() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::ReadOnly);
        let (app, shutdown) = setup_with_auth(vec![token], None).await;
        assert_eq!(
            get_status(app, "/v1/status", Some(&plaintext)).await,
            StatusCode::OK
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn public_routes_need_no_token() {
        // Even with enforcement on (a user token exists), health stays open.
        let (token, _pt) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let (app, shutdown) = setup_with_auth(vec![token], None).await;
        assert_eq!(get_status(app, "/v1/health", None).await, StatusCode::OK);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn service_token_authenticates_as_system() {
        let (token, _pt) = a_user_token(crate::sesame::types::ApiRole::ReadOnly);
        let (app, shutdown) = setup_with_auth(vec![token], Some("rbrg_service".to_string())).await;
        assert_eq!(
            get_status(app, "/v1/status", Some("rbrg_service")).await,
            StatusCode::OK
        );
        shutdown.cancel();
    }

    /// POST `uri` with a Bearer token; return the status.
    async fn post_status(app: Router, uri: &str, bearer: &str, body: &str) -> StatusCode {
        app.oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", format!("Bearer {bearer}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    }

    /// Build a router with a seeded council AND a user token of the given role
    /// in the store, so role authorisation can be exercised end-to-end.
    async fn setup_with_role(
        tag: &str,
        role: crate::sesame::types::ApiRole,
    ) -> (Router, CancellationToken, String) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());
        tokio::spawn(async move {
            agent.run().await;
        });
        let council = seeded_council(tag).await;
        let (token, plaintext) = a_user_token(role);
        let store = crate::sesame::auth::new_token_store();
        store.write().await.push(token);
        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            Some(council),
            Some(store),
            None,
        );
        (app, shutdown, plaintext)
    }

    #[tokio::test]
    async fn admin_token_may_create_tokens() {
        let (app, shutdown, tok) =
            setup_with_role("role-admin", crate::sesame::types::ApiRole::Admin).await;
        let status = post_status(
            app,
            "/v1/token/create",
            &tok,
            &serde_json::json!({ "name": "x", "role": "deployer" }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn deployer_token_is_forbidden_from_creating_tokens() {
        let (app, shutdown, tok) =
            setup_with_role("role-dep-tok", crate::sesame::types::ApiRole::Deployer).await;
        let status = post_status(
            app,
            "/v1/token/create",
            &tok,
            &serde_json::json!({ "name": "x", "role": "deployer" }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn deployer_token_may_apply() {
        let (app, shutdown, tok) =
            setup_with_role("role-dep-apply", crate::sesame::types::ApiRole::Deployer).await;
        let status = post_status(app, "/v1/apply", &tok, "[app.web]\nimage = \"x:1\"\n").await;
        // Deployer passes the role guard; the apply itself may then fall to
        // dry-run, but it must not be a 403.
        assert_ne!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn readonly_token_is_forbidden_from_applying() {
        let (app, shutdown, tok) =
            setup_with_role("role-ro-apply", crate::sesame::types::ApiRole::ReadOnly).await;
        let status = post_status(app, "/v1/apply", &tok, "[app.web]\nimage = \"x:1\"\n").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn service_principal_may_create_tokens() {
        let (cmd_tx, _cmd_rx) = mpsc::channel(32);
        let council = seeded_council("role-system").await;
        // A user token exists (enforcement on), but the service token wins.
        let (token, _pt) = a_user_token(crate::sesame::types::ApiRole::ReadOnly);
        let store = crate::sesame::auth::new_token_store();
        store.write().await.push(token);
        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            Some(council),
            Some(store),
            Some("rbrg_service".to_string()),
        );
        let status = post_status(
            app,
            "/v1/token/create",
            "rbrg_service",
            &serde_json::json!({ "name": "x", "role": "deployer" }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn identity_jwks_returns_key_when_council_present() {
        let (cmd_tx, _cmd_rx) = mpsc::channel(32);
        let council = seeded_council("jwks").await;
        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            Some(council),
            None,
            None,
        );

        let (status, body) = get(app, "/v1/identity/jwks").await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // JWKS response carries at least one key with the seeded OIDC material.
        assert!(
            json["keys"].as_array().is_some_and(|k| !k.is_empty()),
            "expected a JWK, got {json}"
        );
    }

    #[tokio::test]
    async fn identity_jwks_returns_503_without_council() {
        let (app, shutdown) = test_setup();
        let (status, _body) = get(app, "/v1/identity/jwks").await;
        // Single-node mode (no council) is untouched: the endpoint 503s cleanly.
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn token_create_writes_to_raft_and_returns_plaintext() {
        let (cmd_tx, _cmd_rx) = mpsc::channel(32);
        let council = seeded_council("tokencreate").await;
        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            Some(Arc::clone(&council)),
            None,
            None,
        );

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/token/create")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "name": "ci-bot", "role": "deployer" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let plaintext = json["token"].as_str().unwrap();
        assert!(plaintext.starts_with("rbrg_"), "got {plaintext}");

        // The token landed in Raft, and the returned plaintext validates
        // against the stored hash.
        let stored = council.security_state().await.api_tokens;
        let token = stored.iter().find(|t| t.name == "ci-bot").unwrap();
        assert!(crate::sesame::token::validate_token(plaintext, token).is_ok());
    }

    #[tokio::test]
    async fn token_list_returns_seeded_tokens() {
        use crate::council::types::RaftRequest;
        use crate::sesame::token::create_token;
        use crate::sesame::types::{ApiRole, TokenScope};

        let (cmd_tx, _cmd_rx) = mpsc::channel(32);
        let council = seeded_council("tokenlist").await;
        let created =
            create_token("ci-bot", ApiRole::Deployer, TokenScope::default(), None).unwrap();
        council
            .write(RaftRequest::CreateApiToken(created.token))
            .await
            .unwrap();

        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            Some(council),
            None,
            None,
        );
        let (status, body) = get(app, "/v1/token/list").await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let names: Vec<&str> = json["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"ci-bot"), "expected ci-bot in {json}");
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

        let app = router(
            cmd_tx.clone(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

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
    async fn join_endpoint_returns_error_without_council() {
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

        // Without a council, join validation fails with a 400
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        shutdown.cancel();
    }
}

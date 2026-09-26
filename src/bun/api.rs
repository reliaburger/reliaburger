/// Bun local HTTP API.
///
/// An axum server on `127.0.0.1:9117` that bridges HTTP requests to
/// the agent's command channel. Handlers are thin — they construct an
/// `AgentCommand`, send it over the `mpsc` channel, and await the
/// `oneshot` response. The `apply` endpoint streams progress events
/// via Server-Sent Events (SSE).
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::brioche::app_detail::render_app_detail;
use crate::brioche::assets::static_asset_handler;
use crate::brioche::dashboard::{DashboardApp, DashboardData, render_dashboard};
use crate::brioche::fragments;
use crate::brioche::node_detail::render_node_detail;
use crate::brioche::types::{AppDetailData, ChartConfig, NodeDetailData, safe_env};
use crate::config::Config;
use crate::ketchup::log_store::LogStore;
use crate::ketchup::query::fan_out_query;
use crate::ketchup::types::{LogEntry, LogQuery, LogQueryResult, LogQueryWarning};
use crate::mayo::alert::AlertEvaluator;
use crate::mayo::rollup::{MetricsQuery, MetricsQueryResult, MetricsQueryRow, QueryWarning};
use crate::mayo::rollup_store::RollupStore;
use crate::mayo::store::MayoStore;
use crate::meat::deploy_types::DeployHistoryEntry;
use crate::pickle::types::ManifestCatalog;
use crate::testkit::lease::{LeaseScope, is_node_job_lease};

use super::agent::{AgentCommand, ApplyEvent, InstanceStatus};

/// Lightweight node membership info for cross-node queries.
///
/// Extracted from gossip `NodeMembership` to avoid pulling in
/// `Instant` fields which are not Clone-friendly across API state.
#[derive(Debug, Clone)]
pub struct NodeMembershipInfo {
    pub node_id: crate::meat::NodeId,
    /// The node's API endpoint: the one it advertised over gossip, or a
    /// port-offset guess until that advertisement arrives.
    pub address: std::net::SocketAddr,
    /// `true` when `address` is the node's own advertisement. A guess is
    /// fine for best-effort fan-out, but anything that compares or
    /// publishes the address as the node's identity (upgrade plans, the
    /// nodes listing) must wait for the real thing: nodes sharing a host
    /// pick their ports independently, so one node's offset is not another's.
    pub api_advertised: bool,
}

/// Every member gossip still knows (alive, suspect or dead, not left), with
/// its API address.
///
/// [`ApiState::membership`] holds only live members, which is right for
/// fan-out and for injecting faults. A node-kill fault, though, closes a
/// node's cluster transports and leaves its management API open: gossip calls
/// it dead while it can still answer. The node relay and node-fault reversal
/// reach it through this table, so a caller outside the cluster network can
/// still inspect it and heal it. `bun` attaches it as a layer; without it the
/// relay reaches live members only.
#[derive(Clone)]
pub struct KnownMembers(pub Arc<RwLock<Vec<NodeMembershipInfo>>>);

/// Shared state for API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub cmd_tx: mpsc::Sender<AgentCommand>,
    /// Live long-lived-task and placement-capability evidence.
    pub readiness: super::readiness::ReadinessTracker,
    /// Durable standalone resource leases. Cluster leases live in Raft.
    pub local_test_leases: crate::testkit::lease::LocalLeaseStore,
    /// Shared metrics store (read-heavy, queries don't block the agent).
    pub mayo: Option<Arc<RwLock<MayoStore>>>,
    /// Shared log store (Arrow/DataFusion for SQL queries + `/v1/logs/entries`).
    pub log_store: Option<Arc<RwLock<LogStore>>>,
    /// Alert evaluator.
    pub alerts: Option<Arc<RwLock<AlertEvaluator>>>,
    /// Deploy history (shared with agent).
    pub deploy_history: Option<Arc<RwLock<Vec<DeployHistoryEntry>>>>,
    /// Bounded cluster event history and live feed.
    pub events: Option<Arc<RwLock<crate::bun::events::EventStore>>>,
    /// Pickle image catalog (shared with registry).
    pub pickle_catalog: Option<Arc<RwLock<ManifestCatalog>>>,
    /// GitOps webhook signal channel (signals the Lettuce sync loop).
    pub gitops_webhook_tx: Option<mpsc::Sender<()>>,
    /// GitOps webhook validator (HMAC signature, replay, rate limit).
    /// Shared and mutable because it tracks recent delivery ids and
    /// trigger timestamps across requests. `None` when no
    /// `[gitops] webhook_secret` is configured — the route then refuses
    /// every request (fail closed), since a public unauthenticated sync
    /// trigger would be a denial-of-service lever (GIT3).
    pub gitops_webhook_validator:
        Option<Arc<tokio::sync::Mutex<crate::lettuce::webhook::WebhookValidator>>>,
    /// Council node reference (for JWKS and signing endpoints).
    pub council: Option<Arc<crate::council::CouncilNode>>,
    /// Council-side rollup store for cluster-wide metrics queries.
    pub rollup_store: Option<Arc<RwLock<RollupStore>>>,
    /// Cluster membership for cross-node queries (populated from gossip).
    pub membership: Option<Arc<RwLock<Vec<NodeMembershipInfo>>>>,
    /// API token store, seeded from the council's `SecurityState` and refreshed
    /// live. Read by the auth middleware. Production Bun always supplies one;
    /// `None` remains available to small embedded/test routers.
    pub token_store: Option<crate::sesame::auth::TokenStore>,
    /// The cluster's internal service token, presented on cross-node fan-out
    /// calls so peers accept them as the system principal. `None` single-node.
    pub service_token: Option<String>,
    /// Scheme + client for cross-node agent-API calls (HTTPS + CA trust
    /// under mTLS, plain HTTP otherwise).
    pub cluster_http: crate::cluster::ClusterHttp,
    /// The API port this cluster runs on (uniform across nodes), used
    /// to derive peer API addresses from raft/gossip IPs.
    pub api_port: u16,
    /// Self-upgrade manager, for the fast dependency-free `/v1/version`
    /// endpoint. Upgrade *operations* go through the agent command channel.
    pub upgrade: Option<Arc<crate::upgrade::manager::UpgradeManager>>,
    /// Batch job tracker (Phase 12 F1). Lives leader-side: submissions
    /// and status reads leader-forward, so one tracker sees them all.
    pub batch_tracker: Arc<tokio::sync::Mutex<crate::meat::batch_tracker::BatchTracker>>,
    /// The leader's aggregated worker reports — batch capacity comes
    /// from here (the same source the deploy scheduler uses). `None`
    /// standalone; batch then schedules onto this node only.
    pub aggregated_rx:
        Option<tokio::sync::watch::Receiver<crate::reporting::aggregator::AggregatedState>>,
    /// This node's gossip name, for batch self-dispatch short-circuits.
    pub node_name: Option<String>,
    /// Immutable SPIFFE trust domain for build-signing identities.
    pub trust_domain: String,
    /// Async build tracker (Phase 12 F2). Node-local: builds live
    /// where they were submitted; delegated builds proxy status reads.
    pub build_registry: Arc<tokio::sync::Mutex<super::build_runner::BuildRegistry>>,
    /// `[images] build_timeout_secs` — ceiling per buildah stage.
    pub build_timeout_secs: u64,
    /// `[images] registry_port` — the local Pickle registry the build
    /// runner fetches context from and pushes to. Server-owned: never
    /// taken from a build request body (JOB2).
    pub registry_port: u16,
    /// The scheme the local Pickle registry actually serves on, `"http"`
    /// or `"https"` (O2). Server-owned like `registry_port`, and derived
    /// from the same condition that decides whether the registry gets a
    /// TLS identity, so build-context transfers address the registry the
    /// way it really listens instead of assuming plaintext.
    pub registry_scheme: &'static str,
    /// Capability facts only the startup path knows (config, what actually
    /// loaded). The rest of `/v1/capabilities` is derived from the `Option`
    /// fields above — see `bun::capabilities`.
    pub static_capabilities: Arc<crate::bun::capabilities::StaticCapabilities>,
    /// `[images] max_context_bytes` — hard cap on an extracted build
    /// context (JOB6).
    pub max_context_bytes: u64,
    /// `[images.trust_policy] require_signatures` — when set, a build
    /// is only `Completed` once its pushed digest carries a signature
    /// the cluster trusts (JOB7).
    pub require_signatures: bool,
    /// Batch ids with a live leader-side completion watcher in this
    /// process. Lets a restarted leader spot durable batches nobody is
    /// watching and resume them (JOB4).
    pub batch_watchers: Arc<tokio::sync::Mutex<std::collections::HashSet<u64>>>,
    /// Build ids with a live runner task in this process. A durable
    /// `Running` record without one means the node restarted mid-build
    /// and the record is terminated honestly (JOB4).
    pub active_builds: Arc<tokio::sync::Mutex<std::collections::HashSet<u64>>>,
    /// Persistent per-namespace build-signing identities. Provisioned once
    /// via the council and reused across builds, so signing doesn't mint a
    /// fresh ephemeral key per artefact (JOB7 follow-up).
    pub build_signers: Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, super::build_runner::BuildSigner>>,
    >,
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
    rollup_store: Option<Arc<RwLock<RollupStore>>>,
    membership: Option<Arc<RwLock<Vec<NodeMembershipInfo>>>>,
    gitops_webhook_tx: Option<mpsc::Sender<()>>,
    api_port: u16,
    events: Option<Arc<RwLock<crate::bun::events::EventStore>>>,
) -> Router {
    router_with_upgrade(
        cmd_tx,
        mayo,
        log_store,
        deploy_history,
        pickle_catalog,
        alerts,
        council,
        token_store,
        service_token,
        rollup_store,
        membership,
        gitops_webhook_tx,
        None,
        api_port,
        events,
        None,
        None,
        "default".to_string(),
        None,
        900,
        crate::cluster::ClusterHttp::plaintext(),
        5050,
        "http",
        256 * 1024 * 1024,
        false,
        crate::bun::capabilities::StaticCapabilities::default(),
        super::readiness::ReadinessTracker::new(),
        None,
        None,
    )
}

/// Build the API router with a self-upgrade manager attached.
#[allow(clippy::too_many_arguments)]
pub fn router_with_upgrade(
    cmd_tx: mpsc::Sender<AgentCommand>,
    mayo: Option<Arc<RwLock<MayoStore>>>,
    log_store: Option<Arc<RwLock<LogStore>>>,
    deploy_history: Option<Arc<RwLock<Vec<DeployHistoryEntry>>>>,
    pickle_catalog: Option<Arc<RwLock<ManifestCatalog>>>,
    alerts: Option<Arc<RwLock<AlertEvaluator>>>,
    council: Option<Arc<crate::council::CouncilNode>>,
    token_store: Option<crate::sesame::auth::TokenStore>,
    service_token: Option<String>,
    rollup_store: Option<Arc<RwLock<RollupStore>>>,
    membership: Option<Arc<RwLock<Vec<NodeMembershipInfo>>>>,
    gitops_webhook_tx: Option<mpsc::Sender<()>>,
    gitops_webhook_validator: Option<
        Arc<tokio::sync::Mutex<crate::lettuce::webhook::WebhookValidator>>,
    >,
    api_port: u16,
    events: Option<Arc<RwLock<crate::bun::events::EventStore>>>,
    upgrade: Option<Arc<crate::upgrade::manager::UpgradeManager>>,
    aggregated_rx: Option<
        tokio::sync::watch::Receiver<crate::reporting::aggregator::AggregatedState>,
    >,
    trust_domain: String,
    node_name: Option<String>,
    build_timeout_secs: u64,
    cluster_http: crate::cluster::ClusterHttp,
    registry_port: u16,
    registry_scheme: &'static str,
    max_context_bytes: u64,
    require_signatures: bool,
    static_capabilities: crate::bun::capabilities::StaticCapabilities,
    readiness: super::readiness::ReadinessTracker,
    local_test_leases: Option<crate::testkit::lease::LocalLeaseStore>,
    jwt_verifier: Option<crate::sesame::auth::WorkloadJwtVerifier>,
) -> Router {
    let state = ApiState {
        cmd_tx,
        readiness,
        local_test_leases: local_test_leases.unwrap_or_default(),
        mayo,
        log_store,
        alerts,
        deploy_history,
        events,
        pickle_catalog,
        gitops_webhook_tx,
        gitops_webhook_validator,
        council,
        rollup_store,
        membership,
        token_store: token_store.clone(),
        service_token: service_token.clone(),
        cluster_http,
        api_port,
        upgrade,
        batch_tracker: Arc::new(tokio::sync::Mutex::new(
            crate::meat::batch_tracker::BatchTracker::new(),
        )),
        aggregated_rx,
        trust_domain,
        node_name,
        build_registry: Arc::new(tokio::sync::Mutex::new(
            super::build_runner::BuildRegistry::default(),
        )),
        build_timeout_secs,
        registry_port,
        registry_scheme,
        static_capabilities: Arc::new(static_capabilities),
        max_context_bytes,
        require_signatures,
        batch_watchers: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
        active_builds: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
        build_signers: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    };

    spawn_node_fault_reaper(state.clone());

    let mut auth_state = crate::sesame::auth::AuthState::new(
        token_store.unwrap_or_else(crate::sesame::auth::new_token_store),
        service_token,
    );
    if let Some(verifier) = jwt_verifier {
        auth_state = auth_state.with_jwt_verifier(verifier);
    }

    // Public routes need no token: liveness, static assets, the JWKS
    // endpoint (public keys are meant to be readable), and the join endpoint
    // (authenticated by the one-time join token it carries, not a bearer
    // token — a joiner has none yet).
    let public = Router::new()
        .route("/v1/health", get(health_handler))
        .route("/v1/version", get(version_handler))
        .route("/v1/identity/jwks", get(identity_jwks_handler))
        .route("/ui/static/{*path}", get(static_asset_handler))
        .route("/v1/cluster/join", post(join_handler))
        .route("/v1/cluster/ca", get(cluster_ca_handler))
        // The GitOps webhook is public: real providers (GitHub, GitLab)
        // send `X-Hub-Signature-256`/`X-Gitlab-Token`, never a Reliaburger
        // bearer token, so it can't sit behind the bearer-auth middleware.
        // It's authenticated inside the handler by the HMAC signature over
        // the raw body, with replay and rate-limit checks (GIT3).
        .route("/v1/gitops/webhook", post(gitops_webhook_handler))
        .with_state(state.clone());

    // The login page and session exchange carry `AuthState` so they can
    // validate a pasted token and mint a session cookie. They are public (a
    // logged-out browser must reach them) but live on their own router
    // because they need a different state type.
    let auth_routes = Router::new()
        .route("/ui/login", get(login_handler))
        .route("/ui/session", post(ui_session_handler))
        .route("/ui/logout", post(ui_logout_handler))
        .with_state(auth_state.clone());

    // The dashboard and UI now sit behind auth: a bearer token or a session
    // cookie. Unauthenticated HTML navigations are redirected to /ui/login by
    // the middleware.
    let protected = Router::new()
        .route("/", get(dashboard_handler))
        .route("/ui/app/{app}/{namespace}", get(app_detail_handler))
        .route("/ui/node/{name}", get(node_detail_handler))
        .route("/ui/gitops", get(gitops_handler))
        .route("/ui/fragment/apps", get(fragment_apps_handler))
        .route("/ui/fragment/nodes", get(fragment_nodes_handler))
        .route("/ui/fragment/alerts", get(fragment_alerts_handler))
        .route(
            "/ui/fragment/app/{app}/{namespace}/instances",
            get(fragment_instances_handler),
        )
        .route("/ui/app/{app}/{namespace}/env", get(app_env_handler))
        .route("/v1/apply", post(apply_handler))
        .route("/v1/status", get(status_handler))
        .route("/v1/apps", get(current_apps_handler))
        .route("/v1/readiness", get(readiness_handler))
        .route("/v1/jobs", get(jobs_handler))
        .route("/v1/events", get(events_handler))
        .route("/v1/ws/events", get(ws_events_handler))
        .route("/v1/ws/logs/{app}/{namespace}", get(ws_logs_handler))
        .route("/v1/status/{app}/{namespace}", get(status_app_handler))
        .route("/v1/top", get(top_handler))
        .route("/v1/stop/{app}/{namespace}", post(stop_handler))
        .route("/v1/delete/{app}/{namespace}", post(delete_handler))
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
        .route("/v1/capabilities", get(capabilities_handler))
        .route(
            "/v1/capabilities/cluster",
            get(cluster_capabilities_handler),
        )
        .route(
            "/v1/cluster/renew",
            post(node_renewal_handler).layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/v1/registry/query",
            post(registry_query_handler)
                .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
                .layer(axum::middleware::from_fn(registry_proposal_deadline)),
        )
        .route(
            "/v1/registry/propose",
            post(registry_proposal_handler)
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::pickle::authority::MAX_REGISTRY_PROPOSAL_BYTES,
                ))
                .layer(axum::middleware::from_fn(registry_proposal_deadline)),
        )
        .route("/v1/diagnostics", get(diagnostics_handler))
        .route("/v1/diagnostics/apps", get(desired_apps_handler))
        .route("/v1/path", post(path_handler))
        .route("/v1/test/leases", post(test_lease_create_handler))
        .route("/v1/test/leases/{id}", get(test_lease_get_handler))
        .route("/v1/test/leases/{id}/renew", post(test_lease_renew_handler))
        .route(
            "/v1/test/leases/{id}",
            axum::routing::delete(test_lease_release_handler),
        )
        .route("/v1/cluster/nodes", get(nodes_handler))
        .route(
            "/v1/nodes/{node}/relay/{*path}",
            get(node_relay_handler).post(node_relay_handler).layer(
                axum::extract::DefaultBodyLimit::max(MAX_RELAY_REQUEST_BYTES),
            ),
        )
        .route("/v1/cluster/council", get(council_handler))
        .route("/v1/upgrade/apply", post(upgrade_apply_handler))
        .route("/v1/upgrade/status", get(upgrade_status_handler))
        .route("/v1/upgrade/rollback", post(upgrade_rollback_handler))
        .route("/v1/upgrade/start", post(upgrade_start_handler))
        .route("/v1/upgrade/cluster", get(upgrade_cluster_handler))
        .route("/v1/upgrade/resume", post(upgrade_resume_handler))
        .route("/v1/upgrade/abort", post(upgrade_abort_handler))
        .route(
            "/v1/upgrade/cluster-rollback",
            post(upgrade_cluster_rollback_handler),
        )
        .route("/v1/cluster/elect", post(cluster_elect_handler))
        .route("/v1/chaos/reserve", post(node_fault_reserve_handler))
        .route("/v1/chaos/fence", post(node_fault_fence_handler))
        .route("/v1/chaos/status", get(chaos_status_handler))
        .route(
            "/v1/snapshots/{namespace}/{app}",
            get(snapshot_list_handler).post(snapshot_create_handler),
        )
        .route(
            "/v1/snapshots/{namespace}/{app}/restore",
            post(snapshot_restore_handler),
        )
        .route(
            "/v1/snapshots/{namespace}/{app}/{name}",
            axum::routing::delete(snapshot_delete_handler),
        )
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
        .route(
            "/v1/metrics/rollup/owned",
            get(metrics_owned_rollup_handler),
        )
        .route("/v1/metrics/cluster", get(metrics_cluster_handler))
        .route(
            "/v1/metrics/app/{app}/{namespace}",
            get(metrics_app_handler),
        )
        .route(
            "/v1/metrics/app/{app}/{namespace}/chart",
            get(metrics_app_chart_handler),
        )
        .route("/v1/alerts", get(alerts_handler))
        .route("/v1/logs/sql", get(logs_sql_handler))
        .route("/v1/logs/export", post(logs_export_handler))
        .route("/v1/deploys/active", get(deploys_active_handler))
        .route("/v1/deploys/operations", get(deploys_operations_handler))
        .route(
            "/v1/deploys/operations/{id}/cancel",
            post(deploy_cancel_handler),
        )
        .route("/v1/deploys/history/{app}", get(deploys_history_handler))
        .route("/v1/rollback/{app}/{namespace}", post(rollback_handler))
        .route("/v1/nodes/decommission", post(node_decommission_handler))
        .route("/v1/placements/{node_id}", get(placements_handler))
        .route("/v1/discovery/retire", post(producer_retirement_handler))
        .route(
            "/v1/cluster/workload-csr",
            post(workload_csr_handler).layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/v1/discovery/withdrawn",
            post(endpoint_withdrawal_receipt_handler),
        )
        .route("/v1/test/leases/retired", post(test_lease_retired_handler))
        .route("/v1/images", get(images_handler))
        .route("/v1/batch", post(super::batch::batch_submit_handler))
        .route("/v1/batch/run", post(super::batch::batch_run_handler))
        .route(
            "/v1/batch/{id}/report",
            post(super::batch::batch_report_handler),
        )
        .route("/v1/batch/{id}", get(super::batch::batch_status_handler))
        .route("/v1/build", post(super::build_runner::build_submit_handler))
        .route(
            "/v1/build/run",
            post(super::build_runner::build_run_handler),
        )
        .route(
            "/v1/build/track",
            post(super::build_runner::build_track_handler),
        )
        .route(
            "/v1/build/{id}",
            get(super::build_runner::build_status_handler),
        )
        .route("/v1/identity/sign", post(identity_sign_handler))
        .route("/v1/token/create", post(token_create_handler))
        .route("/v1/token/list", get(token_list_handler))
        .route("/v1/token/revoke", post(token_revoke_handler))
        .route("/v1/join-token/create", post(join_token_create_handler))
        .route("/v1/secret/public-key", get(secret_public_key_handler))
        .route("/v1/secret/rotate", post(secret_rotate_handler))
        .route_layer(axum::middleware::from_fn_with_state(
            auth_state,
            crate::sesame::auth::auth_middleware,
        ))
        .with_state(state.clone());

    public
        .merge(auth_routes)
        .merge(protected)
        .layer(axum::middleware::from_fn_with_state(
            state,
            refuse_retired_tls_peer,
        ))
}

/// Liveness check.
async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

/// Return live critical-subsystem evidence. Unlike `/v1/health`, this is an
/// authenticated scheduling signal and returns 503 while the node is fenced.
async fn readiness_handler(State(state): State<ApiState>) -> Response {
    let evidence = state.readiness.snapshot().await;
    let status = if evidence.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(evidence)).into_response()
}

/// Report the running binary version (public, dependency-free, fast).
///
/// The upgrade orchestrator polls this to decide whether a node has
/// reached its target version, so it must answer even when the agent
/// loop is busy — hence direct manager access, not an AgentCommand.
/// `GET /v1/capabilities` — what this node has wired up.
///
/// The `Option` fields on `ApiState` are the source of truth for the
/// subsystems: a `None` there means the subsystem was never built, which is
/// exactly what a caller needs to distinguish from "built but failing".
async fn capabilities_handler(State(state): State<ApiState>) -> impl IntoResponse {
    Json(local_capability_report(&state).await)
}

async fn local_capability_report(
    state: &ApiState,
) -> crate::bun::capabilities::ClusterCapabilities {
    let wired = crate::bun::capabilities::WiredSubsystems {
        metrics: state.mayo.is_some(),
        logs: state.log_store.is_some(),
        rollups: state.rollup_store.is_some(),
        council: state.council.is_some(),
        registry: state.pickle_catalog.is_some(),
        events: state.events.is_some(),
        upgrade: state.upgrade.is_some(),
        member_count: match &state.membership {
            Some(members) => Some(members.read().await.len() as u32),
            None => None,
        },
    };
    let (readiness, placement) = state.readiness.snapshots().await;
    let gossip_fresh = readiness.subsystems.iter().any(|subsystem| {
        subsystem.name == "cluster:gossip"
            && subsystem.state == crate::bun::readiness::SubsystemState::Ready
    });
    let raft_fresh = readiness.subsystems.iter().any(|subsystem| {
        subsystem.name == "cluster:raft-rpc"
            && subsystem.state == crate::bun::readiness::SubsystemState::Ready
    });
    let members = match &state.membership {
        Some(membership) if !state.static_capabilities.cluster_mode || gossip_fresh => {
            Some(membership.read().await.clone())
        }
        Some(_) => None,
        None if state.static_capabilities.cluster_mode => None,
        None => Some(Vec::new()),
    };
    let membership_count = members.as_ref().map(|members| {
        let includes_self = members
            .iter()
            .any(|member| member.node_id.0 == state.static_capabilities.node_id);
        (members.len() + usize::from(!includes_self))
            .try_into()
            .unwrap_or(u32::MAX)
    });
    let council_quorum = match (&state.council, &members) {
        (Some(council), Some(members)) if raft_fresh => Some(council_has_live_quorum(
            council,
            members,
            &state.static_capabilities.node_id,
        )),
        (None, _) if !state.static_capabilities.cluster_mode => Some(false),
        _ => None,
    };
    let cluster_id = match &state.council {
        Some(council) => {
            let security = council.security_state().await;
            security
                .get_ca(crate::sesame::types::CaRole::Root)
                .map(|root| {
                    let digest = ring::digest::digest(&ring::digest::SHA256, &root.certificate_der);
                    format!("sha256:{}", hex::encode(digest.as_ref()))
                })
        }
        None => None,
    };
    let mut wired = wired;
    wired.member_count = membership_count;
    crate::bun::capabilities::ClusterCapabilities::derive_with_observations(
        &state.static_capabilities,
        &wired,
        crate::bun::capabilities::CapabilityObservations {
            readiness: Some(readiness),
            placement: Some(placement),
            cluster_id,
            council_quorum,
        },
    )
}

fn council_has_live_quorum(
    council: &crate::council::CouncilNode,
    members: &[NodeMembershipInfo],
    self_node_id: &str,
) -> bool {
    let metrics = council.metrics().borrow().clone();
    let voters: std::collections::BTreeSet<u64> =
        metrics.membership_config.membership().voter_ids().collect();
    if voters.is_empty() || metrics.current_leader.is_none() {
        return false;
    }
    let live: std::collections::BTreeSet<u64> = members
        .iter()
        .map(|member| crate::cluster::identity::raft_id_from_name(&member.node_id.0))
        .chain(std::iter::once(
            crate::cluster::identity::raft_id_from_name(self_node_id),
        ))
        .collect();
    voters.iter().filter(|id| live.contains(id)).count() > voters.len() / 2
}

async fn cluster_capabilities_handler(
    State(state): State<ApiState>,
) -> Json<crate::bun::capabilities::ClusterCapabilityReport> {
    let started = std::time::SystemTime::now();
    let deadline =
        tokio::time::Instant::now() + crate::bun::capabilities::CLUSTER_COLLECTION_TIMEOUT;
    let local = local_capability_report(&state).await;
    let mut nodes = vec![
        crate::bun::capabilities::CollectedNodeCapability::Evidence {
            node_id: local.node_id.clone(),
            address: "local".to_string(),
            report: Box::new(local),
        },
    ];
    let members = match &state.membership {
        Some(membership) => membership.read().await.clone(),
        None => Vec::new(),
    };
    let peers = members
        .into_iter()
        .filter(|member| member.node_id.0 != state.static_capabilities.node_id)
        .map(|member| {
            let cluster_http = state.cluster_http.clone();
            let service_token = state.service_token.clone();
            async move {
                crate::bun::capabilities::collect_peer_capability(
                    &cluster_http,
                    service_token.as_deref(),
                    &member.node_id.0,
                    member.address,
                    deadline,
                )
                .await
            }
        });
    nodes.extend(futures_util::future::join_all(peers).await);
    nodes.sort_by(|left, right| {
        collected_capability_node_id(left).cmp(collected_capability_node_id(right))
    });

    Json(crate::bun::capabilities::ClusterCapabilityReport {
        schema_version: crate::bun::capabilities::CAPABILITY_SCHEMA_VERSION,
        collected_by: state.static_capabilities.node_id.clone(),
        observed_at_unix_ms: system_time_millis(started),
        deadline_at_unix_ms: system_time_millis(
            started + crate::bun::capabilities::CLUSTER_COLLECTION_TIMEOUT,
        ),
        nodes,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticsQuery {
    window_seconds: Option<u64>,
}

/// `GET /v1/diagnostics` — bounded local evidence for `relish wtf`.
///
/// CPU throttling is a delta between two cumulative cgroup samples. The
/// caller can request a 1–10 second window; one second is the default so the
/// endpoint cannot be turned into an arbitrarily long-lived request.
async fn diagnostics_handler(
    live_identity: Option<axum::Extension<crate::sesame::credentials::LiveNodeIdentity>>,
    renewal: Option<axum::Extension<crate::sesame::renewal_worker::RenewalMonitor>>,
    State(state): State<ApiState>,
    Query(query): Query<DiagnosticsQuery>,
) -> Json<crate::bun::diagnostics::LocalDiagnosticSnapshot> {
    use crate::bun::diagnostics::{DiagnosticSource, LocalDiagnosticSnapshot};

    let window_seconds = query.window_seconds.unwrap_or(1).clamp(1, 10);
    let first_observed_at = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let storage_paths = state.static_capabilities.diagnostics.storage_paths.clone();
    let disk_task = tokio::task::spawn_blocking(move || {
        crate::bun::diagnostics::collect_disk_usage(&storage_paths, first_observed_at)
    });

    let cpu_throttling = if state.static_capabilities.cgroup_faults {
        let first_statuses = gather_statuses(&state).await;
        let first = crate::bun::diagnostics::collect_cpu_throttle_totals(
            &first_statuses,
            first_observed_at,
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_secs(window_seconds)).await;
        let observed_at = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let second_statuses = gather_statuses(&state).await;
        let second =
            crate::bun::diagnostics::collect_cpu_throttle_totals(&second_statuses, observed_at)
                .await;
        crate::bun::diagnostics::cpu_throttle_window(first, second, window_seconds, observed_at)
    } else {
        DiagnosticSource::Unsupported {
            reason: "actual CPU throttled time requires rootful Linux cgroup v2".to_string(),
        }
    };

    let observed_at = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let disks = match disk_task.await {
        Ok(disks) => disks,
        Err(error) => DiagnosticSource::Unavailable {
            reason: format!("disk capacity collector failed: {error}"),
        },
    };
    let certificates = match live_identity {
        Some(identity) => {
            let current = identity.snapshot();
            let worker_state = renewal.as_ref().map(|monitor| monitor.state());
            let rotation_state = if std::time::SystemTime::now() >= current.not_after {
                "expired"
            } else {
                worker_state.map_or("manual", |state| state.as_str())
            };
            let automatic_rotation = worker_state
                .is_some_and(|state| state != crate::sesame::renewal_worker::RenewalState::Stopped);
            match crate::bun::diagnostics::public_certificate_metadata(
                "node",
                &current.node_id,
                &current.certificate_der,
                rotation_state,
                automatic_rotation,
            ) {
                Ok(metadata) => DiagnosticSource::Available {
                    observed_at,
                    value: vec![metadata],
                },
                Err(reason) => DiagnosticSource::Unavailable { reason },
            }
        }
        None => match &state.static_capabilities.diagnostics.node_certificate {
            Some(certificate) => DiagnosticSource::Available {
                observed_at,
                value: vec![certificate.clone()],
            },
            None if !state.static_capabilities.identity => DiagnosticSource::Unsupported {
                reason: "workload identity issuance is disabled and no node mTLS leaf is loaded"
                    .to_string(),
            },
            None => DiagnosticSource::Unavailable {
                reason:
                    "identity issuance is enabled but no safe certificate inventory is available"
                        .to_string(),
            },
        },
    };

    Json(LocalDiagnosticSnapshot {
        schema_version: crate::bun::diagnostics::LOCAL_DIAGNOSTIC_SCHEMA_VERSION,
        node_id: state.static_capabilities.node_id.clone(),
        observed_at,
        disks,
        cpu_throttling,
        certificates,
    })
}

/// `GET /v1/diagnostics/apps` — desired replicas and scheduler coverage.
async fn desired_apps_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    match gather_desired_apps(&state).await {
        Ok(apps) => Json(filter_desired_apps_for_scope(apps, auth.as_deref())).into_response(),
        Err(error) => unavailable_response(error),
    }
}

fn unavailable_response(error: String) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": error})),
    )
        .into_response()
}

async fn gather_desired_apps(
    state: &ApiState,
) -> Result<Vec<crate::bun::diagnostics::DesiredAppEvidence>, String> {
    let apps = if let Some(council) = &state.council {
        let desired = council.desired_state().await;
        let live_nodes = match &state.membership {
            Some(membership) => membership.read().await.len().max(1),
            None => 1,
        };
        let mut apps = desired
            .apps
            .iter()
            .map(
                |(app_id, spec)| crate::bun::diagnostics::DesiredAppEvidence {
                    app: app_id.name.clone(),
                    namespace: app_id.namespace.clone(),
                    desired_replicas: crate::bun::diagnostics::desired_replica_count(
                        spec.replicas,
                        live_nodes,
                    ),
                    scheduled_replicas: desired.scheduling.get(app_id).map_or(0, |placements| {
                        placements.len().try_into().unwrap_or(u32::MAX)
                    }),
                    placements: desired.scheduling.get(app_id).map_or_else(
                        Default::default,
                        |placements| {
                            let mut per_node = std::collections::BTreeMap::new();
                            for placement in placements {
                                *per_node.entry(placement.node_id.0.clone()).or_insert(0u32) += 1;
                            }
                            per_node
                        },
                    ),
                    service_port: spec.port,
                },
            )
            .collect::<Vec<_>>();
        apps.sort_by(|left, right| {
            (&left.namespace, &left.app).cmp(&(&right.namespace, &right.app))
        });
        apps
    } else {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (response, receiver) = oneshot::channel();
            state
                .cmd_tx
                .send(AgentCommand::DesiredApps { response })
                .await
                .map_err(|_| "agent unavailable".to_string())?;
            receiver
                .await
                .map_err(|_| "agent dropped desired-app response".to_string())
        })
        .await
        .map_err(|_| "desired-app query timed out".to_string())??
    };
    Ok(apps)
}

/// `POST /v1/path` — fixed DNS and TCP probes from a local source workload.
async fn path_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(mut request): Json<crate::onion::trace::TraceRequest>,
) -> Response {
    if let Err(response) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return response;
    }
    if request.port == Some(0) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "path destination port must be between 1 and 65535"})),
        )
            .into_response();
    }
    if request
        .count
        .is_some_and(|count| count == 0 || count > crate::onion::trace::MAX_TRACE_CONNECTS)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!(
                "path probe count must be between 1 and {}",
                crate::onion::trace::MAX_TRACE_CONNECTS
            )})),
        )
            .into_response();
    }
    if !valid_path_label(&request.source) || !valid_path_label(&request.source_namespace) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "path source and namespace must be DNS labels"})),
        )
            .into_response();
    }
    if let Err(response) = crate::sesame::auth::authorize_scoped(
        auth.as_deref(),
        &request.source,
        &request.source_namespace,
    ) {
        return response;
    }

    let internal_destination = valid_path_label(&request.destination);
    if internal_destination {
        if !valid_path_label(&request.destination_namespace) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "internal destination namespace must be a DNS label"})),
            )
                .into_response();
        }
        if let Err(response) = crate::sesame::auth::authorize_scoped(
            auth.as_deref(),
            &request.destination,
            &request.destination_namespace,
        ) {
            return response;
        }
        let (sender, receiver) = oneshot::channel();
        if state
            .cmd_tx
            .send(AgentCommand::ResolveAll { response: sender })
            .await
            .is_err()
        {
            return (StatusCode::SERVICE_UNAVAILABLE, "agent unavailable").into_response();
        }
        let services = match receiver.await {
            Ok(services) => services,
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "agent dropped service-map response",
                )
                    .into_response();
            }
        };
        let Some(service) = services.iter().find(|service| {
            service.app_name == request.destination
                && service.namespace == request.destination_namespace
        }) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "internal destination is absent from the live service map"})),
            )
                .into_response();
        };
        request.port.get_or_insert(service.port);
    } else {
        let Some(port) = request.port else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "external path destination requires --port"})),
            )
                .into_response();
        };
        let Some(auth) = auth.as_deref() else {
            return (
                StatusCode::FORBIDDEN,
                "an external path requires an authenticated Admin credential",
            )
                .into_response();
        };
        if let Err(error) = state.static_capabilities.test_policy.authorise(
            crate::testkit::safety::OperationPermission::ProbeExternalDestination,
            &crate::testkit::safety::OperationAuthorisation {
                principal: &auth.principal_id,
                role: auth.role,
                acknowledged: false,
            },
        ) {
            return (StatusCode::FORBIDDEN, error.to_string()).into_response();
        }
        if !state
            .static_capabilities
            .test_policy
            .permits_external_probe(&request.destination, port)
        {
            return (
                StatusCode::FORBIDDEN,
                "external path destination is not exactly allowlisted as host:port",
            )
                .into_response();
        }
    }

    let (sender, receiver) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Trace {
            request,
            internal_destination,
            source_node: state.static_capabilities.node_id.clone(),
            response: sender,
        })
        .await
        .is_err()
    {
        return (StatusCode::SERVICE_UNAVAILABLE, "agent unavailable").into_response();
    }
    // DNS (8s) plus up to ten connects at three seconds each.
    match tokio::time::timeout(std::time::Duration::from_secs(45), receiver).await {
        Ok(Ok(Ok(result))) => Json(result).into_response(),
        Ok(Ok(Err(crate::bun::BunError::AppNotFound { .. }))) => (
            StatusCode::NOT_FOUND,
            "source app has no running instance on this node",
        )
            .into_response(),
        Ok(Ok(Err(crate::bun::BunError::TraceBusy))) => (
            StatusCode::TOO_MANY_REQUESTS,
            "too many path probes are already running on this node",
        )
            .into_response(),
        Ok(Ok(Err(error))) => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
        Ok(Err(_)) => (StatusCode::SERVICE_UNAVAILABLE, "agent dropped response").into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "path probe timed out after 45 seconds",
        )
            .into_response(),
    }
}

fn valid_path_label(value: &str) -> bool {
    crate::config::valid_workload_label(value)
}

fn filter_desired_apps_for_scope(
    mut apps: Vec<crate::bun::diagnostics::DesiredAppEvidence>,
    auth: Option<&crate::sesame::auth::AuthContext>,
) -> Vec<crate::bun::diagnostics::DesiredAppEvidence> {
    apps.retain(|app| {
        crate::sesame::auth::authorize_scoped(auth, &app.app, &app.namespace).is_ok()
    });
    apps
}

fn collected_capability_node_id(entry: &crate::bun::capabilities::CollectedNodeCapability) -> &str {
    match entry {
        crate::bun::capabilities::CollectedNodeCapability::Evidence { node_id, .. }
        | crate::bun::capabilities::CollectedNodeCapability::Unknown { node_id, .. } => node_id,
    }
}

fn system_time_millis(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CreateTestLeaseRequest {
    #[serde(default)]
    scope: LeaseScope,
    ttl_seconds: u64,
    namespace: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RenewTestLeaseRequest {
    ttl_seconds: u64,
}

#[allow(clippy::result_large_err)]
fn authenticated_test_user(
    auth: Option<&crate::sesame::auth::AuthContext>,
    required: crate::sesame::types::ApiRole,
) -> Result<&crate::sesame::auth::AuthContext, Response> {
    let Some(auth) = auth else {
        return Err((StatusCode::UNAUTHORIZED, "authentication required").into_response());
    };
    crate::sesame::auth::authorize_user(Some(auth), required)?;
    Ok(auth)
}

#[allow(clippy::result_large_err)]
fn test_operation_authorisation(
    state: &ApiState,
    auth: &crate::sesame::auth::AuthContext,
) -> Result<(), Response> {
    state
        .static_capabilities
        .test_policy
        .authorise(
            crate::testkit::safety::OperationPermission::ProvisionIsolatedWorkloads,
            &crate::testkit::safety::OperationAuthorisation {
                principal: &auth.token_name,
                role: auth.role,
                acknowledged: false,
            },
        )
        .map(|_| ())
        .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()).into_response())
}

#[allow(clippy::result_large_err)]
fn validate_lease_ttl(state: &ApiState, ttl_seconds: u64) -> Result<u64, Response> {
    if ttl_seconds == 0 || ttl_seconds > state.static_capabilities.test_policy.max_lease_seconds {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "ttl_seconds must be between 1 and {}",
                    state.static_capabilities.test_policy.max_lease_seconds
                )
            })),
        )
            .into_response());
    }
    Ok(ttl_seconds.saturating_mul(1_000))
}

async fn test_lease_create_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<CreateTestLeaseRequest>,
) -> Response {
    let auth =
        match authenticated_test_user(auth.as_deref(), crate::sesame::types::ApiRole::Deployer) {
            Ok(auth) => auth,
            Err(response) => return response,
        };
    if let Err(response) = test_operation_authorisation(&state, auth) {
        return response;
    }
    let ttl_millis = match validate_lease_ttl(&state, request.ttl_seconds) {
        Ok(ttl) => ttl,
        Err(response) => return response,
    };
    if request.scope == LeaseScope::NodeJobs {
        if let Err(response) = crate::sesame::auth::require_unscoped(Some(auth)) {
            return response;
        }
        if request.namespace.is_some() {
            return lease_error_response(crate::testkit::lease::LeaseError::InvalidScope);
        }
    }
    if let Some(council) = &state.council
        && request.scope == LeaseScope::Applications
        && !council.is_leader().await
    {
        let created = forward_test_lease_request(
            &state,
            council,
            reqwest::Method::POST,
            "/v1/test/leases",
            &headers,
            Some(&request),
        )
        .await;
        return await_forwarded_lease_replica(council, created).await;
    }
    let mut random = [0u8; 16];
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to generate lease id",
        )
            .into_response();
    }
    let random_id = hex::encode(random);
    let (lease_id, namespace) = match request.scope {
        LeaseScope::Applications => {
            let namespace = request
                .namespace
                .unwrap_or_else(|| format!("rbtest-{}", &random_id[..12]));
            (random_id, namespace)
        }
        LeaseScope::NodeJobs => (
            format!("node-jobs-{random_id}"),
            format!("rbtest-node-{random_id}"),
        ),
    };
    if !crate::testkit::lease::valid_test_namespace(&namespace) {
        return (
            StatusCode::BAD_REQUEST,
            crate::testkit::lease::LeaseError::InvalidNamespace.to_string(),
        )
            .into_response();
    }
    if auth
        .scoped_namespaces
        .as_ref()
        .is_some_and(|namespaces| !namespaces.contains(&namespace))
    {
        return (
            StatusCode::FORBIDDEN,
            "token scope does not allow the requested test namespace",
        )
            .into_response();
    }
    let now = crate::testkit::lease::now_unix_millis();
    let lease = match crate::testkit::lease::TestLease::new_scoped(
        lease_id,
        auth.principal_id.clone(),
        auth.token_name.clone(),
        namespace,
        now,
        now.saturating_add(ttl_millis),
        request.scope,
    ) {
        Ok(lease) => lease,
        Err(error) => return lease_error_response(error),
    };

    if let Some(council) = &state.council
        && request.scope == LeaseScope::Applications
    {
        if let Err(response) = write_lease_request(
            council,
            crate::council::RaftRequest::TestLeaseCreate(lease.clone()),
        )
        .await
        {
            return response;
        }
    } else if let Err(error) = state.local_test_leases.create(lease.clone()).await {
        return lease_error_response(error);
    }
    (StatusCode::CREATED, Json(lease)).into_response()
}

/// How long a follower holds a forwarded lease creation for its own replica.
const FORWARDED_LEASE_REPLICA_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Hold a follower's forwarded lease creation until its own replica has it.
///
/// The leader answers once a quorum has committed the lease, and that quorum
/// need not include this follower. The caller's next request, an apply that
/// carries the lease, usually comes back to this node, which checks the lease
/// against its local replica before forwarding the apply. Answering early let
/// that check report "lease not found" for a lease the caller had just been
/// given. A replica still behind at the deadline gets the lease returned
/// anyway: it exists, and a later request will find it.
async fn await_forwarded_lease_replica(
    council: &crate::council::CouncilNode,
    created: Response,
) -> Response {
    if created.status() != StatusCode::CREATED {
        return created;
    }
    let (parts, body) = created.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_LEASE_FORWARD_RESPONSE_BYTES).await else {
        return (
            StatusCode::BAD_GATEWAY,
            "failed to read leader lease response",
        )
            .into_response();
    };
    if let Ok(lease) = serde_json::from_slice::<crate::testkit::lease::TestLease>(&bytes) {
        // Subscribe before the first look, so an entry applied between the
        // look and the wait still wakes it.
        let mut applied = council.metrics();
        let _ = tokio::time::timeout(FORWARDED_LEASE_REPLICA_WAIT, async {
            while !council
                .desired_state()
                .await
                .test_leases
                .contains_key(&lease.lease_id)
            {
                if applied.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
    Response::from_parts(parts, axum::body::Body::from(bytes))
}

async fn test_lease_get_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(lease_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let auth =
        match authenticated_test_user(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly) {
            Ok(auth) => auth,
            Err(response) => return response,
        };
    if let Some(council) = &state.council
        && !is_node_job_lease(&lease_id)
        && !confirmed_lease_leader(council).await
    {
        return forward_test_lease_request::<()>(
            &state,
            council,
            reqwest::Method::GET,
            &format!("/v1/test/leases/{lease_id}"),
            &headers,
            None,
        )
        .await;
    }
    let Some(lease) = find_test_lease(&state, &lease_id).await else {
        return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
    };
    if lease.owner_id != auth.principal_id {
        if auth.role != crate::sesame::types::ApiRole::Admin {
            return lease_error_response(crate::testkit::lease::LeaseError::WrongOwner);
        }
        if let Err(response) = crate::sesame::auth::require_unscoped(Some(auth)) {
            return response;
        }
    }
    Json(lease).into_response()
}

async fn test_lease_renew_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(lease_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<RenewTestLeaseRequest>,
) -> Response {
    let auth =
        match authenticated_test_user(auth.as_deref(), crate::sesame::types::ApiRole::Deployer) {
            Ok(auth) => auth,
            Err(response) => return response,
        };
    if let Err(response) = test_operation_authorisation(&state, auth) {
        return response;
    }
    let ttl_millis = match validate_lease_ttl(&state, request.ttl_seconds) {
        Ok(ttl) => ttl,
        Err(response) => return response,
    };
    if let Some(council) = &state.council
        && !is_node_job_lease(&lease_id)
        && !council.is_leader().await
    {
        return forward_test_lease_request(
            &state,
            council,
            reqwest::Method::POST,
            &format!("/v1/test/leases/{lease_id}/renew"),
            &headers,
            Some(&request),
        )
        .await;
    }
    let now = crate::testkit::lease::now_unix_millis();
    let expires = now.saturating_add(ttl_millis);
    let Some(existing) = find_test_lease(&state, &lease_id).await else {
        return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
    };
    if let Err(error) = existing.authorise_owner(&auth.principal_id, now) {
        return lease_error_response(error);
    }

    if let Some(council) = &state.council
        && !is_node_job_lease(&lease_id)
    {
        if let Err(response) = write_lease_request(
            council,
            crate::council::RaftRequest::TestLeaseRenew {
                lease_id: lease_id.clone(),
                owner_id: auth.principal_id.clone(),
                renewed_at_unix_ms: now,
                expires_at_unix_ms: expires,
            },
        )
        .await
        {
            return response;
        }
        let Some(lease) = find_test_lease(&state, &lease_id).await else {
            return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
        };
        Json(lease).into_response()
    } else {
        match state
            .local_test_leases
            .renew(&lease_id, &auth.principal_id, now, expires)
            .await
        {
            Ok(lease) => Json(lease).into_response(),
            Err(error) => lease_error_response(error),
        }
    }
}

async fn test_lease_release_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(lease_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let auth =
        match authenticated_test_user(auth.as_deref(), crate::sesame::types::ApiRole::Deployer) {
            Ok(auth) => auth,
            Err(response) => return response,
        };
    if let Some(council) = &state.council
        && !is_node_job_lease(&lease_id)
        && !council.is_leader().await
    {
        return forward_test_lease_request::<()>(
            &state,
            council,
            reqwest::Method::DELETE,
            &format!("/v1/test/leases/{lease_id}"),
            &headers,
            None,
        )
        .await;
    }
    let Some(lease) = find_test_lease(&state, &lease_id).await else {
        return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
    };
    let owner_id = if lease.owner_id == auth.principal_id {
        Some(auth.principal_id.as_str())
    } else if auth.role == crate::sesame::types::ApiRole::Admin {
        if let Err(response) = crate::sesame::auth::require_unscoped(Some(auth)) {
            return response;
        }
        None
    } else {
        return lease_error_response(crate::testkit::lease::LeaseError::WrongOwner);
    };
    let result = if let Some(council) = &state.council
        && !is_node_job_lease(&lease_id)
    {
        crate::testkit::lease::cleanup_cluster_lease(council, &lease_id, owner_id).await
    } else {
        crate::testkit::lease::cleanup_local_lease(
            &state.local_test_leases,
            &state.cmd_tx,
            &lease_id,
            owner_id,
        )
        .await
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(crate::testkit::lease::LeaseError::NotFound) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => lease_error_response(error),
    }
}

/// Require a current quorum before a lease read or retirement instruction.
async fn confirmed_lease_leader(council: &crate::council::CouncilNode) -> bool {
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(3), council.is_leader()).await,
        Ok(true)
    )
}

async fn test_lease_retired_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(retirement): Json<crate::cluster::orchestrate::LeaseRetirement>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "not running in cluster mode",
        )
            .into_response();
    };
    if !confirmed_lease_leader(council).await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "retirement requires a current leader",
        )
            .into_response();
    }
    match write_lease_request(
        council,
        crate::council::RaftRequest::TestLeasePlacementRetired {
            lease_id: retirement.lease_id,
            placement: retirement.placement,
        },
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(response) => response,
    }
}

const MAX_LEASE_FORWARD_RESPONSE_BYTES: usize = 64 * 1024;

/// Forward a lease mutation to the current leader while retaining the
/// caller's own credentials. The leader repeats authentication and policy
/// checks; a follower never replaces user authority with the service token.
async fn forward_test_lease_request<T: Serialize + ?Sized>(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    method: reqwest::Method,
    path: &str,
    headers: &HeaderMap,
    body: Option<&T>,
) -> Response {
    let points_to_self = {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        metrics.current_leader == Some(metrics.id)
    };
    if points_to_self || headers.contains_key("x-reliaburger-lease-forwarded") {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "lease leader is unavailable; retry shortly",
        )
            .into_response();
    }
    let Some(leader_url) = leader_api_url(state, council).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no cluster leader known yet; retry shortly",
        )
            .into_response();
    };
    let mut request = state
        .cluster_http
        .client()
        .request(method, format!("{leader_url}{path}"))
        .header("x-reliaburger-lease-forwarded", "1");
    for name in [
        axum::http::header::AUTHORIZATION,
        axum::http::header::COOKIE,
    ] {
        if let Some(value) = headers.get(&name) {
            request = request.header(name.as_str(), value.as_bytes());
        }
    }
    if let Some(body) = body {
        request = request.json(body);
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut upstream = match tokio::time::timeout_at(deadline, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("failed to forward lease request to the leader: {error}"),
            )
                .into_response();
        }
        Err(_) => {
            return (StatusCode::GATEWAY_TIMEOUT, "leader request timed out").into_response();
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .cloned();
    let mut bytes = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, upstream.chunk()).await {
            Ok(Ok(Some(chunk)))
                if bytes.len().saturating_add(chunk.len()) <= MAX_LEASE_FORWARD_RESPONSE_BYTES =>
            {
                bytes.extend_from_slice(&chunk);
            }
            Ok(Ok(Some(_))) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    "leader lease response exceeded 64 KiB",
                )
                    .into_response();
            }
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("failed to read leader lease response: {error}"),
                )
                    .into_response();
            }
            Err(_) => {
                return (StatusCode::GATEWAY_TIMEOUT, "leader response timed out").into_response();
            }
        }
    }
    let mut response = Response::builder().status(status);
    if let Some(content_type) = content_type {
        response = response.header(axum::http::header::CONTENT_TYPE, content_type.as_bytes());
    }
    response
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

async fn find_test_lease(
    state: &ApiState,
    lease_id: &str,
) -> Option<crate::testkit::lease::TestLease> {
    if is_node_job_lease(lease_id) {
        return state.local_test_leases.get(lease_id).await;
    }
    match &state.council {
        Some(council) => council
            .desired_state()
            .await
            .test_leases
            .get(lease_id)
            .cloned(),
        None => state.local_test_leases.get(lease_id).await,
    }
}

// `Response` is large but it IS the HTTP reply to send on failure —
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
async fn write_lease_request(
    council: &crate::council::CouncilNode,
    request: crate::council::RaftRequest,
) -> Result<(), Response> {
    match council.write(request).await {
        Ok(crate::council::CouncilResponse::Refused { reason }) => {
            Err((StatusCode::CONFLICT, reason).into_response())
        }
        Ok(_) => Ok(()),
        Err(error) => Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()),
    }
}

fn lease_error_response(error: crate::testkit::lease::LeaseError) -> Response {
    let status = match error {
        crate::testkit::lease::LeaseError::NotFound => StatusCode::NOT_FOUND,
        crate::testkit::lease::LeaseError::CleanupPending => StatusCode::ACCEPTED,
        crate::testkit::lease::LeaseError::WrongOwner
        | crate::testkit::lease::LeaseError::ImageOwnership => StatusCode::FORBIDDEN,
        crate::testkit::lease::LeaseError::NotActive
        | crate::testkit::lease::LeaseError::Busy
        | crate::testkit::lease::LeaseError::AlreadyExists
        | crate::testkit::lease::LeaseError::NamespaceOwned
        | crate::testkit::lease::LeaseError::NamespaceMismatch
        | crate::testkit::lease::LeaseError::ResourceLimit => StatusCode::CONFLICT,
        crate::testkit::lease::LeaseError::TooManyLeases => StatusCode::TOO_MANY_REQUESTS,
        crate::testkit::lease::LeaseError::InvalidId
        | crate::testkit::lease::LeaseError::InvalidScope
        | crate::testkit::lease::LeaseError::InvalidOwner
        | crate::testkit::lease::LeaseError::InvalidNamespace
        | crate::testkit::lease::LeaseError::InvalidExpiry
        | crate::testkit::lease::LeaseError::InvalidToken
        | crate::testkit::lease::LeaseError::UnsupportedSchema { .. } => StatusCode::BAD_REQUEST,
        crate::testkit::lease::LeaseError::Persistence(_)
        | crate::testkit::lease::LeaseError::PersistenceUncertain
        | crate::testkit::lease::LeaseError::Malformed(_)
        | crate::testkit::lease::LeaseError::StoreTooLarge
        | crate::testkit::lease::LeaseError::Cleanup(_)
        | crate::testkit::lease::LeaseError::Consensus(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}

/// Report the running binary version (public, dependency-free, fast).
///
/// The upgrade orchestrator polls this to decide whether a node has
/// reached its target version, so it must answer even when the agent
/// loop is busy — hence direct manager access, not an AgentCommand.
async fn version_handler(State(state): State<ApiState>) -> impl IntoResponse {
    match &state.upgrade {
        Some(manager) => Json(serde_json::json!({
            "version": manager.running_version().to_string(),
            // The version alone doesn't identify the bytes: the upgrade
            // start gate and the orchestrator compare this digest with the
            // candidate's so a same-version build can't pass as a swap.
            "binary_sha256": manager.running_binary_sha256().await,
            "compatibility": crate::compatibility::CURRENT,
            "upgrade_in_flight": manager.upgrade_in_flight(),
            // Ids this node attempted and reverted — the orchestrator
            // reads these to detect node-side reverts.
            "failed_upgrade_ids": manager.reverted_upgrade_ids(),
            // The leader refuses a cluster upgrade up front when a node
            // reports false, rather than recording a run the node will
            // refuse and leaving it paused.
            "accepts_network_upgrades": manager.accepts_network_upgrades(),
        })),
        None => Json(serde_json::json!({
            "version": crate::upgrade::version::compiled_version().to_string(),
            "compatibility": crate::compatibility::CURRENT,
            "upgrade_in_flight": false,
            "failed_upgrade_ids": [],
            // No upgrade manager, so no way to apply a directive at all.
            "accepts_network_upgrades": false,
        })),
    }
}

/// Admin with cluster-wide authority. Upgrades, rollbacks and elections act
/// on every node and every tenant, so an Admin token scoped to some apps or
/// namespaces is refused (403) like on the other cluster-wide routes. The
/// service token (the orchestrator directing nodes) passes.
// `Response` is large but it IS the HTTP reply to send on failure.
#[allow(clippy::result_large_err)]
fn authorize_cluster_admin(
    auth: Option<&crate::sesame::auth::AuthContext>,
) -> Result<(), Response> {
    crate::sesame::auth::authorize(auth, crate::sesame::types::ApiRole::Admin)?;
    crate::sesame::auth::require_unscoped(auth)
}

/// Apply a node-level upgrade directive (admin). Responds 202 once the
/// binary is verified and staged; the process execs moments later.
async fn upgrade_apply_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(auth.as_deref()) {
        return resp;
    }
    let directive: crate::upgrade::types::UpgradeDirective = match serde_json::from_str(&body) {
        Ok(directive) => directive,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid directive: {e}") })),
            )
                .into_response();
        }
    };

    match ask_agent(&state.cmd_tx, |response| AgentCommand::UpgradeApply {
        directive,
        response,
    })
    .await
    {
        Ok(Ok(())) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "upgrading" })),
        )
            .into_response(),
        Ok(Err(crate::bun::BunError::Upgrade(
            error @ crate::upgrade::UpgradeError::AlreadyRunning { .. },
        ))) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "already_running",
                "detail": error.to_string(),
            })),
        )
            .into_response(),
        // "Not right now" (the binary's registry is unreachable or
        // restarting) is a 503, so the orchestrator re-sends the directive
        // instead of pausing the whole run on one blip.
        Ok(Err(crate::bun::BunError::Upgrade(error))) if error.is_transient() => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => agent_unavailable(),
    }
}

/// Node-level upgrade status: running version, in-flight marker, history.
async fn upgrade_status_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::UpgradeStatus {
        response,
    })
    .await
    {
        Ok(Ok(status)) => Json(status).into_response(),
        Ok(Err(e)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => agent_unavailable(),
    }
}

/// Revert this node to a previous binary version (admin).
async fn upgrade_rollback_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(auth.as_deref()) {
        return resp;
    }
    #[derive(serde::Deserialize, Default)]
    struct RollbackRequest {
        #[serde(default)]
        version: Option<crate::upgrade::BinaryVersion>,
    }
    let request: RollbackRequest = if body.trim().is_empty() {
        RollbackRequest::default()
    } else {
        match serde_json::from_str(&body) {
            Ok(request) => request,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("invalid request: {e}") })),
                )
                    .into_response();
            }
        }
    };

    match ask_agent(&state.cmd_tx, |response| AgentCommand::UpgradeRollback {
        version: request.version,
        response,
    })
    .await
    {
        Ok(Ok(())) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "rolling back" })),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => agent_unavailable(),
    }
}

/// Send one command to the agent loop and wait for its reply.
///
/// `build` receives the reply half of a fresh oneshot channel and returns the
/// command that carries it. If the agent loop has gone away, either before it
/// accepts the command or before it answers, the error is the 500 response the
/// handlers return for that case.
// `Response` is large, but it is the reply the handler sends as-is.
#[allow(clippy::result_large_err)]
async fn ask_agent<T>(
    cmd_tx: &mpsc::Sender<AgentCommand>,
    build: impl FnOnce(oneshot::Sender<T>) -> AgentCommand,
) -> Result<T, Response> {
    let (response, reply) = oneshot::channel();
    if cmd_tx.send(build(response)).await.is_err() {
        return Err(internal_error("agent unavailable"));
    }
    reply
        .await
        .map_err(|_| internal_error("agent dropped response"))
}

fn internal_error(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

fn agent_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": "agent unavailable" })),
    )
        .into_response()
}

/// A node in a cluster upgrade start request.
#[derive(serde::Deserialize)]
struct StartUpgradeNode {
    node_id: String,
    /// The node's bun API address (`host:port`).
    address: String,
    role: crate::upgrade::types::NodeRole,
}

/// Start a cluster-wide rolling upgrade (admin, leader only).
///
/// The caller (relish) has already pushed the binary blob to the leader's
/// Pickle registry; this handler records the plan in Raft and the
/// orchestrator loop takes it from there.
async fn upgrade_start_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(auth.as_deref()) {
        return resp;
    }
    #[derive(serde::Deserialize)]
    struct StartRequest {
        target_version: crate::upgrade::BinaryVersion,
        binary_sha256: String,
        embedded_signature: String,
        #[serde(default)]
        external_signature: Option<String>,
        #[serde(default = "default_parallel")]
        parallel: u32,
        /// Registry the nodes fetch the binary from (the leader's Pickle).
        registry_address: String,
        nodes: Vec<StartUpgradeNode>,
        #[serde(default)]
        direction: Option<crate::upgrade::types::UpgradeDirection>,
        /// Allow a target older than what the nodes run.
        #[serde(default)]
        allow_downgrade: bool,
    }
    fn default_parallel() -> u32 {
        1
    }

    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "cluster upgrades need a council (cluster mode)" })),
        )
            .into_response();
    };
    let request: StartRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid request: {e}") })),
            )
                .into_response();
        }
    };
    if request.nodes.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "nodes list must not be empty" })),
        )
            .into_response();
    }

    // Derive each node's authoritative role + address server-side from
    // gossip membership and the Raft voter set, then validate the client's
    // claims against it (UPG2). A caller cannot upgrade a node under a
    // false identity: an unknown node, a spoofed role or a mismatched
    // address is rejected here rather than trusted into the plan.
    let authoritative = match build_authoritative_view(&state, council).await {
        Ok(view) => view,
        Err(resp) => return resp,
    };
    let requested: Vec<crate::upgrade::plan::RequestedNode> = request
        .nodes
        .iter()
        .map(|node| crate::upgrade::plan::RequestedNode {
            node_id: node.node_id.clone(),
            address: node.address.clone(),
            role: node.role,
        })
        .collect();
    let derived_nodes = match crate::upgrade::plan::derive_upgrade_nodes(&requested, |id| {
        authoritative.get(id).cloned()
    }) {
        Ok(nodes) => nodes,
        Err(e) => return plan_error_response(&e),
    };

    if let Some(active) = council.desired_state().await.active_upgrade {
        return upgrade_in_progress(&active);
    }

    // Refuse same-version and unrequested downgrades before anything is
    // recorded: once in Raft, a same-version run would "complete" without
    // swapping a single byte.
    let (running, readiness) = probe_running_binaries(&state, &derived_nodes).await;
    let direction = request
        .direction
        .unwrap_or(crate::upgrade::types::UpgradeDirection::Upgrade);
    // Every node fetches an upgrade from Pickle and so demands the external
    // signature. A run the nodes will refuse would only pause and then block
    // every later start, so refuse it here instead.
    if direction == crate::upgrade::types::UpgradeDirection::Upgrade
        && let Err(e) = crate::upgrade::plan::check_network_prerequisites(
            request.external_signature.as_deref(),
            &readiness,
        )
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    match crate::upgrade::plan::check_target(
        &request.target_version,
        &request.binary_sha256,
        request.allow_downgrade,
        &running,
    ) {
        Ok(crate::upgrade::plan::TargetCheck::Proceed) => {}
        Ok(crate::upgrade::plan::TargetCheck::AlreadyRunning) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "already_running",
                    "detail": format!(
                        "every node already runs {} with this exact binary; nothing to do",
                        request.target_version
                    ),
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    let upgrade_id = format!(
        "up-{}-{}",
        request.target_version,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default()
    );
    let upgrade = crate::upgrade::types::ClusterUpgradeState {
        upgrade_id: upgrade_id.clone(),
        target_version: request.target_version,
        binary_sha256: request.binary_sha256,
        embedded_signature: request.embedded_signature,
        external_signature: request.external_signature,
        parallel: request.parallel.max(1),
        direction,
        phase: crate::upgrade::types::ClusterUpgradePhase::Preparing,
        registry_address: request.registry_address,
        allow_downgrade: request.allow_downgrade,
        nodes: derived_nodes,
    };

    match council
        .write(crate::council::types::RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade),
        })
        .await
    {
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "starting", "upgrade_id": upgrade_id })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("could not record the upgrade (are we the leader?): {e}")
            })),
        )
            .into_response(),
    }
}

/// Ask every planned node what it runs and whether it can verify a
/// network upgrade, for the start-time gates.
///
/// Probes run concurrently, each bounded. An unreachable node is left out:
/// the orchestrator re-checks every node as the walk reaches it.
async fn probe_running_binaries(
    state: &ApiState,
    nodes: &[crate::upgrade::types::NodeUpgradeRecord],
) -> (
    Vec<crate::upgrade::plan::RunningBinary>,
    Vec<crate::upgrade::plan::NetworkReadiness>,
) {
    use crate::upgrade::orchestrator::NodeControl as _;

    let control = crate::upgrade::orchestrator::HttpNodeControl::with_http(
        state.service_token.clone(),
        state.cluster_http.clone(),
    );
    let probes = nodes.iter().map(|record| {
        let control = &control;
        async move {
            let probe = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                control.probe(&record.address),
            )
            .await
            .ok()
            .flatten()?;
            let node = format!("node {}", record.node_id);
            Some((
                crate::upgrade::plan::RunningBinary {
                    node: node.clone(),
                    version: probe.version,
                    sha256: probe.binary_sha256,
                },
                crate::upgrade::plan::NetworkReadiness {
                    node,
                    accepts_network_upgrades: probe.accepts_network_upgrades,
                },
            ))
        }
    });
    futures_util::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .unzip()
}

/// The 409 for a start or rollback while another run is active. A paused
/// run says how to get out of it: resume, abort or roll back.
fn upgrade_in_progress(active: &crate::upgrade::types::ClusterUpgradeState) -> Response {
    let error = match &active.phase {
        crate::upgrade::types::ClusterUpgradePhase::Paused { reason } => format!(
            "upgrade {} is paused ({reason}); run `relish upgrade resume`, \
             `relish upgrade abort`, or `relish upgrade rollback <version>` first",
            active.upgrade_id
        ),
        _ => format!("upgrade {} is already in progress", active.upgrade_id),
    };
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "error": error })),
    )
        .into_response()
}

/// Archive a paused run that the operator ended: record it as `Aborted`,
/// then move it to history.
///
/// Two Raft writes. If the second is lost, the orchestrator archives the
/// aborted run on its next tick, and a start meanwhile gets a 409 that
/// names it.
// `Response` is large but it IS the HTTP reply to send on failure.
#[allow(clippy::result_large_err)]
async fn archive_aborted_upgrade(
    council: &crate::council::CouncilNode,
    aborted: crate::upgrade::types::ClusterUpgradeState,
) -> Result<(), Response> {
    let upgrade_id = aborted.upgrade_id.clone();
    let writes = [
        crate::council::types::RaftRequest::UpgradeUpdate {
            state: Box::new(aborted),
        },
        crate::council::types::RaftRequest::UpgradeClear { upgrade_id },
    ];
    for write in writes {
        if let Err(e) = council.write(write).await {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("could not end the paused upgrade (are we the leader?): {e}")
                })),
            )
                .into_response());
        }
    }
    Ok(())
}

/// Reply to a refused upgrade plan. A node whose endpoint the leader hasn't
/// heard yet is a 503 (retry shortly); a claim that contradicts the cluster
/// is the caller's fault, a 400.
fn plan_error_response(error: &crate::upgrade::plan::PlanError) -> Response {
    let status = if error.is_transient() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::BAD_REQUEST
    };
    (
        status,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}

/// Build the leader's authoritative view of every node for upgrade
/// planning (UPG2): node id → its API address (from gossip membership) and
/// role (from the Raft voter set + current leader). This is the source of
/// truth the client's start request is validated against.
///
/// The role comes from Raft: the current leader is `Leader`, other voters
/// are `Council`, and everything else `Worker`. Gossip identifies nodes by
/// name; the Raft voter set by `raft_id_from_name(name)`, so we bridge them
/// with that same stable hash.
// `Response` is large but it IS the HTTP reply to send on failure —
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
async fn build_authoritative_view(
    state: &ApiState,
    council: &Arc<crate::council::CouncilNode>,
) -> Result<std::collections::HashMap<String, crate::upgrade::plan::AuthoritativeNode>, Response> {
    use crate::cluster::identity::raft_id_from_name;
    use crate::upgrade::plan::AuthoritativeNode;

    let Some(membership) = &state.membership else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no gossip membership on this node" })),
        )
            .into_response());
    };

    let metrics = council.metrics().borrow().clone();
    let voters: std::collections::BTreeSet<u64> =
        metrics.membership_config.membership().voter_ids().collect();
    let leader_id = metrics.current_leader;

    let mut view = std::collections::HashMap::new();
    for member in membership.read().await.iter() {
        let name = member.node_id.0.clone();
        let raft_id = raft_id_from_name(&name);
        let role = crate::upgrade::plan::role_from_raft(raft_id, leader_id, &voters);
        view.insert(
            name,
            AuthoritativeNode {
                address: member.api_advertised.then(|| member.address.to_string()),
                role,
            },
        );
    }
    Ok(view)
}

/// Cluster upgrade state, readable from any node (it's replicated).
async fn upgrade_cluster_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return resp;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council on this node" })),
        )
            .into_response();
    };
    let desired = council.desired_state().await;
    Json(serde_json::json!({
        "active": desired.active_upgrade,
        "history": desired.upgrade_history,
    }))
    .into_response()
}

/// Un-pause a paused cluster upgrade (admin, leader only).
async fn upgrade_resume_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(auth.as_deref()) {
        return resp;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council on this node" })),
        )
            .into_response();
    };
    let Some(upgrade) = council.desired_state().await.active_upgrade else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no upgrade in progress" })),
        )
            .into_response();
    };
    if !matches!(
        upgrade.phase,
        crate::upgrade::types::ClusterUpgradePhase::Paused { .. }
    ) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "the upgrade is not paused" })),
        )
            .into_response();
    }

    let resumed = crate::upgrade::orchestrator::resume(upgrade);
    match council
        .write(crate::council::types::RaftRequest::UpgradeUpdate {
            state: Box::new(resumed),
        })
        .await
    {
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "resumed" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// End a paused cluster upgrade in which no node moved (admin, leader
/// only). A run that already swapped nodes is refused with a pointer to
/// `relish upgrade rollback`, which walks them back.
async fn upgrade_abort_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(auth.as_deref()) {
        return resp;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council on this node" })),
        )
            .into_response();
    };
    let Some(upgrade) = council.desired_state().await.active_upgrade else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no upgrade in progress" })),
        )
            .into_response();
    };
    let upgrade_id = upgrade.upgrade_id.clone();
    let aborted = match crate::upgrade::orchestrator::abort(upgrade, "aborted by the operator") {
        Ok(aborted) => aborted,
        Err(e) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    if let Err(resp) = archive_aborted_upgrade(council, aborted).await {
        return resp;
    }
    Json(serde_json::json!({ "status": "aborted", "upgrade_id": upgrade_id })).into_response()
}

/// Start a cluster-wide rolling rollback (admin, leader only). The
/// binaries are already on every node's disk, so there is no registry or
/// signature material — just a target version and the node list.
///
/// A paused run is replaced: it is archived as aborted and the rollback
/// walks every node, moved or not, to the target.
async fn upgrade_cluster_rollback_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(auth.as_deref()) {
        return resp;
    }
    #[derive(serde::Deserialize)]
    struct RollbackRequest {
        target_version: crate::upgrade::BinaryVersion,
        nodes: Vec<StartUpgradeNode>,
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council on this node" })),
        )
            .into_response();
    };
    let request: RollbackRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid request: {e}") })),
            )
                .into_response();
        }
    };
    let paused = match council.desired_state().await.active_upgrade {
        None => None,
        Some(active) => match crate::upgrade::orchestrator::supersede(
            active.clone(),
            &format!("replaced by a rollback to {}", request.target_version),
        ) {
            Ok(superseded) => Some(superseded),
            Err(_) => return upgrade_in_progress(&active),
        },
    };

    // Validate each rollback node's identity against the authoritative gossip /
    // Raft view, exactly as upgrade_start does (M13/UPG2). The old rollback path
    // copied client-supplied node_id/address/role straight into the replicated
    // plan, so a caller could point the orchestrator at spoofed addresses or
    // roles that UPG2 exists to reject.
    let authoritative = match build_authoritative_view(&state, council).await {
        Ok(view) => view,
        Err(resp) => return resp,
    };
    let requested: Vec<crate::upgrade::plan::RequestedNode> = request
        .nodes
        .iter()
        .map(|node| crate::upgrade::plan::RequestedNode {
            node_id: node.node_id.clone(),
            address: node.address.clone(),
            role: node.role,
        })
        .collect();
    let derived_nodes = match crate::upgrade::plan::derive_upgrade_nodes(&requested, |id| {
        authoritative.get(id).cloned()
    }) {
        Ok(nodes) => nodes,
        Err(e) => return plan_error_response(&e),
    };

    let upgrade_id = format!(
        "rollback-{}-{}",
        request.target_version,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default()
    );
    let upgrade = crate::upgrade::types::ClusterUpgradeState {
        upgrade_id: upgrade_id.clone(),
        target_version: request.target_version,
        binary_sha256: String::new(),
        embedded_signature: String::new(),
        external_signature: None,
        parallel: 1,
        direction: crate::upgrade::types::UpgradeDirection::Rollback,
        phase: crate::upgrade::types::ClusterUpgradePhase::Preparing,
        registry_address: String::new(),
        allow_downgrade: false,
        nodes: derived_nodes,
    };

    // Archive the paused run only once the rollback plan is valid, so a
    // malformed request leaves it where it was.
    if let Some(superseded) = paused
        && let Err(resp) = archive_aborted_upgrade(council, superseded).await
    {
        return resp;
    }

    match council
        .write(crate::council::types::RaftRequest::UpgradeUpdate {
            state: Box::new(upgrade),
        })
        .await
    {
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "rolling back", "upgrade_id": upgrade_id })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Ask this node to call a Raft election on itself (admin; manual
/// recovery tool — e.g. to move leadership off a node before maintenance).
async fn cluster_elect_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(auth.as_deref()) {
        return resp;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council on this node" })),
        )
            .into_response();
    };
    match council.raft().trigger().elect().await {
        Ok(()) => Json(serde_json::json!({ "status": "election triggered" })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Enforce a principal's `[permission]` spec for a per-app action.
///
/// Reads the replicated permission map from the council (empty when there is no
/// council, e.g. single-node mode, where permissions cannot be configured) and
/// defers to [`crate::sesame::auth::authorize_permission`]. Call it after the
/// role and scope checks in a gated handler.
// `Response` is large but it IS the HTTP reply to send on failure —
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
async fn enforce_permission(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    action: crate::config::PermissionAction,
    app: &str,
    namespace: &str,
) -> Result<(), Response> {
    let permissions = match &state.council {
        Some(council) => council.desired_state().await.permissions,
        None => std::collections::BTreeMap::new(),
    };
    crate::sesame::auth::authorize_permission(auth, action, app, namespace, &permissions)
}

/// Deploy workloads, streaming progress via SSE.
///
/// Returns a Server-Sent Events stream. Each event's `data` field
/// contains a JSON-serialised `ApplyEvent`. The stream ends after
/// the `Complete` or `Error` event.
const CAPACITY_PROBE_HEADER: &str = "x-reliaburger-capacity-probe";

async fn apply_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    capacity_admission: Option<axum::Extension<crate::cluster::capacity::CapacityAdmission>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    let mut config = match Config::parse(&body) {
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

    let rerun_jobs = match headers.get("x-reliaburger-rerun-jobs") {
        None => false,
        Some(value) if value.as_bytes() == b"acknowledged" => true,
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "x-reliaburger-rerun-jobs must equal acknowledged",
            )
                .into_response();
        }
    };
    if rerun_jobs {
        if let Err(error) = crate::bun::jobs::validate_rerun(&config) {
            return (StatusCode::BAD_REQUEST, error).into_response();
        }
        if let Err(response) = crate::sesame::auth::authorize_user(
            auth.as_deref(),
            crate::sesame::types::ApiRole::Deployer,
        ) {
            return response;
        }
    }

    let lease_id = match headers.get("x-reliaburger-test-lease") {
        Some(value) => match value.to_str() {
            Ok(value) if !value.is_empty() => Some(value.to_string()),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    "x-reliaburger-test-lease must contain a lease id",
                )
                    .into_response();
            }
        },
        None => None,
    };
    let capacity_probe = match headers.get(CAPACITY_PROBE_HEADER) {
        Some(value) if value.as_bytes() == b"acknowledged" => true,
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "x-reliaburger-capacity-probe must equal acknowledged",
            )
                .into_response();
        }
        None => false,
    };
    if capacity_probe && lease_id.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "capacity probe requires x-reliaburger-test-lease",
        )
            .into_response();
    }
    if capacity_probe {
        if config.app.len() != 1
            || !config.job.is_empty()
            || !config.namespace.is_empty()
            || !config.permission.is_empty()
            || !config.build.is_empty()
            || config
                .app
                .values()
                .any(|spec| spec.replicas != crate::config::Replicas::Fixed(1))
        {
            return (
                StatusCode::BAD_REQUEST,
                "capacity probe requires exactly one new app with one replica",
            )
                .into_response();
        }
        let Some(auth) = auth.as_deref() else {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        };
        if let Err(error) = state.static_capabilities.test_policy.authorise(
            crate::testkit::safety::OperationPermission::SaturateCapacity,
            &crate::testkit::safety::OperationAuthorisation {
                principal: &auth.token_name,
                role: auth.role,
                acknowledged: true,
            },
        ) {
            return (StatusCode::FORBIDDEN, error.to_string()).into_response();
        }
    }
    let mut lease_owner_id = None;
    let mut image_lease = None;
    if let Some(lease_id) = &lease_id {
        let Some(auth) = auth.as_deref() else {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        };
        let Some(lease) = find_test_lease(&state, lease_id).await else {
            return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
        };
        let wrong_kind = match lease.scope {
            LeaseScope::Applications => !config.job.is_empty(),
            LeaseScope::NodeJobs => {
                config.job.is_empty() || !config.app.is_empty() || !config.namespace.is_empty()
            }
        };
        if wrong_kind || !config.permission.is_empty() || !config.build.is_empty() {
            return lease_error_response(crate::testkit::lease::LeaseError::InvalidScope);
        }
        if auth.token_name != crate::sesame::auth::SYSTEM_PRINCIPAL
            && lease.owner_id != auth.principal_id
        {
            return lease_error_response(crate::testkit::lease::LeaseError::WrongOwner);
        }
        if !lease.is_active_at(crate::testkit::lease::now_unix_millis()) {
            return lease_error_response(crate::testkit::lease::LeaseError::NotActive);
        }
        if config
            .namespace
            .keys()
            .any(|namespace| namespace != &lease.namespace)
        {
            return lease_error_response(crate::testkit::lease::LeaseError::NamespaceMismatch);
        }
        for spec in config.app.values_mut() {
            match &spec.namespace {
                Some(namespace) if namespace != &lease.namespace => {
                    return lease_error_response(
                        crate::testkit::lease::LeaseError::NamespaceMismatch,
                    );
                }
                Some(_) => {}
                None => spec.namespace = Some(lease.namespace.clone()),
            }
        }
        for spec in config.job.values_mut() {
            match &spec.namespace {
                Some(namespace) if namespace != &lease.namespace => {
                    return lease_error_response(
                        crate::testkit::lease::LeaseError::NamespaceMismatch,
                    );
                }
                Some(_) => {}
                None => spec.namespace = Some(lease.namespace.clone()),
            }
        }
        lease_owner_id = Some(lease.owner_id.clone());
        image_lease = Some(lease);
    } else {
        if config
            .namespace
            .keys()
            .any(|namespace| crate::testkit::lease::valid_test_namespace(namespace))
        {
            return (
                StatusCode::CONFLICT,
                "test lease namespace requires x-reliaburger-test-lease",
            )
                .into_response();
        }
        for namespace in config
            .app
            .values()
            .map(|spec| spec.namespace.as_deref())
            .chain(config.job.values().map(|spec| spec.namespace.as_deref()))
        {
            let namespace = namespace.unwrap_or("default");
            if crate::testkit::lease::valid_test_namespace(namespace) {
                return (
                    StatusCode::CONFLICT,
                    "test lease namespace requires x-reliaburger-test-lease",
                )
                    .into_response();
            }
        }
    }

    // Ordinary namespace quotas and permission grants are operator policy.
    // A test lease has already confined its namespace declaration to the
    // caller-owned reservation above; its quota cannot affect other tenants.
    if !config.permission.is_empty() || (lease_id.is_none() && !config.namespace.is_empty()) {
        if let Err(response) = crate::sesame::auth::authorize_user(
            auth.as_deref(),
            crate::sesame::types::ApiRole::Admin,
        ) {
            return response;
        }
        if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
            return response;
        }
    }

    let images = config
        .app
        .values()
        .flat_map(crate::config::AppSpec::image_references)
        .chain(config.job.values().filter_map(|job| job.image.as_deref()));
    if let Err(error) = crate::testkit::lease::authorise_image_references(
        images,
        image_lease
            .as_ref()
            .map(|lease| (lease, crate::testkit::lease::now_unix_millis())),
    ) {
        return lease_error_response(error);
    }

    // Check every workload before any Raft write or agent command. A job in
    // a mixed manifest must not bypass admission after its apps have committed.
    // Host execution includes both explicit binaries and inline scripts.
    let permissions = match &state.council {
        Some(council) => council.desired_state().await.permissions,
        None => std::collections::BTreeMap::new(),
    };
    let targets = config
        .app
        .iter()
        .map(|(name, spec)| {
            (
                name.as_str(),
                spec.namespace.as_deref().unwrap_or("default"),
                spec.script.is_some() || spec.exec.is_some(),
            )
        })
        .chain(config.job.iter().map(|(name, spec)| {
            (
                name.as_str(),
                spec.namespace.as_deref().unwrap_or("default"),
                spec.script.is_some() || spec.exec.is_some(),
            )
        }));
    for (app_name, namespace, host_execution) in targets {
        if let Err(resp) =
            crate::sesame::auth::authorize_scoped(auth.as_deref(), app_name, namespace)
        {
            return resp;
        }
        if let Err(resp) = crate::sesame::auth::authorize_permission(
            auth.as_deref(),
            crate::config::PermissionAction::Deploy,
            app_name,
            namespace,
            &permissions,
        ) {
            return resp;
        }
        if host_execution
            && let Err(resp) = crate::sesame::auth::authorize_permission(
                auth.as_deref(),
                crate::config::PermissionAction::HostExec,
                app_name,
                namespace,
                &permissions,
            )
        {
            return resp;
        }
    }

    // Cluster mode (L1): apps, namespaces and permissions become desired
    // state in Raft; the leader schedules apps and every node's reconciler
    // converges. Jobs stay on the receiving node (cluster-wide job
    // scheduling is later work). A namespace/permission-only config still
    // routes through the cluster path so its resources are committed.
    if let Some(council) = &state.council
        && (!config.app.is_empty() || !config.namespace.is_empty() || !config.permission.is_empty())
    {
        return cluster_apply(
            state.clone(),
            Arc::clone(council),
            config,
            body,
            lease_id,
            headers,
            capacity_admission.map(|extension| extension.0),
        )
        .await;
    }
    if capacity_probe {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "capacity admission requires a live cluster scheduler",
        )
            .into_response();
    }

    let lease_operation = if let (Some(lease_id), Some(owner_id)) =
        (&lease_id, lease_owner_id.as_deref())
    {
        let now = crate::testkit::lease::now_unix_millis();
        let result = if is_node_job_lease(lease_id) {
            let job_ids = config
                .job
                .iter()
                .map(|(name, spec)| {
                    crate::meat::AppId::new(name, spec.namespace.as_deref().unwrap_or("default"))
                })
                .collect();
            state
                .local_test_leases
                .begin_job_operation(lease_id, owner_id, job_ids, now)
                .await
        } else {
            let app_ids = config
                .app
                .iter()
                .map(|(name, spec)| {
                    crate::meat::AppId::new(name, spec.namespace.as_deref().unwrap_or("default"))
                })
                .collect();
            state
                .local_test_leases
                .begin_app_operation(lease_id, owner_id, app_ids, now)
                .await
        };
        match result {
            Ok(operation) => Some(operation),
            Err(error) => return lease_error_response(error),
        }
    } else {
        None
    };

    let (agent_event_tx, mut agent_event_rx) = mpsc::channel::<ApplyEvent>(32);
    let command = if rerun_jobs {
        AgentCommand::RerunJobs {
            config,
            events: agent_event_tx,
        }
    } else {
        AgentCommand::Deploy {
            config,
            events: agent_event_tx,
        }
    };
    if state.cmd_tx.send(command).await.is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent unavailable" })),
        )
            .into_response();
    }

    let event_rx = if let Some(operation) = lease_operation {
        let (client_event_tx, client_event_rx) = mpsc::channel::<ApplyEvent>(32);
        // Keep consuming agent progress even when the HTTP client disconnects.
        // The per-lease guard prevents expiry cleanup from overtaking a deploy
        // which the agent has accepted but not completed yet.
        tokio::spawn(async move {
            let mut operation = Some(operation);
            while let Some(event) = agent_event_rx.recv().await {
                let terminal = matches!(
                    event,
                    ApplyEvent::Complete { .. } | ApplyEvent::Error { .. }
                );
                match client_event_tx.try_send(event) {
                    Ok(()) => {
                        if terminal {
                            operation.take();
                        }
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Full(event)) if terminal => {
                        // The deploy is over, so cleanup may proceed even if a
                        // slow client still needs time to accept its terminal
                        // event. Progress events may be coalesced under this
                        // backpressure, but the outcome is never dropped.
                        operation.take();
                        let _ = client_event_tx.send(event).await;
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
                }
            }
        });
        client_event_rx
    } else {
        agent_event_rx
    };

    let stream = ReceiverStream::new(event_rx).map(|apply_event| {
        let json = serde_json::to_string(&apply_event).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(Event::default().data(json))
    });

    Sse::new(stream).into_response()
}

/// Apply a config in cluster mode: propose each app spec to Raft.
///
/// On a follower, the whole request is forwarded to the leader's API
/// (openraft does not forward client writes), streaming its SSE
/// response back verbatim. Jobs in the same config still deploy on the
/// receiving node after the specs commit.
async fn cluster_apply(
    state: ApiState,
    council: Arc<crate::council::CouncilNode>,
    config: Config,
    raw_body: String,
    lease_id: Option<String>,
    caller_headers: HeaderMap,
    capacity_admission: Option<crate::cluster::capacity::CapacityAdmission>,
) -> Response {
    // Follower? Forward to the leader rather than half-failing.
    if !council.is_leader().await {
        let Some(leader_url) = leader_api_url(&state, &council).await else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "no cluster leader known yet; retry shortly"
                })),
            )
                .into_response();
        };
        let mut request = state
            .cluster_http
            .client()
            .post(format!("{leader_url}/v1/apply"))
            .body(raw_body);
        if let Some(lease_id) = &lease_id {
            request = request.header("x-reliaburger-test-lease", lease_id);
            if let Some(value) = caller_headers.get(CAPACITY_PROBE_HEADER) {
                request = request.header(CAPACITY_PROBE_HEADER, value.as_bytes());
            }
        }
        // The leader must evaluate the user's current grants, not the
        // follower's internal service identity. ClusterHttp has no default
        // bearer; node-to-node requests attach theirs explicitly.
        request = copy_forwarded_auth(request, &caller_headers);
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(5), request.send()).await;
        return match response {
            Ok(Ok(response)) => {
                let mut builder = Response::builder().status(response.status());
                if let Some(content_type) = response.headers().get(axum::http::header::CONTENT_TYPE)
                {
                    builder = builder.header(axum::http::header::CONTENT_TYPE, content_type);
                }
                builder
                    .body(axum::body::Body::from_stream(response.bytes_stream()))
                    .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
            }
            Err(_) => (
                StatusCode::GATEWAY_TIMEOUT,
                "leader apply request timed out",
            )
                .into_response(),
            Ok(Err(e)) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": format!("failed to forward apply to the leader: {e}")
                })),
            )
                .into_response(),
        };
    }

    // Permissions and builds may target a namespace an earlier apply
    // created, not just one in this file. Validate against the union of
    // this config's namespaces and those already committed, so a build
    // scoped to an existing namespace validates and one targeting a ghost
    // namespace is rejected before any write lands.
    let known_namespaces: Vec<String> = council
        .desired_state()
        .await
        .namespaces
        .keys()
        .cloned()
        .collect();
    if let Err(e) = config.validate_against(&known_namespaces) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    if caller_headers.contains_key(CAPACITY_PROBE_HEADER) {
        use crate::cluster::capacity::{CapacityAdmissionError, SchedulingRefusal};
        let Some(admission) = capacity_admission else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "capacity admission is unavailable",
            )
                .into_response();
        };
        let Some((name, spec)) = config.app.iter().next() else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let app_id = crate::meat::AppId::new(name, spec.namespace.as_deref().unwrap_or("default"));
        let outcome = admission.check(&app_id, spec).await;
        if !council.is_leader().await {
            return unavailable_response("leadership changed during capacity admission".into());
        }
        let active_lease = council.desired_state().await.test_leases;
        if !lease_id
            .as_ref()
            .and_then(|id| active_lease.get(id))
            .is_some_and(|lease| lease.is_active_at(crate::testkit::lease::now_unix_millis()))
        {
            return lease_error_response(crate::testkit::lease::LeaseError::NotActive);
        }
        match outcome {
            Ok(()) => {}
            Err(CapacityAdmissionError::Rejected(error)) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(SchedulingRefusal { error }),
                )
                    .into_response();
            }
            Err(error) => return unavailable_response(error.to_string()),
        }
    }

    let (event_tx, event_rx) = mpsc::channel::<ApplyEvent>(32);
    let cmd_tx = state.cmd_tx.clone();
    tokio::spawn(async move {
        let mut committed = 0usize;
        // The one shared path: namespaces, then permissions, then apps.
        // Lettuce writes the exact same set for the same config, so manual
        // apply and GitOps can't diverge (12b.2 T6). A failed write is a
        // hard stop — half an apply leaves desired state inconsistent.
        let writes = match &lease_id {
            Some(lease_id) => match crate::council::config_to_leased_writes(
                &config,
                lease_id,
                crate::testkit::lease::now_unix_millis(),
            ) {
                Ok(writes) => writes,
                // A leased apply that declares a non-owned kind (job, build,
                // permission) is rejected outright rather than silently
                // dropping it — see `config_to_leased_writes`.
                Err(e) => {
                    let _ = event_tx
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }
            },
            None => crate::council::config_to_desired_writes(&config),
        };
        for request in writes {
            let describe = describe_write(&request);
            match council.write(request).await {
                // A state-machine refusal (lease expired, in cleanup, resource
                // owned elsewhere, quota) is NOT a commit — surfacing it as an
                // error stops the apply instead of streaming "committed" and
                // letting the case die later as Unknown(TimedOut).
                Ok(crate::council::CouncilResponse::Refused { reason }) => {
                    let _ = event_tx
                        .send(ApplyEvent::Error {
                            message: format!("{describe}: refused by the cluster: {reason}"),
                        })
                        .await;
                    return;
                }
                Ok(_) => {
                    committed += 1;
                    let _ = event_tx
                        .send(ApplyEvent::Progress {
                            message: format!("{describe}: committed to the cluster"),
                        })
                        .await;
                }
                Err(e) => {
                    let _ = event_tx
                        .send(ApplyEvent::Error {
                            message: format!("{describe}: raft write failed: {e}"),
                        })
                        .await;
                    return;
                }
            }
        }

        // Jobs are not cluster-scheduled yet; run them here, as before.
        if !config.job.is_empty() {
            let _ = event_tx
                .send(ApplyEvent::Progress {
                    message: format!(
                        "{} job(s) deploying on this node (jobs are not cluster-scheduled yet)",
                        config.job.len()
                    ),
                })
                .await;
            let job_config = Config {
                job: config.job.clone(),
                ..Config::default()
            };
            let _ = cmd_tx
                .send(AgentCommand::Deploy {
                    config: job_config,
                    events: event_tx.clone(),
                })
                .await;
            // The agent sends Complete/Error for the job deploy.
            return;
        }

        let _ = event_tx
            .send(ApplyEvent::Complete {
                created: committed,
                instances: vec![],
            })
            .await;
    });

    let stream = ReceiverStream::new(event_rx).map(|apply_event| {
        let json = serde_json::to_string(&apply_event).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(Event::default().data(json))
    });
    Sse::new(stream).into_response()
}

/// A short human-readable label for an apply progress message.
fn describe_write(request: &crate::council::types::RaftRequest) -> String {
    use crate::council::types::RaftRequest;
    match request {
        RaftRequest::AppSpec { app_id, .. } => format!("app {}", app_id.name),
        RaftRequest::NamespaceSpec { name, .. } => format!("namespace {name}"),
        RaftRequest::PermissionSpec { name, .. } => format!("permission {name}"),
        RaftRequest::TestLeaseAppSpec { app_id, .. } => {
            format!("leased app {}", app_id.name)
        }
        RaftRequest::TestLeaseNamespaceSpec { name, .. } => {
            format!("leased namespace {name}")
        }
        _ => "resource".to_string(),
    }
}

/// Resolve the current leader's API base URL.
///
/// Preferred source is the gossip-fed membership table (it stores real
/// per-node API addresses); the fallback derives from the leader's
/// raft IP and this node's own API port, which is correct only when
/// ports are uniform across the cluster.
pub(crate) async fn leader_api_url(
    state: &ApiState,
    council: &crate::council::CouncilNode,
) -> Option<String> {
    let leader_id = council.current_leader().await?;
    let (leader_name, leader_ip) = {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        let info = metrics
            .membership_config
            .membership()
            .get_node(&leader_id)?;
        (info.name.clone(), info.addr.ip())
    };

    if let Some(membership) = &state.membership {
        let members = membership.read().await;
        if let Some(info) = members
            .iter()
            .find(|m| m.node_id == crate::meat::NodeId::new(&leader_name))
        {
            return Some(state.cluster_http.url(&info.address.to_string(), ""));
        }
    }

    Some(
        state
            .cluster_http
            .url(&format!("{leader_ip}:{}", state.api_port), ""),
    )
}

/// Retire an identity only on an explicit, authenticated operator attestation.
async fn node_decommission_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<crate::cluster::retirement::DecommissionRequest>,
) -> Response {
    use crate::council::{CouncilResponse, RaftRequest};
    let Some(auth) = auth.as_deref() else {
        return (
            StatusCode::UNAUTHORIZED,
            "an authenticated operator is required",
        )
            .into_response();
    };
    if let Err(response) =
        crate::sesame::auth::authorize_user(Some(auth), crate::sesame::types::ApiRole::Admin)
    {
        return response;
    }
    if let Err(response) = crate::sesame::auth::require_unscoped(Some(auth)) {
        return response;
    }
    if let Err(error) = request.validate() {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "decommissioning requires a cluster council",
        )
            .into_response();
    };
    if !confirmed_lease_leader(council).await {
        return forward_test_lease_request(
            &state,
            council,
            reqwest::Method::POST,
            "/v1/nodes/decommission",
            &headers,
            Some(&request),
        )
        .await;
    }
    let (is_self, membership_log_id) = {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        (
            metrics
                .membership_config
                .membership()
                .get_node(&metrics.id)
                .is_some_and(|node| node.name == request.node_id),
            *metrics.membership_config.log_id(),
        )
    };
    if is_self {
        return (
            StatusCode::CONFLICT,
            "stop or fence the target and retry through a surviving leader",
        )
            .into_response();
    }
    let write = council.write(RaftRequest::DecommissionNode {
        node_id: request.node_id,
        retired_by: auth.principal_id.clone(),
        reason: request.reason,
        retired_at_unix_ms: crate::testkit::lease::now_unix_millis(),
        membership_log_id,
    });
    match tokio::time::timeout(std::time::Duration::from_secs(10), write).await {
        Ok(Ok(CouncilResponse::NodeDecommissioned { retirement })) => {
            Json(retirement).into_response()
        }
        Ok(Ok(CouncilResponse::Refused { reason })) => {
            (StatusCode::CONFLICT, reason).into_response()
        }
        Ok(Ok(_)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected decommission response",
        )
            .into_response(),
        Ok(Err(error)) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "decommission outcome unknown; repeat the same request",
        )
            .into_response(),
    }
}

/// Existing TLS connections must observe an identity retirement too.
async fn refuse_retired_tls_peer(
    State(state): State<ApiState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let (Some(council), Some(peer)) = (
        &state.council,
        request
            .extensions()
            .get::<crate::sesame::renewal::TlsPeerCertificate>(),
    ) {
        // An identity we can't read might belong to a retired node, so refuse it.
        let Ok(uris) = crate::sesame::cert::subject_uri_sans(&peer.0) else {
            return (
                StatusCode::FORBIDDEN,
                "peer certificate identity is unreadable",
            )
                .into_response();
        };
        let mut retired = false;
        for node in uris
            .iter()
            .filter_map(|uri| crate::sesame::ca::node_id_from_spiffe_uri(uri))
        {
            retired |= council.is_node_retired(node).await;
        }
        if retired {
            return (
                StatusCode::FORBIDDEN,
                "node identity is retired; fresh enrolment is required",
            )
                .into_response();
        }
    }
    next.run(request).await
}

/// Receipts must reach the leader directly, preserving the consumer's TLS identity.
async fn endpoint_withdrawal_receipt_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    State(state): State<ApiState>,
    Json(receipt): Json<crate::onion::withdrawal::EndpointWithdrawalReceipt>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    if let Err(error) = receipt.compatibility.require_current() {
        return (StatusCode::CONFLICT, error.to_string()).into_response();
    }
    let Some(peer) = peer else {
        return (
            StatusCode::FORBIDDEN,
            "endpoint receipts require a TLS node certificate",
        )
            .into_response();
    };
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no endpoint council available",
        )
            .into_response();
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let security = council
            .security_state_linearizable()
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
        let node_id = crate::sesame::renewal::validate_peer(&peer, &security)
            .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        council
            .write(crate::council::RaftRequest::AcknowledgeEndpointWithdrawal {
                node_id,
                generation: receipt.generation,
            })
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))
    })
    .await;
    match result {
        Ok(Ok(crate::council::CouncilResponse::Applied { .. })) => {
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Ok(crate::council::CouncilResponse::Refused { reason })) => {
            (StatusCode::CONFLICT, reason).into_response()
        }
        Ok(Ok(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "endpoint receipt is unconfirmed",
        )
            .into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "endpoint receipt outcome unknown; repeat the same receipt",
        )
            .into_response(),
    }
}

/// Producers contact the leader directly so forwarding cannot replace their TLS identity.
/// `POST /v1/cluster/workload-csr` — sign a workload CSR for a follower.
///
/// Only the leader can sign (the CA read is linearised and the serial comes
/// from Raft). The caller is identified by its node certificate, and the
/// SPIFFE identity is derived from the instance id, which must belong to an
/// app scheduled on that node.
async fn workload_csr_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    State(state): State<ApiState>,
    Json(request): Json<crate::cluster::workload_identity::WorkloadCsrRequest>,
) -> Response {
    use crate::cluster::workload_identity::{SignedWorkload, WorkloadCsrResponse, authorise};
    use base64::Engine as _;
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    if let Err(error) = request.compatibility.require_current() {
        return (StatusCode::CONFLICT, error.to_string()).into_response();
    }
    let Some(peer) = peer else {
        return (
            StatusCode::FORBIDDEN,
            "workload signing requires a TLS node certificate",
        )
            .into_response();
    };
    let Some(council) = &state.council else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no council available").into_response();
    };
    let Ok(csr_der) = base64::engine::general_purpose::STANDARD.decode(&request.csr_der) else {
        return (StatusCode::BAD_REQUEST, "workload CSR is not base64").into_response();
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let security = council
            .security_state_linearizable()
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
        let node_id = crate::sesame::renewal::validate_peer(&peer, &security)
            .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        let desired = council.desired_state().await;
        let (namespace, name) = authorise(
            &desired,
            &node_id,
            &request.instance_id,
            request.workload_type,
        )
        .map_err(|reason| (StatusCode::FORBIDDEN, reason))?;
        let spiffe_uri = crate::bun::agent::workload_spiffe_uri(
            &state.trust_domain,
            &namespace,
            &name,
            request.workload_type,
        );
        council
            .sign_workload_csr(
                &csr_der,
                &spiffe_uri,
                crate::sesame::identity::CertUsage::Mtls,
                &state.trust_domain,
                &node_id,
                &request.instance_id,
            )
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))
    })
    .await;
    match result {
        Ok(Ok(signed)) => Json(WorkloadCsrResponse::encode(&SignedWorkload {
            cert_der: signed.cert_der,
            workload_ca_cert_der: signed.workload_ca_cert_der,
            root_ca_cert_der: signed.root_ca_cert_der,
            jwt_token: signed.jwt_token,
        }))
        .into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "workload signing timed out").into_response(),
    }
}

async fn producer_retirement_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    State(state): State<ApiState>,
    Json(receipt): Json<crate::onion::producer::ProducerRetirementRequest>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    if let Err(error) = receipt.compatibility.require_current() {
        return (StatusCode::CONFLICT, error.to_string()).into_response();
    }
    let Some(peer) = peer else {
        return (
            StatusCode::FORBIDDEN,
            "producer retirement requires a TLS node certificate",
        )
            .into_response();
    };
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no endpoint council available",
        )
            .into_response();
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let security = council
            .security_state_linearizable()
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
        let node_id = crate::sesame::renewal::validate_peer(&peer, &security)
            .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        council
            .write(crate::council::RaftRequest::RetireEndpointExecution {
                node_id: node_id.clone(),
                execution: receipt.execution.clone(),
            })
            .await
            .map(|response| (node_id, response))
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))
    })
    .await;
    match result {
        Ok(Ok((
            node_id,
            crate::council::CouncilResponse::EndpointExecutionRetired { released: true },
        ))) => Json(crate::onion::producer::ProducerReleaseConfirmation {
            node_id,
            execution: receipt.execution,
        })
        .into_response(),
        Ok(Ok((
            _,
            crate::council::CouncilResponse::EndpointExecutionRetired { released: false },
        ))) => StatusCode::ACCEPTED.into_response(),
        Ok(Ok((_, crate::council::CouncilResponse::Refused { reason }))) => {
            (StatusCode::CONFLICT, reason).into_response()
        }
        Ok(Ok(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "producer retirement is unconfirmed",
        )
            .into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "producer retirement outcome unknown; repeat the same retirement",
        )
            .into_response(),
    }
}

/// `GET /v1/placements/{node_id}` — the apps (and per-node replica
/// counts) the leader has assigned to a node. Served from the Raft
/// state machine; reconcilers poll this every couple of seconds.
async fn placements_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    State(state): State<ApiState>,
    Path(node_id): Path<String>,
) -> Response {
    // Credential-free development clusters already expose placements. Adding
    // a retained consumer cannot authorise cleanup; receipt endpoints must
    // separately authenticate permission to discharge that obligation.
    let development_without_credentials = auth.is_none()
        && state.service_token.is_none()
        && state.cluster_http.scheme() == "http"
        && match &state.token_store {
            Some(tokens) => tokens.read().await.is_empty(),
            None => true,
        };
    if !development_without_credentials
        && let Err(response) = crate::sesame::auth::require_system(auth.as_deref())
    {
        return response;
    }
    if let Err(reason) = crate::cluster::retirement::validate_node_id(&node_id) {
        return (StatusCode::BAD_REQUEST, reason).into_response();
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "not running in cluster mode" })),
        )
            .into_response();
    };

    if !confirmed_lease_leader(council).await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "placements require a current leader",
        )
            .into_response();
    }
    let mut desired = council.desired_state().await;
    // Receipts must come from this same TLS identity. A plaintext consumer
    // could never send one, so registering it would only freeze discovery.
    let authenticated_consumer = peer.is_some();
    if let Some(peer) = peer {
        match crate::sesame::renewal::validate_peer(&peer, &desired.security_state) {
            Ok(identity) if identity == node_id => {}
            _ => {
                return (
                    StatusCode::FORBIDDEN,
                    "placement consumer does not match TLS identity",
                )
                    .into_response();
            }
        }
    } else if state.cluster_http.scheme() == "https" {
        return (
            StatusCode::FORBIDDEN,
            "placement consumers require a TLS node certificate",
        )
            .into_response();
    }
    if desired
        .security_state
        .crl
        .retired_nodes
        .contains_key(&node_id)
    {
        return (
            StatusCode::GONE,
            "node identity is retired; fresh enrolment is required",
        )
            .into_response();
    }
    // Record the contact before reading which consumers are registered. A
    // discharge takes the same lock, so either it sees this contact and
    // leaves the node alone, or it finishes first and the read below finds
    // the node unregistered.
    if authenticated_consumer {
        let recorded = {
            let mut contacts = council.consumer_contacts().lock().await;
            let now = std::time::Instant::now();
            contacts.observe_term(council.current_term(), now);
            contacts.record(&node_id, now)
        };
        if !recorded {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "endpoint consumer discharge in progress; poll again",
            )
                .into_response();
        }
        desired = council.desired_state().await;
    }
    // Registration precedes every first exposure. Once committed, a consumer
    // stays accountable until its view lease lapses and the leader discharges
    // it, or the operator permanently fences it.
    if authenticated_consumer && !desired.endpoint_consumers.contains(&node_id) {
        let registration = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            council.write(crate::council::RaftRequest::RegisterEndpointConsumer {
                node_id: node_id.clone(),
            }),
        )
        .await;
        match registration {
            Ok(Ok(crate::council::CouncilResponse::Applied { .. })) => {}
            Ok(Ok(crate::council::CouncilResponse::Refused { reason })) => {
                return (StatusCode::CONFLICT, reason).into_response();
            }
            _ => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "endpoint consumer registration is unconfirmed",
                )
                    .into_response();
            }
        }
        desired = council.desired_state().await;
        if !desired.endpoint_consumers.contains(&node_id) {
            return (StatusCode::GONE, "endpoint consumer identity was retired").into_response();
        }
    }
    let node = crate::meat::NodeId::new(&node_id);

    let mut apps = Vec::new();
    for (app_id, placements) in &desired.scheduling {
        let replicas = placements.iter().filter(|p| p.node_id == node).count() as u32;
        if replicas == 0 {
            continue;
        }
        let Some(spec) = desired.apps.get(app_id) else {
            continue; // spec deleted; placements lag briefly
        };
        apps.push(crate::cluster::orchestrate::NodeAssignment {
            name: app_id.name.clone(),
            namespace: app_id.namespace.clone(),
            replicas,
            spec: spec.clone(),
        });
    }

    Json(crate::cluster::orchestrate::NodeAssignments {
        apps,
        retirements: desired
            .test_leases
            .values()
            .filter(|lease| {
                matches!(
                    lease.state,
                    crate::testkit::lease::TestLeaseState::Cleaning { .. }
                )
            })
            .flat_map(|lease| {
                lease
                    .placements
                    .iter()
                    .filter(|placement| {
                        placement.node_id == node && !desired.apps.contains_key(&placement.app_id)
                    })
                    .map(|placement| crate::cluster::orchestrate::LeaseRetirement {
                        lease_id: lease.lease_id.clone(),
                        placement: placement.clone(),
                    })
            })
            .collect(),
        // All discovery fields describe the same committed state; serving them
        // does not discharge any cleanup obligation.
        endpoint_generation: desired.endpoint_withdrawals.generation,
        endpoint_catalog: desired.endpoint_catalog.clone(),
        endpoint_withdrawals: desired
            .endpoint_withdrawals
            .pending
            .iter()
            .filter(|(_, withdrawal)| withdrawal.consumers.contains(&node_id))
            .map(|(generation, withdrawal)| {
                crate::onion::withdrawal::EndpointWithdrawalInstruction {
                    generation: *generation,
                    services: withdrawal.services.clone(),
                }
            })
            .collect(),
        ingress: crate::cluster::orchestrate::cluster_ingress(&desired),
    })
    .into_response()
}

/// List all instances.
/// `GET /v1/apps` — the currently deployed resources in the CLI plan's
/// identifier format, for `relish apply --dry-run` diffing.
///
/// Cluster mode answers from the council's desired state (authoritative and
/// cluster-wide: apps with images, declared namespaces and permissions),
/// merged over the local agent's view (which contributes node-local jobs —
/// jobs don't live in desired state). Standalone answers from the local
/// agent alone.
async fn current_apps_handler(State(state): State<ApiState>) -> Response {
    // Plan-key → image; later inserts overwrite, so the council's
    // authoritative entries land last.
    let mut resources: std::collections::BTreeMap<String, Option<String>> =
        std::collections::BTreeMap::new();

    if let Ok(local) = ask_agent(&state.cmd_tx, |response| AgentCommand::CurrentResources {
        response,
    })
    .await
    {
        for entry in local {
            resources.insert(entry.resource, entry.image);
        }
    }

    if let Some(council) = &state.council {
        let desired = council.desired_state().await;
        for (app_id, spec) in &desired.apps {
            resources.insert(format!("app.{}", app_id.name), spec.image.clone());
        }
        for name in desired.namespaces.keys() {
            resources.insert(format!("namespace.{name}"), None);
        }
        for name in desired.permissions.keys() {
            resources.insert(format!("permission.{name}"), None);
        }
    }

    let rows: Vec<crate::bun::agent::CurrentResourceStatus> = resources
        .into_iter()
        .map(|(resource, image)| crate::bun::agent::CurrentResourceStatus { resource, image })
        .collect();
    Json(rows).into_response()
}

#[derive(Debug, Default, Deserialize)]
struct StatusQuery {
    #[serde(default)]
    cluster: bool,
}

async fn local_statuses(state: &ApiState) -> Result<Vec<InstanceStatus>, String> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (response, receiver) = oneshot::channel();
        state
            .cmd_tx
            .send(AgentCommand::Status { response })
            .await
            .map_err(|_| "agent unavailable".to_string())?;
        receiver
            .await
            .map_err(|_| "agent dropped response".to_string())
    })
    .await
    .map_err(|_| "agent status timed out".to_string())?
}

async fn status_handler(
    State(state): State<ApiState>,
    Query(query): Query<StatusQuery>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    let auth = auth.as_deref();
    let visible = |app: &str, namespace: &str| {
        crate::sesame::auth::authorize_scoped(auth, app, namespace).is_ok()
    };
    if !query.cluster {
        return match local_statuses(&state).await {
            Ok(mut statuses) => {
                statuses.retain(|status| visible(&status.app_name, &status.namespace));
                Json(statuses).into_response()
            }
            Err(error) => unavailable_response(error),
        };
    }
    // Peers answer the fan-out under this node's service token, which sees
    // everything, so the caller's scope has to be applied here.
    match cluster_statuses(&state).await {
        Ok(mut statuses) => {
            statuses
                .retain(|status| visible(&status.instance.app_name, &status.instance.namespace));
            Json(statuses).into_response()
        }
        Err(error) => unavailable_response(error),
    }
}

async fn cluster_statuses(
    state: &ApiState,
) -> Result<Vec<super::agent::ClusterInstanceStatus>, String> {
    let (statuses, failures) = collect_cluster_statuses(state, CLUSTER_STATUS_TIMEOUT).await?;
    match failures.into_iter().next() {
        Some(failure) => Err(format!("status incomplete: {failure}")),
        None => Ok(statuses),
    }
}

/// How long one peer may take to answer a cluster status fan-out.
const CLUSTER_STATUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// This node's cluster name, or `local` for a standalone agent.
fn local_node_name(state: &ApiState) -> String {
    state
        .node_name
        .clone()
        .or_else(|| {
            state.council.as_ref().and_then(|council| {
                let receiver = council.metrics();
                let metrics = receiver.borrow();
                metrics
                    .membership_config
                    .membership()
                    .get_node(&metrics.id)
                    .map(|node| node.name.clone())
            })
        })
        .unwrap_or_else(|| "local".to_string())
}

/// Every node's workload statuses, plus one message per peer that didn't
/// answer. Only this node's own status failing is an error: callers decide
/// whether a partial cluster view is good enough.
async fn collect_cluster_statuses(
    state: &ApiState,
    peer_timeout: std::time::Duration,
) -> Result<(Vec<super::agent::ClusterInstanceStatus>, Vec<String>), String> {
    let local_name = local_node_name(state);
    let mut statuses: Vec<_> = local_statuses(state)
        .await?
        .into_iter()
        .map(|instance| super::agent::ClusterInstanceStatus {
            node: local_name.to_string(),
            instance,
        })
        .collect();
    let members = match &state.membership {
        Some(membership) => membership.read().await.clone(),
        None => Vec::new(),
    };
    let requests = futures_util::stream::iter(
        members
            .into_iter()
            .filter(|member| member.node_id.0 != local_name)
            .map(|member| async move {
                let name = member.node_id.0;
                let result = tokio::time::timeout(peer_timeout, async {
                    let url = state
                        .cluster_http
                        .url(&member.address.to_string(), "/v1/status");
                    let mut request = state.cluster_http.client().get(url);
                    if let Some(token) = &state.service_token {
                        request = request.bearer_auth(token);
                    }
                    request
                        .send()
                        .await?
                        .error_for_status()?
                        .json::<Vec<InstanceStatus>>()
                        .await
                })
                .await;
                match result {
                    Ok(Ok(instances)) => Ok(instances
                        .into_iter()
                        .map(|instance| super::agent::ClusterInstanceStatus {
                            node: name.clone(),
                            instance,
                        })
                        .collect::<Vec<_>>()),
                    Ok(Err(error)) => Err(format!("node {name}: {error}")),
                    Err(_) => Err(format!("node {name} timed out")),
                }
            }),
    )
    .buffer_unordered(8);
    tokio::pin!(requests);
    let mut failures = Vec::new();
    while let Some(result) = requests.next().await {
        match result {
            Ok(instances) => statuses.extend(instances),
            Err(failure) => failures.push(failure),
        }
    }
    failures.sort();
    statuses.sort_by(|left, right| {
        (&left.node, &left.instance.namespace, &left.instance.id).cmp(&(
            &right.node,
            &right.instance.namespace,
            &right.instance.id,
        ))
    });
    Ok((statuses, failures))
}

/// `GET /v1/top[?cluster=true]`: workloads with their latest CPU and memory.
///
/// Without `cluster` a node answers for itself. With it, the node merges its
/// own rows with every peer's; a peer that doesn't answer becomes a warning
/// rather than failing the whole view, so `relish top` still works while a
/// node is down.
async fn top_handler(
    State(state): State<ApiState>,
    Query(query): Query<StatusQuery>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    let auth = auth.as_deref();
    let visible = |row: &crate::bun::top::TopRow| {
        crate::sesame::auth::authorize_scoped(auth, &row.instance.app_name, &row.instance.namespace)
            .is_ok()
    };
    let mut rows = match local_top_rows(&state).await {
        Ok(rows) => rows,
        Err(error) => return unavailable_response(error),
    };
    if !query.cluster {
        rows.retain(visible);
        return Json(rows).into_response();
    }
    let mut warnings = Vec::new();
    let local_name = local_node_name(&state);
    let members = match &state.membership {
        Some(membership) => membership.read().await.clone(),
        None => Vec::new(),
    };
    let requests = futures_util::stream::iter(
        members
            .into_iter()
            .filter(|member| member.node_id.0 != local_name)
            .map(|member| {
                let state = &state;
                async move {
                    let name = member.node_id.0;
                    let result = tokio::time::timeout(CLUSTER_STATUS_TIMEOUT, async {
                        let url = state
                            .cluster_http
                            .url(&member.address.to_string(), "/v1/top");
                        let mut request = state.cluster_http.client().get(url);
                        if let Some(token) = &state.service_token {
                            request = request.bearer_auth(token);
                        }
                        request
                            .send()
                            .await?
                            .error_for_status()?
                            .json::<Vec<crate::bun::top::TopRow>>()
                            .await
                    })
                    .await;
                    match result {
                        Ok(Ok(rows)) => Ok(rows),
                        Ok(Err(error)) => Err(format!("node {name}: {error}")),
                        Err(_) => Err(format!("node {name} timed out")),
                    }
                }
            }),
    )
    .buffer_unordered(8);
    tokio::pin!(requests);
    while let Some(result) = requests.next().await {
        match result {
            Ok(peer_rows) => rows.extend(peer_rows),
            Err(warning) => warnings.push(warning),
        }
    }
    // Peers answered with the node's service token, which sees everything,
    // so the caller's scope applies here.
    rows.retain(visible);
    rows.sort_by(|left, right| {
        (&left.node, &left.instance.namespace, &left.instance.id).cmp(&(
            &right.node,
            &right.instance.namespace,
            &right.instance.id,
        ))
    });
    warnings.sort();
    Json(crate::bun::top::ClusterTop { rows, warnings }).into_response()
}

/// This node's workloads joined to their latest samples in its own store.
async fn local_top_rows(state: &ApiState) -> Result<Vec<crate::bun::top::TopRow>, String> {
    use crate::bun::top::{CPU_METRIC, MEMORY_METRIC, USAGE_WINDOW_SECS};

    let statuses = local_statuses(state).await?;
    let usage = match &state.mayo {
        Some(mayo) => {
            let since = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_sub(USAGE_WINDOW_SECS);
            let sql = format!(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE metric_name IN ('{CPU_METRIC}', '{MEMORY_METRIC}') \
                 AND timestamp >= {since} ORDER BY timestamp"
            );
            // Missing samples leave the columns empty; they don't hide the
            // workloads themselves.
            match mayo.read().await.query_sql(&sql).await {
                Ok(samples) => crate::bun::top::latest_usage(&samples),
                Err(_) => std::collections::HashMap::new(),
            }
        }
        None => std::collections::HashMap::new(),
    };
    Ok(crate::bun::top::node_rows(
        &local_node_name(state),
        statuses,
        &usage,
    ))
}

/// List all run-to-completion workload instances.
async fn jobs_handler(State(state): State<ApiState>) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::JobStatus {
        response,
    })
    .await
    {
        Ok(statuses) => Json(statuses).into_response(),
        Err(_) => agent_unavailable(),
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    limit: Option<usize>,
    app: Option<String>,
    severity: Option<crate::bun::events::EventSeverity>,
}

/// Return recent events from the bounded in-memory store.
async fn events_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(query): Query<EventsQuery>,
) -> Response {
    // Audit events span every app and namespace, so a scoped token is refused
    // (C3) just as it is for the cluster-wide metrics and logs endpoints.
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    let Some(events) = &state.events else {
        return Json(serde_json::json!({"events": []})).into_response();
    };
    let store = events.read().await;
    Json(serde_json::json!({
        "events": store.recent(
            query.limit.unwrap_or(100),
            query.app.as_deref(),
            query.severity,
        )
    }))
    .into_response()
}

/// Upgrade an authenticated request to the live event stream.
async fn ws_events_handler(State(state): State<ApiState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .on_upgrade(move |socket| ws_events_session(socket, state.events))
        .into_response()
}

async fn ws_events_session(
    mut socket: WebSocket,
    events: Option<Arc<RwLock<crate::bun::events::EventStore>>>,
) {
    let Some(events) = events else { return };
    let (recent, mut receiver) = {
        let store = events.read().await;
        (store.recent(50, None, None), store.subscribe())
    };
    for event in recent {
        let Ok(json) = serde_json::to_string(&event) else {
            continue;
        };
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            event = receiver.recv() => match event {
                Ok(event) => {
                    let Ok(json) = serde_json::to_string(&event) else { continue };
                    if socket.send(Message::Text(json.into())).await.is_err() { return; }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            message = socket.recv() => if message.is_none() { return; },
        }
    }
}

/// Status for a specific app.
async fn status_app_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Status { response }).await {
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
        Err(response) => response,
    }
}

/// Stop an app.
///
/// In cluster mode, stopping an app is a desired-state change: the app is
/// deleted from Raft (`AppDelete`) so the scheduler stops placing it and no
/// reconciler resurrects it on the next tick (DEP2). The local supervisor
/// stop is then best-effort. Because the delete goes through the council,
/// a leader that holds no local replica still clears cluster state instead
/// of returning a spurious 404. In standalone mode there is no desired
/// state, so we just stop the local instances as before.
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
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Scale,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }

    if let Some(council) = state.council.clone() {
        return cluster_app_change(state, council, app, namespace, AppChange::Stop).await;
    }

    stop_local(&state, app, namespace).await
}

/// `POST /v1/delete/{app}/{namespace}` — remove an app from the cluster.
///
/// In cluster mode the app leaves desired state and every node retires its
/// instances. A standalone node has no desired state beyond its running
/// instances, so deleting is the same as stopping there.
async fn delete_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Deploy,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }

    if let Some(council) = state.council.clone() {
        return cluster_app_change(state, council, app, namespace, AppChange::Delete).await;
    }

    stop_local(&state, app, namespace).await
}

/// Whether `relish stop` or `relish delete` is changing an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppChange {
    /// Scale to zero, keeping the specification, until the next apply.
    Stop,
    /// Remove the app from desired state.
    Delete,
}

impl AppChange {
    fn verb(self) -> &'static str {
        match self {
            AppChange::Stop => "stop",
            AppChange::Delete => "delete",
        }
    }
}

/// Stop or delete an app in cluster mode through Raft. Nodes' reconcilers
/// then retire its instances, the leader's own included. Stopping the local
/// replica directly used to leave the reconciler believing it still ran, so
/// an apply straight afterwards never brought it back.
async fn cluster_app_change(
    state: ApiState,
    council: Arc<crate::council::CouncilNode>,
    app: String,
    namespace: String,
    change: AppChange,
) -> Response {
    // Followers can't write to Raft (openraft does not forward client
    // writes), so forward the whole request to the leader's API.
    if !council.is_leader().await {
        let Some(leader_url) = leader_api_url(&state, &council).await else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "no cluster leader known yet; retry shortly"
                })),
            )
                .into_response();
        };
        let url = format!("{leader_url}/v1/{}/{app}/{namespace}", change.verb());
        let mut request = state.cluster_http.client().post(url);
        if let Some(token) = &state.service_token {
            request = request.bearer_auth(token);
        }
        return match request.send().await {
            Ok(response) => {
                let status = StatusCode::from_u16(response.status().as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY);
                let body = response.bytes().await.unwrap_or_default();
                (status, body).into_response()
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": format!("failed to forward {} to the leader: {e}", change.verb())
                })),
            )
                .into_response(),
        };
    }

    let app_id = crate::meat::AppId::new(&app, &namespace);
    let request = match change {
        AppChange::Stop => crate::council::types::RaftRequest::AppStop { app_id },
        AppChange::Delete => crate::council::types::RaftRequest::AppDelete { app_id },
    };
    match council.write(request).await {
        Ok(crate::council::CouncilResponse::Refused { reason }) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response(),
        Ok(_) => {
            let status = match change {
                AppChange::Stop => "stopped",
                AppChange::Delete => "deleted",
            };
            Json(serde_json::json!({ "status": status })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("failed to update desired state: {e}")
            })),
        )
            .into_response(),
    }
}

/// Stop an app on this node only (standalone mode).
async fn stop_local(state: &ApiState, app: String, namespace: String) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Stop {
        app_name: app,
        namespace,
        response,
    })
    .await
    {
        Ok(Ok(())) => Json(serde_json::json!({ "status": "stopped" })).into_response(),
        Ok(Err(error)) => {
            let status = match error {
                crate::bun::BunError::AppNotFound { .. } => StatusCode::NOT_FOUND,
                crate::bun::BunError::WorkloadBusy { .. } => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                status,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response()
        }
        Err(response) => response,
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
    /// Follow only this node's instances. Set on the internal per-node
    /// streams of a cluster-wide follow, so a peer never fans out again.
    local: Option<bool>,
    /// Prefix each followed line with `[node instance]`.
    label: Option<bool>,
}

/// Get logs for an app.
///
/// Supports `?tail=N` to return only the last N lines, and
/// `?follow=true` to stream new lines as an SSE stream.
async fn logs_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    let follow = query.follow.unwrap_or(false);

    if follow {
        // A cluster member follows every node that runs the app; the
        // per-node streams it opens come back here with `local=true`.
        if !query.local.unwrap_or(false)
            && let (Some(council), Some(membership), Some(self_name)) =
                (&state.council, &state.membership, &state.node_name)
        {
            let (events_tx, events_rx) = mpsc::channel::<Event>(256);
            tokio::spawn(follow_cluster_logs(
                state.clone(),
                Arc::clone(council),
                Arc::clone(membership),
                self_name.clone(),
                app,
                namespace,
                query.tail,
                events_tx,
            ));
            let stream = ReceiverStream::new(events_rx).map(Ok::<_, std::convert::Infallible>);
            return Sse::new(stream)
                .keep_alive(axum::response::sse::KeepAlive::default())
                .into_response();
        }
        let label = query
            .label
            .unwrap_or(false)
            .then(|| state.node_name.clone())
            .flatten();
        let lines_rx = match follow_local_logs(&state, app, namespace, query.tail, label).await {
            Ok(lines_rx) => lines_rx,
            Err(response) => return response,
        };
        let stream = ReceiverStream::new(lines_rx)
            .map(|line| Ok::<_, std::convert::Infallible>(Event::default().data(line)));
        return Sse::new(stream).into_response();
    }

    match ask_agent(&state.cmd_tx, |response| AgentCommand::Logs {
        app_name: app,
        namespace,
        tail: query.tail,
        response,
    })
    .await
    {
        Ok(Ok(logs)) => Json(serde_json::json!({ "logs": logs })).into_response(),
        Ok(Err(e)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(response) => response,
    }
}

/// Start following this node's instances of an app.
// `Response` is large but it IS the HTTP reply to send on failure;
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
async fn follow_local_logs(
    state: &ApiState,
    app: String,
    namespace: String,
    tail: Option<usize>,
    label: Option<String>,
) -> Result<mpsc::Receiver<String>, Response> {
    let (lines_tx, lines_rx) = mpsc::channel::<String>(64);
    state
        .cmd_tx
        .send(AgentCommand::FollowLogs {
            app_name: app,
            namespace,
            tail,
            label,
            lines: lines_tx,
        })
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "agent unavailable" })),
            )
                .into_response()
        })?;
    Ok(lines_rx)
}

/// How often a cluster-wide follow re-reads placements, to pick up replicas
/// scheduled onto new nodes and to notice nodes that left.
const LOG_FOLLOW_REFRESH: std::time::Duration = std::time::Duration::from_secs(2);

/// Why one node's part of a cluster-wide follow stopped.
struct LogSourceEnded {
    node: String,
    /// `None` when the stream ended cleanly, say because its replica
    /// restarted; the next refresh reconnects without a warning.
    error: Option<String>,
}

/// Merge the log streams of every node that runs an app into `events`.
///
/// Every [`LOG_FOLLOW_REFRESH`] it re-reads the app's placements and the live
/// membership: it opens a stream to each placed node it isn't following yet
/// and drops the streams of nodes that left. A node that goes away produces a
/// `warning` event and the follow carries on with the rest. It returns when
/// the client disconnects.
#[allow(clippy::too_many_arguments)]
async fn follow_cluster_logs(
    state: ApiState,
    council: Arc<crate::council::CouncilNode>,
    membership: Arc<RwLock<Vec<NodeMembershipInfo>>>,
    self_name: String,
    app: String,
    namespace: String,
    tail: Option<usize>,
    events: mpsc::Sender<Event>,
) {
    let app_id = crate::meat::types::AppId::new(&app, &namespace);
    let mut sources: std::collections::HashMap<String, tokio::task::AbortHandle> =
        std::collections::HashMap::new();
    let mut connected_before: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut departed: std::collections::HashSet<String> = std::collections::HashSet::new();
    // When each node's last stream ended, so a node with nothing to stream
    // yet is retried once per refresh rather than in a tight loop.
    let mut ended_at: std::collections::HashMap<String, tokio::time::Instant> =
        std::collections::HashMap::new();
    let (ended_tx, mut ended_rx) = mpsc::channel::<LogSourceEnded>(16);
    loop {
        let placed: std::collections::BTreeSet<crate::meat::NodeId> = council
            .desired_state()
            .await
            .scheduling
            .get(&app_id)
            .map(|placements| placements.iter().map(|p| p.node_id.clone()).collect())
            .unwrap_or_default();
        let members = membership.read().await.clone();

        // A node we followed that dropped out of the live membership gets
        // one warning, whether its stream broke, ended cleanly (a graceful
        // shutdown) or is still hanging on a dead connection.
        let alive = |node: &str| members.iter().any(|member| member.node_id.0 == node);
        departed.retain(|node| !alive(node));
        let newly_departed: Vec<String> = connected_before
            .iter()
            .filter(|node| **node != self_name && !alive(node) && !departed.contains(*node))
            .cloned()
            .collect();
        for node in newly_departed {
            if let Some(source) = sources.remove(&node) {
                source.abort();
            }
            let warning = format!("node {node} left the cluster; no longer following its logs");
            if !send_log_warning(&events, warning).await {
                return;
            }
            departed.insert(node);
        }

        for node in placed {
            let cooling = ended_at
                .get(&node.0)
                .is_some_and(|at| at.elapsed() < LOG_FOLLOW_REFRESH);
            if sources.contains_key(&node.0) || cooling {
                continue;
            }
            // Only the first connection replays the tail; a reconnect after
            // a replica restart carries on from new lines.
            let tail = if connected_before.insert(node.0.clone()) {
                tail
            } else {
                None
            };
            let source = if node.0 == self_name {
                spawn_local_log_source(
                    &state,
                    &app,
                    &namespace,
                    tail,
                    &self_name,
                    events.clone(),
                    ended_tx.clone(),
                )
                .await
            } else {
                let Some(member) = members.iter().find(|member| member.node_id == node) else {
                    continue;
                };
                let url = state.cluster_http.url(
                    &member.address.to_string(),
                    &format!("/v1/logs/{app}/{namespace}"),
                );
                Some(spawn_peer_log_source(
                    &state,
                    node.0.clone(),
                    url,
                    tail,
                    events.clone(),
                    ended_tx.clone(),
                ))
            };
            if let Some(source) = source {
                sources.insert(node.0, source);
            }
        }

        tokio::select! {
            () = events.closed() => break,
            Some(ended) = ended_rx.recv() => {
                sources.remove(&ended.node);
                ended_at.insert(ended.node.clone(), tokio::time::Instant::now());
                if let Some(error) = ended.error
                    && !send_log_warning(&events, format!("node {}: {error}", ended.node)).await
                {
                    break;
                }
            }
            () = tokio::time::sleep(LOG_FOLLOW_REFRESH) => {}
        }
    }
    for source in sources.into_values() {
        source.abort();
    }
}

async fn send_log_warning(events: &mpsc::Sender<Event>, warning: String) -> bool {
    events
        .send(
            Event::default()
                .event(crate::ketchup::sse::WARNING_EVENT)
                .data(warning),
        )
        .await
        .is_ok()
}

/// Follow this node's own instances as one source of a cluster-wide follow.
async fn spawn_local_log_source(
    state: &ApiState,
    app: &str,
    namespace: &str,
    tail: Option<usize>,
    self_name: &str,
    events: mpsc::Sender<Event>,
    ended: mpsc::Sender<LogSourceEnded>,
) -> Option<tokio::task::AbortHandle> {
    let mut lines = follow_local_logs(
        state,
        app.to_string(),
        namespace.to_string(),
        tail,
        Some(self_name.to_string()),
    )
    .await
    .ok()?;
    let node = self_name.to_string();
    Some(
        tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                if events.send(Event::default().data(line)).await.is_err() {
                    return;
                }
            }
            let _ = ended.send(LogSourceEnded { node, error: None }).await;
        })
        .abort_handle(),
    )
}

/// Stream one peer's labelled log lines into `events`, and report how the
/// stream ended.
fn spawn_peer_log_source(
    state: &ApiState,
    node: String,
    url: String,
    tail: Option<usize>,
    events: mpsc::Sender<Event>,
    ended: mpsc::Sender<LogSourceEnded>,
) -> tokio::task::AbortHandle {
    let mut request = state.cluster_http.client().get(url).query(&[
        ("follow", "true"),
        ("local", "true"),
        ("label", "true"),
    ]);
    if let Some(tail) = tail {
        request = request.query(&[("tail", tail)]);
    }
    if let Some(token) = &state.service_token {
        request = request.bearer_auth(token);
    }
    tokio::spawn(async move {
        let error = relay_peer_log_stream(request, &events).await.err();
        let _ = ended.send(LogSourceEnded { node, error }).await;
    })
    .abort_handle()
}

async fn relay_peer_log_stream(
    request: reqwest::RequestBuilder,
    events: &mpsc::Sender<Event>,
) -> Result<(), String> {
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), request.send())
        .await
        .map_err(|_| "log stream did not start within 5s".to_string())?
        .map_err(|error| format!("log stream failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("log stream refused: {}", response.status()));
    }
    let mut decoder = crate::ketchup::sse::SseDecoder::default();
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| format!("log stream broke: {error}"))?;
        for event in decoder.push(&chunk) {
            let mut forwarded = Event::default().data(event.data);
            if let Some(kind) = event.event {
                forwarded = forwarded.event(kind);
            }
            if events.send(forwarded).await.is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Upgrade an authenticated request to a live log stream.
async fn ws_logs_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Scope is checked *before* the upgrade: once the socket is live there
    // is no response left to refuse with.
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    upgrade
        .on_upgrade(move |socket| ws_logs_session(socket, state.cmd_tx, app, namespace, query.tail))
        .into_response()
}

async fn ws_logs_session(
    mut socket: WebSocket,
    command_tx: mpsc::Sender<AgentCommand>,
    app: String,
    namespace: String,
    tail: Option<usize>,
) {
    let (lines_tx, mut lines_rx) = mpsc::channel(64);
    if command_tx
        .send(AgentCommand::FollowLogs {
            app_name: app,
            namespace,
            tail,
            label: None,
            lines: lines_tx,
        })
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            line = lines_rx.recv() => match line {
                Some(line) => if socket.send(Message::Text(line.into())).await.is_err() { return; },
                None => return,
            },
            message = socket.recv() => if message.is_none() { return; },
        }
    }
}

/// `GET /v1/logs/entries/{app}/{namespace}?start=S&end=E&grep=G&tail=N`
///
/// Internal structured log query endpoint. Returns `Vec<LogEntry>` as
/// JSON. Called by `fan_out_query` on each node during cross-node queries.
async fn logs_entries_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
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
/// Cross-node log query. Fans out to every live member (an app's lines stay
/// on each node it ever ran on, see [`crate::ketchup::query::query_targets`])
/// and merges the answers in ingest order.
async fn logs_cross_node_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
) -> Response {
    use crate::meat::types::AppId;

    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }

    // Build a LogQuery from request params
    let log_query = LogQuery {
        app: app.clone(),
        namespace: namespace.clone(),
        start: query.start,
        end: query.end,
        grep: query.grep.clone(),
        json_field: None,
        // The newest N cluster-wide are among each node's newest N, so every
        // node sends only its own tail; the merge below trims to N again.
        tail: query.tail,
    };

    // If we have council + membership, do cross-node fan-out
    if let (Some(council), Some(membership)) = (&state.council, &state.membership) {
        let desired = council.desired_state().await;
        let app_id = AppId::new(&app, &namespace);

        // Where the app runs now; its history may be on any live member.
        let placed: Vec<String> = desired
            .scheduling
            .get(&app_id)
            .map(|placements| placements.iter().map(|p| p.node_id.0.clone()).collect())
            .unwrap_or_default();
        let live: Vec<(String, String)> = membership
            .read()
            .await
            .iter()
            .map(|member| {
                (
                    member.node_id.0.clone(),
                    state.cluster_http.url(&member.address.to_string(), ""),
                )
            })
            .collect();
        let targets = crate::ketchup::query::query_targets(&placed, &live);
        let nodes = targets.reachable;
        // A placed node with no membership entry can't be reached at all.
        let mut warnings: Vec<LogQueryWarning> = targets
            .unreachable
            .into_iter()
            .map(|node_id| LogQueryWarning::NodeUnresponsive { node_id })
            .collect();

        let node_count = nodes.len() + warnings.len();

        // Fan out to all reachable nodes
        let timeout = std::time::Duration::from_secs(10);
        match fan_out_query(
            &log_query,
            &nodes,
            state.cluster_http.client(),
            timeout,
            state.service_token.as_deref(),
        )
        .await
        {
            Ok(result) => {
                let mut entries = result.entries;
                // Each node that failed the fan-out becomes a warning, so the
                // caller sees "some replicas were down", not a silent empty.
                for failure in result.failures {
                    warnings.push(LogQueryWarning::NodeUnresponsive {
                        node_id: failure.node_id,
                    });
                }
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
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Exec,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Exec {
        app_name: app,
        namespace,
        command: body.command,
        response,
    })
    .await
    {
        Ok(Ok(output)) => Json(serde_json::json!({ "output": output })).into_response(),
        Ok(Err(e)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(response) => response,
    }
}

/// List cluster nodes.
async fn nodes_handler(State(state): State<ApiState>) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Nodes { response }).await {
        Ok(mut nodes) => {
            if let Some(membership) = &state.membership {
                let members = membership.read().await;
                for node in &mut nodes {
                    node.api_address = members
                        .iter()
                        .find(|member| member.node_id.0 == node.node_id && member.api_advertised)
                        .map(|member| member.address);
                }
            }
            Json(nodes).into_response()
        }
        Err(response) => response,
    }
}

/// Largest request body the node relay forwards (a path request is tiny).
const MAX_RELAY_REQUEST_BYTES: usize = 64 * 1024;
/// Largest response the node relay passes back (an events page is the biggest).
const MAX_RELAY_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// A path probe runs for up to 25 seconds on the target; allow for the hop.
const RELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The per-node reads `relish wtf`, `relish path` and `relish test` make, and
/// nothing else. The relay is a reachability aid, not a general proxy.
fn relay_allows(method: &axum::http::Method, path: &str) -> bool {
    const READS: &[&str] = &[
        "v1/health",
        "v1/status",
        "v1/diagnostics",
        "v1/diagnostics/apps",
        "v1/events",
        "v1/deploys/operations",
        "v1/alerts",
        "v1/fault",
        "v1/cluster/council",
        "v1/cluster/nodes",
        "v1/capabilities",
    ];
    match *method {
        // `relish test` compares each node's own deploy history.
        axum::http::Method::GET => READS.contains(&path) || is_deploy_history_path(path),
        // `relish exec` reaches an instance on another node this way too;
        // the target repeats the exec authorisation with the caller's token.
        axum::http::Method::POST => path == "v1/path" || is_exec_path(path),
        _ => false,
    }
}

/// `v1/deploys/history/{app}` and nothing longer (the namespace is a query).
fn is_deploy_history_path(path: &str) -> bool {
    path.strip_prefix("v1/deploys/history/")
        .is_some_and(|app| !app.is_empty() && !app.contains('/'))
}

/// `v1/exec/{app}/{namespace}` and nothing longer.
fn is_exec_path(path: &str) -> bool {
    let mut segments = path.split('/');
    segments.next() == Some("v1")
        && segments.next() == Some("exec")
        && segments.next().is_some_and(|app| !app.is_empty())
        && segments
            .next()
            .is_some_and(|namespace| !namespace.is_empty())
        && segments.next().is_none()
}

/// `GET|POST /v1/nodes/{node}/relay/{path}`: send one of a few per-node
/// diagnostic requests to a named node and return its answer.
///
/// A laptop host can reach node 1's forwarded port but not the guests' own
/// addresses, so `relish wtf` and `relish path` reach every other node
/// through this. The caller's own credential travels with the request and the
/// target repeats every authentication and authorisation check; the relay
/// never adds the node's service identity.
async fn node_relay_handler(
    State(state): State<ApiState>,
    known: Option<axum::Extension<KnownMembers>>,
    Path((node, path)): Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !relay_allows(&method, &path) {
        return (
            StatusCode::NOT_FOUND,
            format!("the node relay does not forward {method} /{path}"),
        )
            .into_response();
    }
    let mut url =
        match known_node_api_url(&state, known.as_deref(), &node, &format!("/{path}")).await {
            Ok(url) => url,
            Err(response) => return response,
        };
    if let Some(query) = uri.query() {
        url.push('?');
        url.push_str(query);
    }
    let mut request = state.cluster_http.client().request(method.clone(), url);
    if method == axum::http::Method::POST {
        request = request
            .header(
                axum::http::header::CONTENT_TYPE.as_str(),
                "application/json",
            )
            .body(body);
    }
    let request = copy_forwarded_auth(request, &headers);
    let response = match tokio::time::timeout(RELAY_TIMEOUT, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("node {node} did not answer: {error}"),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "node {node} did not answer within {}s",
                    RELAY_TIMEOUT.as_secs()
                ),
            )
                .into_response();
        }
    };
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return (
                StatusCode::BAD_GATEWAY,
                format!("node {node} broke off its answer"),
            )
                .into_response();
        };
        if bytes.len() + chunk.len() > MAX_RELAY_RESPONSE_BYTES {
            return (
                StatusCode::BAD_GATEWAY,
                format!("node {node} answered with more than the relay's 8 MiB limit"),
            )
                .into_response();
        }
        bytes.extend_from_slice(&chunk);
    }
    let mut relayed = (status, bytes).into_response();
    if let Some(content_type) = content_type
        && let Ok(value) = axum::http::HeaderValue::from_str(&content_type)
    {
        relayed
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, value);
    }
    relayed
}

/// Show council (Raft) status.
async fn council_handler(State(state): State<ApiState>) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Council { response }).await {
        Ok(council) => Json(serde_json::json!(council)).into_response(),
        Err(response) => response,
    }
}

/// Render the dashboard login page.
async fn login_handler() -> Response {
    axum::response::Html(crate::brioche::login::render_login(None)).into_response()
}

/// Form body for the login/session exchange.
#[derive(Deserialize)]
struct SessionForm {
    token: String,
}

/// Exchange an API token for a read-only session cookie.
///
/// The browser posts a token once; on success it receives an `HttpOnly`,
/// `SameSite=Strict` cookie and is redirected to the dashboard. The session
/// is read-only regardless of the token's role.
async fn ui_session_handler(
    State(auth): State<crate::sesame::auth::AuthState>,
    axum::Form(form): axum::Form<SessionForm>,
) -> Response {
    // Accept the internal service token or any valid user token. The session
    // inherits the presented token's scope (C3), so a tenant-scoped token
    // cannot widen to cluster-wide reads by exchanging itself for a cookie.
    let identity = if auth
        .service_token
        .as_deref()
        .is_some_and(|s| crate::sesame::auth::tokens_equal(&form.token, s))
    {
        // The operator presented the real service token; the session is
        // unconfined (but still read-only), matching the service principal.
        Some((
            crate::sesame::auth::SYSTEM_PRINCIPAL.to_string(),
            crate::sesame::types::TokenScope::default(),
        ))
    } else {
        // Snapshot the tokens under the lock, then run the Argon2id verify on
        // the blocking pool (M7) so the deliberately-slow hashing doesn't stall
        // the async runtime worker.
        let tokens = auth.tokens.read().await.clone();
        let candidate = form.token.clone();
        tokio::task::spawn_blocking(move || {
            crate::sesame::auth::authenticate(&candidate, &tokens)
                .ok()
                .map(|ctx| {
                    (
                        ctx.token_name,
                        crate::sesame::types::TokenScope {
                            apps: ctx.scoped_apps,
                            namespaces: ctx.scoped_namespaces,
                        },
                    )
                })
        })
        .await
        .unwrap_or(None)
    };

    let Some((name, scope)) = identity else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::response::Html(crate::brioche::login::render_login(Some(
                "invalid or expired token",
            ))),
        )
            .into_response();
    };

    let id = auth.sessions.create(&name, scope).await;
    let cookie = format!(
        "{}={id}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200",
        crate::sesame::session::SESSION_COOKIE
    );
    (
        [(axum::http::header::SET_COOKIE, cookie)],
        axum::response::Redirect::to("/"),
    )
        .into_response()
}

/// Clear the current session (logout).
async fn ui_logout_handler(
    State(auth): State<crate::sesame::auth::AuthState>,
    headers: HeaderMap,
) -> Response {
    if let Some(id) = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(crate::sesame::session::session_id_from_cookie_header)
    {
        auth.sessions.remove(id).await;
    }
    let cleared = format!(
        "{}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        crate::sesame::session::SESSION_COOKIE
    );
    (
        [(axum::http::header::SET_COOKIE, cleared)],
        axum::response::Redirect::to("/ui/login"),
    )
        .into_response()
}

/// Issue a certificate bundle to a joining node (issuer side).
///
/// Public route: the join token is the credential. The joiner sends a CSR and
/// keeps its private key (PKI4); we sign the CSR and return the leaf plus CA
/// chain the joiner persists as its identity.
/// `GET /v1/cluster/ca` — the cluster's public CA certificates.
///
/// A joiner fetches these *before* sending its one-time join token so it can
/// verify the cluster's identity against a pinned `--ca-fingerprint` and then
/// transmit the token only over a connection proven to chain to this CA. CA
/// certificates are public material, so the endpoint needs no authentication.
async fn cluster_ca_handler(State(state): State<ApiState>) -> Response {
    use base64::Engine as _;
    let Some(ref council) = state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council available" })),
        )
            .into_response();
    };
    let security = council.security_state().await;
    let (Some(node_ca), Some(root_ca)) = (
        security.get_ca(crate::sesame::types::CaRole::Node),
        security.get_ca(crate::sesame::types::CaRole::Root),
    ) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "cluster CA not initialised" })),
        )
            .into_response();
    };
    let encoder = base64::engine::general_purpose::STANDARD;
    Json(serde_json::json!({
        "compatibility": crate::compatibility::CURRENT,
        "node_ca_b64": encoder.encode(&node_ca.certificate_der),
        "root_ca_b64": encoder.encode(&root_ca.certificate_der),
    }))
    .into_response()
}

/// Renew only the node authenticated on this connection. A follower refuses;
/// forwarding would substitute the follower's TLS identity for the caller's.
async fn node_renewal_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    lifetime: Option<axum::Extension<crate::sesame::renewal::NodeLeafLifetime>>,
    State(state): State<ApiState>,
    Json(request): Json<crate::sesame::renewal::RenewalRequest>,
) -> Response {
    use crate::sesame::renewal::{RenewalError, issue_renewal};
    let lifetime = lifetime.map_or(crate::sesame::ca::NODE_LEAF_LIFETIME, |lifetime| {
        lifetime.0.0
    });
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    let Some(peer) = peer else {
        return (
            StatusCode::FORBIDDEN,
            "node renewal requires a TLS client certificate",
        )
            .into_response();
    };
    let Some(council) = &state.council else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no council available").into_response();
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        issue_renewal(council, &peer, &request, lifetime),
    )
    .await
    {
        Ok(Ok(bundle)) => Json(bundle).into_response(),
        Ok(Err(error)) => {
            let status = match &error {
                RenewalError::Identity(_) => StatusCode::FORBIDDEN,
                RenewalError::Request(_) => StatusCode::BAD_REQUEST,
                RenewalError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            };
            (
                status,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response()
        }
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "node renewal timed out").into_response(),
    }
}

/// Include request-body extraction in the control-operation deadline.
async fn registry_proposal_deadline(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    match tokio::time::timeout(std::time::Duration::from_secs(10), next.run(request)).await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::REQUEST_TIMEOUT,
            "registry proposal deadline exceeded",
        )
            .into_response(),
    }
}

/// A follower refuses instead of forwarding a request under its own identity.
async fn registry_proposal_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    State(state): State<ApiState>,
    Json(proposal): Json<crate::pickle::authority::RegistryProposal>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    if let Err(error) = proposal.compatibility.require_current() {
        return (StatusCode::CONFLICT, error.to_string()).into_response();
    }
    let Some(peer) = peer else {
        return (
            StatusCode::FORBIDDEN,
            "registry proposals require a TLS node certificate",
        )
            .into_response();
    };
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no registry council available",
        )
            .into_response();
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let security = council
            .security_state_linearizable()
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
        let node_id = crate::sesame::renewal::validate_peer(&peer, &security)
            .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        let request = proposal
            .mutation
            .request_for_node(&node_id, crate::testkit::lease::now_unix_millis())
            .map_err(|error| (StatusCode::FORBIDDEN, error.to_string()))?;
        council
            .write(request)
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))
    })
    .await;
    match result {
        Ok(Ok(response @ crate::council::CouncilResponse::Refused { .. })) => {
            (StatusCode::CONFLICT, Json(response)).into_response()
        }
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "registry proposal timed out").into_response(),
    }
}

async fn registry_query_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    peer: Option<axum::Extension<crate::sesame::renewal::TlsPeerCertificate>>,
    State(state): State<ApiState>,
    Json(request): Json<crate::pickle::authority::RegistryQueryRequest>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    if let Err(error) = request.compatibility.require_current() {
        return (StatusCode::CONFLICT, error.to_string()).into_response();
    }
    let Some(peer) = peer else {
        return (
            StatusCode::FORBIDDEN,
            "registry queries require a TLS node certificate",
        )
            .into_response();
    };
    let Some(council) = &state.council else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let security = match council.security_state_linearizable().await {
        Ok(security) => security,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let node = match crate::sesame::renewal::validate_peer(&peer, &security) {
        Ok(node) => node,
        Err(error) => return (StatusCode::FORBIDDEN, error.to_string()).into_response(),
    };
    if crate::cluster::identity::raft_id_from_name(&node) != request.node_id {
        return (
            StatusCode::FORBIDDEN,
            "registry query does not belong to authenticated node",
        )
            .into_response();
    }
    let answer = request
        .query
        .answer(&council.desired_state().await, request.node_id);
    bounded_registry_query_response(answer).await
}

async fn bounded_registry_query_response(
    answer: crate::pickle::authority::RegistryQueryResponse,
) -> Response {
    let encoded = tokio::task::spawn_blocking(move || serde_json::to_vec(&answer)).await;
    match encoded {
        Ok(Ok(bytes)) if bytes.len() <= crate::pickle::authority::MAX_REGISTRY_PROPOSAL_BYTES => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        Ok(Ok(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "registry query result exceeds the control-message limit",
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn join_handler(
    State(state): State<ApiState>,
    Json(body): Json<crate::sesame::join::JoinRequest>,
) -> Response {
    use base64::Engine as _;
    if let Err(error) = body.compatibility.require_current() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response();
    }
    let csr_der = match base64::engine::general_purpose::STANDARD.decode(&body.csr_b64) {
        Ok(der) => der,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid CSR: {e}") })),
            )
                .into_response();
        }
    };
    match ask_agent(&state.cmd_tx, |response| AgentCommand::JoinIssue {
        token: body.token,
        node_id: body.node_id,
        csr_der,
        response,
    })
    .await
    {
        Ok(Ok(bundle)) => Json(bundle).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(response) => response,
    }
}

// ---------------------------------------------------------------------------
// Chaos testing endpoints
// ---------------------------------------------------------------------------

/// Show the locally replicated node-experiment reservation, if any.
async fn chaos_status_handler(State(state): State<ApiState>) -> Response {
    let reservation = match &state.council {
        Some(council) => council.desired_state().await.node_fault_reservations.active,
        None => None,
    };
    Json(serde_json::json!({
        "node_fault_reservation": reservation.map(|grant| serde_json::json!({
            "sequence": grant.sequence,
            "target_node": grant.request.target_node,
            "fault_type": grant.request.fault_type,
            "cleanup_after_unix_ms": grant.cleanup_after_unix_ms,
        })),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Volume snapshots (Phase 12 E2)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
struct SnapshotCreateBody {
    /// Container mount path; omitted = every provisioned volume.
    volume: Option<String>,
    /// Custom snapshot name; omitted = unix-seconds timestamp.
    name: Option<String>,
}

#[derive(serde::Deserialize)]
struct SnapshotRestoreBody {
    name: String,
}

/// Map snapshot failures to honest status codes: a running app is a
/// conflict, missing things are 404, a non-btrfs volume is the
/// client's setup problem, anything else is ours.
fn snapshot_error_response(error: &crate::bun::BunError) -> Response {
    use crate::grill::snapshot::SnapshotError;
    let status = match error {
        crate::bun::BunError::Snapshot(SnapshotError::AppRunning { .. }) => StatusCode::CONFLICT,
        crate::bun::BunError::Snapshot(
            SnapshotError::NotFound { .. } | SnapshotError::NoVolumes { .. },
        ) => StatusCode::NOT_FOUND,
        crate::bun::BunError::Snapshot(
            SnapshotError::UnsupportedFilesystem { .. } | SnapshotError::TestStorage,
        ) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}

async fn snapshot_create_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((namespace, app)): Path<(String, String)>,
    body: Option<Json<SnapshotCreateBody>>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    let Json(body) = body.unwrap_or_default();
    match ask_agent(&state.cmd_tx, |response| AgentCommand::SnapshotCreate {
        namespace,
        app_name: app,
        volume: body.volume,
        name: body.name,
        response,
    })
    .await
    {
        Ok(Ok(metas)) => (StatusCode::CREATED, Json(serde_json::json!(metas))).into_response(),
        Ok(Err(e)) => snapshot_error_response(&e),
        Err(response) => response,
    }
}

async fn snapshot_list_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((namespace, app)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::SnapshotList {
        namespace,
        app_name: app,
        response,
    })
    .await
    {
        Ok(Ok(metas)) => Json(serde_json::json!(metas)).into_response(),
        Ok(Err(e)) => snapshot_error_response(&e),
        Err(response) => response,
    }
}

async fn snapshot_restore_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((namespace, app)): Path<(String, String)>,
    Json(body): Json<SnapshotRestoreBody>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::SnapshotRestore {
        namespace,
        app_name: app,
        name: body.name,
        response,
    })
    .await
    {
        Ok(Ok(())) => Json(serde_json::json!({ "restored": true })).into_response(),
        Ok(Err(e)) => snapshot_error_response(&e),
        Err(response) => response,
    }
}

async fn snapshot_delete_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((namespace, app, name)): Path<(String, String, String)>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::SnapshotDelete {
        namespace,
        app_name: app,
        name,
        response,
    })
    .await
    {
        Ok(Ok(())) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Ok(Err(e)) => snapshot_error_response(&e),
        Err(response) => response,
    }
}

/// Inject a fault (Smoker).
async fn fault_inject_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(mut request): Json<crate::smoker::types::FaultRequest>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_user(
        auth.as_deref(),
        crate::sesame::types::ApiRole::Deployer,
    ) {
        return resp;
    }
    if request.fault_type.is_node_targeted() {
        let Some(auth) = auth.as_deref() else {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        };
        let operation = if matches!(
            request.fault_type,
            crate::smoker::types::FaultType::NodePressure { .. }
        ) {
            crate::testkit::safety::OperationPermission::SaturateCapacity
        } else {
            crate::testkit::safety::OperationPermission::AlterNodeState
        };
        if let Err(response) = state.static_capabilities.test_policy.authorise(
            operation,
            &crate::testkit::safety::OperationAuthorisation {
                principal: &auth.principal_id,
                role: auth.role,
                acknowledged: request.acknowledged,
            },
        ) {
            return (StatusCode::FORBIDDEN, response.to_string()).into_response();
        }
        let Some(target_node) = request
            .target_node
            .as_deref()
            .filter(|target| !target.is_empty())
        else {
            return (
                StatusCode::BAD_REQUEST,
                "node-targeted faults require target_node",
            )
                .into_response();
        };
        if let Err(response) = check_node_fault_cluster_safety(&state, &request).await {
            return response;
        }
        // A node with no cluster identity can't decide whether it *is* the
        // target, so applying locally would pressure/kill the wrong (unnamed)
        // node. Refuse rather than mis-route.
        let Some(self_name) = state.node_name.as_deref() else {
            return (
                StatusCode::BAD_REQUEST,
                "this node has no cluster identity; cannot route node-targeted faults",
            )
                .into_response();
        };
        if self_name != target_node {
            return forward_node_fault(&state, target_node, &headers, &request).await;
        }
    } else {
        // Workload fault: normalise the namespace (apps default to `default`)
        // and enforce the caller's token scope against it, so a Deployer scoped
        // to one namespace cannot inject a fault into another tenant's
        // same-named service (AUTH1 for faults). The normalised namespace is
        // written back so the agent targets only the intended tenant.
        let namespace = request
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        request.namespace = Some(namespace.clone());
        if let Err(response) = crate::sesame::auth::authorize_scoped(
            auth.as_deref(),
            &request.target_service,
            &namespace,
        ) {
            return response;
        }
        let (principal, role) = auth
            .as_deref()
            .map(|auth| (auth.principal_id.as_str(), auth.role))
            .unwrap_or(("local-bootstrap", crate::sesame::types::ApiRole::Admin));
        if let Err(response) = state.static_capabilities.test_policy.authorise(
            crate::testkit::safety::OperationPermission::InjectWorkloadFaults,
            &crate::testkit::safety::OperationAuthorisation {
                principal,
                role,
                acknowledged: request.acknowledged,
            },
        ) {
            return (StatusCode::FORBIDDEN, response.to_string()).into_response();
        }
        // Workload faults act on processes, so they have to reach the node
        // that runs them. A cluster member routes every one, including those
        // it keeps for itself, so the replica rail always sees the whole
        // service.
        if let Some(self_name) = state.node_name.clone()
            && state.membership.is_some()
        {
            return route_workload_fault(&state, auth.as_deref(), &headers, request, &self_name)
                .await;
        }
    }

    // The caller controls the JSON body, so it cannot be the audit identity.
    // Token names are already authenticated by the middleware.
    request.injected_by = auth
        .as_deref()
        .map(|auth| auth.token_name.clone())
        .unwrap_or_else(|| "local-bootstrap".to_string());
    let reservation = if request.fault_type.is_node_targeted() {
        match prepare_and_reserve_node_fault(&state, request.clone()).await {
            Ok(grant) => {
                request = grant.request.clone();
                Some(grant)
            }
            Err(response) => return *response,
        }
    } else {
        None
    };
    match apply_fault_locally(&state, auth.as_deref(), request, reservation, None).await {
        Ok(summary) => Json(summary).into_response(),
        Err(response) => response,
    }
}

/// Apply a fault on this node and record its audit event.
///
/// `replica_evidence` carries the cluster-wide replica counts a routed
/// workload fault was judged against; `None` keeps the agent's local view.
// `Response` is large but it IS the HTTP reply to send on failure;
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
async fn apply_fault_locally(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    mut request: crate::smoker::types::FaultRequest,
    reservation: Option<crate::smoker::reservation::NodeFaultReservation>,
    replica_evidence: Option<crate::smoker::types::ReplicaEvidence>,
) -> Result<crate::smoker::types::FaultSummary, Response> {
    request.injected_by = auth
        .map(|auth| auth.token_name.clone())
        .unwrap_or_else(|| "local-bootstrap".to_string());
    let audit_principal = auth
        .map(|auth| auth.principal_id.clone())
        .unwrap_or_else(|| "local-bootstrap".to_string());
    let audit_target_node = request.target_node.clone();
    let audit_target_service = request.target_service.clone();
    let audit_target_instance = request.target_instance.clone();
    let audit_fault_type = serde_json::to_value(&request.fault_type)
        .ok()
        .and_then(|value| value.get("type")?.as_str().map(str::to_string))
        .unwrap_or_else(|| request.fault_type.to_string());
    let audit_duration_seconds = request.duration.as_secs();
    let audit_reason = request.reason.clone();
    match ask_agent(&state.cmd_tx, |response| AgentCommand::InjectFault {
        reservation: reservation.map(Box::new),
        request,
        replica_evidence,
        response,
    })
    .await
    {
        Ok(Ok(summary)) => {
            if let Some(events) = &state.events {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let mut details = std::collections::BTreeMap::from([
                    ("fault_id".to_string(), summary.id.to_string()),
                    ("fault_type".to_string(), audit_fault_type.clone()),
                    (
                        "duration_seconds".to_string(),
                        audit_duration_seconds.to_string(),
                    ),
                ]);
                if let Some(instance) = audit_target_instance {
                    details.insert("target_instance".to_string(), instance);
                }
                if let Some(reason) = audit_reason {
                    details.insert("reason".to_string(), reason);
                }
                events
                    .write()
                    .await
                    .record_audit(crate::bun::events::AuditEvent {
                        timestamp,
                        kind: crate::bun::events::EventKind::Fault,
                        severity: crate::bun::events::EventSeverity::Warning,
                        action: "fault.injected".to_string(),
                        principal: audit_principal.clone(),
                        app: (!audit_target_service.is_empty()).then_some(audit_target_service),
                        namespace: None,
                        node: audit_target_node,
                        details,
                        message: format!(
                            "fault {} ({}) injected for {}s by principal {}",
                            summary.id, summary.fault_type, audit_duration_seconds, audit_principal
                        ),
                    });
            }
            Ok(summary)
        }
        Ok(Err(e)) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response()),
        Err(response) => Err(response),
    }
}

struct FaultAudit<'a> {
    action: &'a str,
    principal: &'a str,
    severity: crate::bun::events::EventSeverity,
    app: Option<String>,
    node: Option<String>,
    details: std::collections::BTreeMap<String, String>,
    message: String,
}

async fn record_fault_audit(state: &ApiState, audit: FaultAudit<'_>) {
    let Some(events) = &state.events else {
        return;
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    events
        .write()
        .await
        .record_audit(crate::bun::events::AuditEvent {
            timestamp,
            kind: crate::bun::events::EventKind::Fault,
            severity: audit.severity,
            action: audit.action.to_string(),
            principal: audit.principal.to_string(),
            app: audit.app,
            namespace: None,
            node: audit.node,
            details: audit.details,
            message: audit.message,
        });
}

/// Re-evaluate node safety from API-owned live cluster state before routing.
///
/// Fault registries are node-local. A killed voter is therefore counted from
/// the replicated voter set minus live SWIM members, so a request reaching a
/// different node cannot silently exceed quorum. The target agent repeats its
/// local checks immediately before applying the effect.
// `Response` is large but it IS the HTTP reply to send on failure —
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
async fn check_node_fault_cluster_safety(
    state: &ApiState,
    request: &crate::smoker::types::FaultRequest,
) -> Result<(), Response> {
    let Some(council) = &state.council else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety requires live council evidence",
        )
            .into_response());
    };
    let metrics = council.metrics().borrow().clone();
    let raft_membership = metrics.membership_config.membership();
    let council_voters: std::collections::BTreeSet<_> = raft_membership.voter_ids().collect();
    if council_voters.is_empty() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety requires a known council membership",
        )
            .into_response());
    }
    let Some(membership) = &state.membership else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety requires live membership evidence",
        )
            .into_response());
    };
    let members = membership.read().await;
    let alive_voters: std::collections::BTreeSet<_> = members
        .iter()
        .map(|member| crate::cluster::identity::raft_id_from_name(&member.node_id.0))
        .collect();
    let unavailable_council_nodes = council_voters.difference(&alive_voters).count() as u32;
    let Some(leader) = metrics.current_leader else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety requires a known council leader",
        )
            .into_response());
    };
    let Some(leader_node_id) = members
        .iter()
        .find(|member| crate::cluster::identity::raft_id_from_name(&member.node_id.0) == leader)
        .map(|member| member.node_id.0.clone())
    else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety cannot map the council leader to live membership",
        )
            .into_response());
    };
    let context = crate::smoker::types::SafetyContext {
        council_size: council_voters.len() as u32,
        council_nodes_with_active_faults: unavailable_council_nodes,
        leader_node_id,
        total_nodes: members.len().max(council_voters.len()) as u32,
        nodes_with_active_faults: unavailable_council_nodes,
        target_service_replicas: 0,
        target_service_faulted_replicas: 0,
    };
    let decision = crate::smoker::safety::evaluate_safety(request, &context);
    if decision.approved {
        Ok(())
    } else {
        let reason = decision
            .violation
            .map(|violation| violation.to_string())
            .unwrap_or_else(|| "node fault safety check failed".to_string());
        Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct NodeFaultPreparation {
    boot_id: String,
    request: crate::smoker::types::FaultRequest,
}

/// The public endpoint remains an operator action; only a trusted target API
/// may obtain the internal grant after it has checked its own server policy.
async fn node_fault_reserve_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(prepared): Json<NodeFaultPreparation>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    match reserve_node_fault_on_leader(&state, prepared).await {
        Ok(grant) => Json(grant).into_response(),
        Err(response) => *response,
    }
}

#[derive(Serialize, Deserialize)]
struct NodeFaultFenceRequest {
    reservation: crate::smoker::reservation::NodeFaultReservation,
    only_if_finished: bool,
}

async fn node_fault_fence_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(request): Json<NodeFaultFenceRequest>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    if request.reservation.request.target_node.as_deref() != state.node_name.as_deref() {
        return (
            StatusCode::BAD_REQUEST,
            "node fault fence targets another node",
        )
            .into_response();
    }
    match fence_node_fault_locally(&state, request.reservation, request.only_if_finished).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    }
}

async fn prepare_and_reserve_node_fault(
    state: &ApiState,
    request: crate::smoker::types::FaultRequest,
) -> Result<crate::smoker::reservation::NodeFaultReservation, Box<Response>> {
    let operation = async {
        let (response, receiver) = oneshot::channel();
        state
            .cmd_tx
            .send(AgentCommand::PrepareNodeFault { request, response })
            .await
            .map_err(|_| "agent unavailable".to_string())?;
        receiver
            .await
            .map_err(|_| "agent dropped preparation response".to_string())?
            .map_err(|error| error.to_string())
    };
    let (boot_id, request) =
        match tokio::time::timeout(std::time::Duration::from_secs(5), operation).await {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return Err((StatusCode::BAD_REQUEST, error).into_response().into()),
            Err(_) => {
                return Err((
                    StatusCode::GATEWAY_TIMEOUT,
                    "node fault preparation timed out",
                )
                    .into_response()
                    .into());
            }
        };
    let prepared = NodeFaultPreparation { boot_id, request };
    let Some(council) = &state.council else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety requires live council evidence",
        )
            .into_response()
            .into());
    };
    if council.is_leader().await {
        return reserve_node_fault_on_leader(state, prepared).await;
    }
    let Some(leader) = leader_api_url(state, council).await else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault safety requires a known council leader",
        )
            .into_response()
            .into());
    };
    let bytes = post_node_fault_internal(state, format!("{leader}/v1/chaos/reserve"), &prepared)
        .await
        .map_err(|error| Box::new((StatusCode::SERVICE_UNAVAILABLE, error).into_response()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| Box::new((StatusCode::BAD_GATEWAY, error.to_string()).into_response()))
}

async fn reserve_node_fault_on_leader(
    state: &ApiState,
    prepared: NodeFaultPreparation,
) -> Result<crate::smoker::reservation::NodeFaultReservation, Box<Response>> {
    check_node_fault_cluster_safety(state, &prepared.request).await?;
    let council = state
        .council
        .as_ref()
        .ok_or_else(|| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?;
    if !council.is_leader().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault leader changed; retry",
        )
            .into_response()
            .into());
    }
    let metrics = council.metrics().borrow().clone();
    let voters: std::collections::BTreeSet<_> =
        metrics.membership_config.membership().voter_ids().collect();
    let membership = state
        .membership
        .as_ref()
        .ok_or_else(|| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?;
    let members = membership.read().await;
    if !members
        .iter()
        .any(|member| Some(member.node_id.0.as_str()) == prepared.request.target_node.as_deref())
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "node fault target is not in live membership",
        )
            .into_response()
            .into());
    }
    let alive: std::collections::BTreeSet<_> = members
        .iter()
        .map(|member| crate::cluster::identity::raft_id_from_name(&member.node_id.0))
        .collect();
    drop(members);
    let ledger = council.desired_state().await.node_fault_reservations;
    let Some(sequence) = ledger.last_sequence.checked_add(1) else {
        return Err((StatusCode::CONFLICT, "node fault sequence exhausted")
            .into_response()
            .into());
    };
    let reservation = crate::smoker::reservation::NodeFaultReservation {
        sequence,
        boot_id: prepared.boot_id,
        cleanup_after_unix_ms: crate::testkit::lease::now_unix_millis()
            .saturating_add(prepared.request.duration.as_millis().min(u64::MAX as u128) as u64),
        request: prepared.request,
    };
    let write = council.write(crate::council::RaftRequest::ReserveNodeFault {
        reservation: Box::new(reservation.clone()),
        membership_log_id: *metrics.membership_config.log_id(),
        unavailable_voters: voters.difference(&alive).copied().collect(),
    });
    match tokio::time::timeout(std::time::Duration::from_secs(5), write).await {
        Ok(Ok(crate::council::CouncilResponse::Refused { reason })) => {
            Err((StatusCode::CONFLICT, reason).into_response().into())
        }
        Ok(Ok(_)) => Ok(reservation),
        Ok(Err(error)) => Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string())
            .into_response()
            .into()),
        Err(_) => Err((
            StatusCode::GATEWAY_TIMEOUT,
            "node fault reservation outcome unknown; capacity retained until fenced",
        )
            .into_response()
            .into()),
    }
}

async fn post_node_fault_internal<T: Serialize>(
    state: &ApiState,
    url: String,
    body: &T,
) -> Result<Vec<u8>, String> {
    let token = state
        .service_token
        .as_ref()
        .ok_or("node fault coordination requires a service identity")?;
    let operation = async {
        let mut response = state
            .cluster_http
            .client()
            .post(url)
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_FAULT_FORWARD_RESPONSE_BYTES {
                return Err("node fault coordination response exceeds 64 KiB".to_string());
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(format!(
                "node fault coordination refused ({status}): {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        Ok(bytes)
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), operation)
        .await
        .map_err(|_| "node fault coordination timed out; ownership remains reserved".to_string())?
}

async fn fence_node_fault_locally(
    state: &ApiState,
    reservation: crate::smoker::reservation::NodeFaultReservation,
    only_if_finished: bool,
) -> Result<(), String> {
    let operation = async {
        let (response, receiver) = oneshot::channel();
        state
            .cmd_tx
            .send(AgentCommand::FenceNodeFault {
                only_if_finished,
                reservation,
                response,
            })
            .await
            .map_err(|_| "agent unavailable".to_string())?;
        receiver
            .await
            .map_err(|_| "agent dropped fence response".to_string())?
            .map_err(|error| error.to_string())
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), operation)
        .await
        .map_err(|_| "node fault fence outcome unknown".to_string())?
}

fn spawn_node_fault_reaper(state: ApiState) {
    let Some(council) = state.council.clone() else {
        return;
    };
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = state.cmd_tx.closed() => return, _ = interval.tick() => {} }
            let cleanup = async {
                if !council.is_leader().await {
                    return;
                }
                let Some(grant) = council.desired_state().await.node_fault_reservations.active
                else {
                    return;
                };
                let only_if_finished =
                    grant.cleanup_after_unix_ms > crate::testkit::lease::now_unix_millis();
                let result = if grant.request.target_node.as_deref() == state.node_name.as_deref() {
                    fence_node_fault_locally(&state, grant.clone(), only_if_finished).await
                } else if let Some(target) = grant.request.target_node.as_deref() {
                    match target_node_api_url(&state, target, "/v1/chaos/fence").await {
                        Ok(url) => post_node_fault_internal(
                            &state,
                            url,
                            &NodeFaultFenceRequest {
                                reservation: grant.clone(),
                                only_if_finished,
                            },
                        )
                        .await
                        .map(|_| ()),
                        Err(_) => Err("node fault target is unavailable for fencing".to_string()),
                    }
                } else {
                    Err("node fault reservation has no target".to_string())
                };
                if result.is_ok() {
                    // A new leader either inherits this slot or sees the release.
                    // No deadline or failed acknowledgement can clear ownership.
                    let _ = council
                        .write(crate::council::RaftRequest::ReleaseNodeFault {
                            sequence: grant.sequence,
                        })
                        .await;
                }
            };
            tokio::select! {
                _ = state.cmd_tx.closed() => return,
                _ = tokio::time::timeout(std::time::Duration::from_secs(10), cleanup) => {}
            }
        }
    });
}

const MAX_FAULT_FORWARD_RESPONSE_BYTES: usize = 64 * 1024;

/// Send a node-level operation to the named node while preserving the caller's
/// credential. The target repeats role, policy and acknowledgement checks.
async fn forward_node_fault(
    state: &ApiState,
    target_node: &str,
    headers: &HeaderMap,
    request: &crate::smoker::types::FaultRequest,
) -> Response {
    let url = match target_node_api_url(state, target_node, "/v1/fault").await {
        Ok(url) => url,
        Err(response) => return response,
    };
    let forwarded = state.cluster_http.client().post(url).json(request);
    send_node_request(
        target_node,
        copy_forwarded_auth(forwarded, headers),
        "fault",
    )
    .await
}

/// How long a peer may take to report its instances or faults while a
/// workload fault is being routed. It stays well under the 5-second deadline
/// a forwarding node gives the owner, which gathers the same evidence again.
const FAULT_EVIDENCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Route a workload fault to the nodes that run its targets.
///
/// The node that receives the request plans one request per owner from live
/// cluster status, checks the replica rail against cluster-wide counts, then
/// applies its own share and forwards the rest under the caller's credential.
/// An owner receiving a forwarded share (its `target_node` names the owner)
/// repeats the same steps, so its own server policy and its own view of the
/// replica rail decide before anything happens there.
async fn route_workload_fault(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    headers: &HeaderMap,
    request: crate::smoker::types::FaultRequest,
    self_name: &str,
) -> Response {
    use crate::smoker::routing::{WorkloadInstance, plan_workload_fault, replica_evidence};

    if request.fault_type.acts_on_callers() {
        return route_network_fault(state, auth, headers, request, self_name).await;
    }
    let namespace = request.namespace.clone().unwrap_or_default();
    let (statuses, faults) = tokio::join!(
        collect_cluster_statuses(state, FAULT_EVIDENCE_TIMEOUT),
        collect_cluster_faults(state, FAULT_EVIDENCE_TIMEOUT),
    );
    // A peer that didn't answer contributes no replicas, which only makes the
    // replica rail stricter.
    let statuses = match statuses {
        Ok((statuses, _unreachable)) => statuses,
        Err(error) => return unavailable_response(error),
    };
    let instances: Vec<WorkloadInstance> = statuses
        .into_iter()
        .filter(|status| {
            status.instance.app_name == request.target_service
                && status.instance.namespace == namespace
        })
        .map(|status| WorkloadInstance {
            running: status.instance.state == "running",
            node: status.node,
            instance_id: status.instance.id,
        })
        .collect();
    let evidence = replica_evidence(&request, &instances, &faults.0);

    let context = crate::smoker::types::SafetyContext {
        // Workload faults only meet the replica rail; zeroed cluster fields
        // make the node rails stand aside, as they do in standalone mode.
        council_size: 0,
        council_nodes_with_active_faults: 0,
        leader_node_id: String::new(),
        total_nodes: 0,
        nodes_with_active_faults: 0,
        target_service_replicas: evidence.replicas,
        target_service_faulted_replicas: evidence.faulted_replicas,
    };
    let check = crate::smoker::safety::evaluate_safety(&request, &context);
    if !check.approved {
        let reason = check
            .violation
            .map(|violation| violation.to_string())
            .unwrap_or_else(|| "safety check failed".to_string());
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response();
    }

    let plan = match plan_workload_fault(&request, &instances) {
        Ok(plan) => plan,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    };

    send_routed_faults(state, auth, headers, plan, self_name, Some(evidence)).await
}

/// Route a network fault to the nodes that run its callers.
///
/// Network faults act where a connection starts, so a destination-wide fault
/// goes to every live node and a `--from` fault to the nodes that run the
/// source app in the fault's namespace. No replica rail applies: nothing is
/// stopped, only traffic towards the target changes.
async fn route_network_fault(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    headers: &HeaderMap,
    request: crate::smoker::types::FaultRequest,
    self_name: &str,
) -> Response {
    use crate::smoker::routing::{WorkloadInstance, plan_network_fault};

    let namespace = request.namespace.clone().unwrap_or_default();
    let mut nodes: Vec<String> = match &state.membership {
        Some(membership) => membership
            .read()
            .await
            .iter()
            .map(|member| member.node_id.0.clone())
            .collect(),
        None => Vec::new(),
    };
    nodes.push(self_name.to_string());
    let sources: Vec<WorkloadInstance> = match request.fault_type.source_app() {
        Some(source) => match collect_cluster_statuses(state, FAULT_EVIDENCE_TIMEOUT).await {
            Ok((statuses, _unreachable)) => statuses
                .into_iter()
                .filter(|status| {
                    status.instance.app_name == source && status.instance.namespace == namespace
                })
                .map(|status| WorkloadInstance {
                    running: status.instance.state == "running",
                    node: status.node,
                    instance_id: status.instance.id,
                })
                .collect(),
            Err(error) => return unavailable_response(error),
        },
        None => Vec::new(),
    };
    let plan = match plan_network_fault(&request, &nodes, &sources) {
        Ok(plan) => plan,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    };
    send_routed_faults(state, auth, headers, plan, self_name, None).await
}

/// Apply this node's share of a routed fault and forward every other share,
/// returning one summary whose `routed` lists the rest.
async fn send_routed_faults(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    headers: &HeaderMap,
    plan: Vec<crate::smoker::routing::RoutedFault>,
    self_name: &str,
    evidence: Option<crate::smoker::types::ReplicaEvidence>,
) -> Response {
    let mut applied: Vec<crate::smoker::types::FaultSummary> = Vec::new();
    for routed in plan {
        let result = if routed.node == self_name {
            apply_fault_locally(state, auth, routed.request, None, evidence).await
        } else {
            forward_workload_fault(state, &routed.node, headers, &routed.request).await
        };
        match result {
            Ok(mut summary) => {
                summary.node = Some(routed.node);
                applied.push(summary);
            }
            Err(response) if applied.is_empty() => return response,
            Err(response) => {
                return partial_fault_response(&routed.node, response, applied).await;
            }
        }
    }
    let mut applied = applied.into_iter();
    let Some(mut first) = applied.next() else {
        return (StatusCode::BAD_REQUEST, "fault matched no instances").into_response();
    };
    first.routed = applied.collect();
    Json(first).into_response()
}

/// Forward one owner's share of a workload fault and read back its summary.
// `Response` is large but it IS the HTTP reply to send on failure;
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
async fn forward_workload_fault(
    state: &ApiState,
    node: &str,
    headers: &HeaderMap,
    request: &crate::smoker::types::FaultRequest,
) -> Result<crate::smoker::types::FaultSummary, Response> {
    let response = forward_node_fault(state, node, headers, request).await;
    if !response.status().is_success() {
        return Err(response);
    }
    let body = axum::body::to_bytes(response.into_body(), MAX_FAULT_FORWARD_RESPONSE_BYTES)
        .await
        .map_err(|error| {
            (
                StatusCode::BAD_GATEWAY,
                format!("failed to read fault response from {node}: {error}"),
            )
                .into_response()
        })?;
    serde_json::from_slice(&body).map_err(|error| {
        (
            StatusCode::BAD_GATEWAY,
            format!("node {node} returned an unreadable fault summary: {error}"),
        )
            .into_response()
    })
}

/// A routed fault took effect on some owners and failed on another. Report
/// both, so the operator can clear what did land.
async fn partial_fault_response(
    failed_node: &str,
    failure: Response,
    applied: Vec<crate::smoker::types::FaultSummary>,
) -> Response {
    let status = failure.status();
    let body = axum::body::to_bytes(failure.into_body(), MAX_FAULT_FORWARD_RESPONSE_BYTES)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let landed: Vec<String> = applied
        .iter()
        .map(|summary| {
            format!(
                "{} on {}",
                summary.id,
                summary.node.as_deref().unwrap_or("?")
            )
        })
        .collect();
    (
        status,
        Json(serde_json::json!({
            "error": format!(
                "fault failed on {failed_node} ({body}) after it took effect as {}",
                landed.join(", ")
            ),
            "applied": applied,
        })),
    )
        .into_response()
}

/// Every node's active faults, each tagged with the node that holds it, plus
/// one message per peer that didn't answer.
async fn collect_cluster_faults(
    state: &ApiState,
    peer_timeout: std::time::Duration,
) -> (Vec<crate::smoker::types::FaultSummary>, Vec<String>) {
    let local_name = local_node_name(state);
    let mut failures = Vec::new();
    let mut faults: Vec<_> = match ask_agent(&state.cmd_tx, |response| AgentCommand::ListFaults {
        response,
    })
    .await
    {
        Ok(local) => local
            .into_iter()
            .map(|mut fault| {
                fault.node = Some(local_name.clone());
                fault
            })
            .collect(),
        Err(_) => {
            failures.push(format!("node {local_name}: agent unavailable"));
            Vec::new()
        }
    };
    let members = match &state.membership {
        Some(membership) => membership.read().await.clone(),
        None => Vec::new(),
    };
    let requests = futures_util::stream::iter(
        members
            .into_iter()
            .filter(|member| member.node_id.0 != local_name)
            .map(|member| async move {
                let name = member.node_id.0;
                let result = tokio::time::timeout(peer_timeout, async {
                    let url = state
                        .cluster_http
                        .url(&member.address.to_string(), "/v1/fault");
                    let mut request = state.cluster_http.client().get(url);
                    if let Some(token) = &state.service_token {
                        request = request.bearer_auth(token);
                    }
                    request
                        .send()
                        .await?
                        .error_for_status()?
                        .json::<Vec<crate::smoker::types::FaultSummary>>()
                        .await
                })
                .await;
                match result {
                    Ok(Ok(faults)) => Ok(faults
                        .into_iter()
                        .map(|mut fault| {
                            fault.node = Some(name.clone());
                            fault
                        })
                        .collect::<Vec<_>>()),
                    Ok(Err(error)) => Err(format!("node {name}: {error}")),
                    Err(_) => Err(format!("node {name} timed out")),
                }
            }),
    )
    .buffer_unordered(8);
    tokio::pin!(requests);
    while let Some(result) = requests.next().await {
        match result {
            Ok(node_faults) => faults.extend(node_faults),
            Err(failure) => failures.push(failure),
        }
    }
    failures.sort();
    faults.sort_by(|left, right| (&left.node, left.id).cmp(&(&right.node, right.id)));
    (faults, failures)
}

/// Resolve a live cluster member to one of its API URLs.
// `Response` is large but it IS the HTTP reply to send on failure —
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
async fn target_node_api_url(
    state: &ApiState,
    target_node: &str,
    path: &str,
) -> Result<String, Response> {
    let Some(membership) = &state.membership else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "cluster membership is unavailable",
        )
            .into_response());
    };
    let address = {
        let members = membership.read().await;
        members
            .iter()
            .find(|member| member.node_id == crate::meat::NodeId::new(target_node))
            .map(|member| member.address)
    };
    let Some(address) = address else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("target node {target_node} is not alive or is unknown"),
        )
            .into_response());
    };
    Ok(state.cluster_http.url(&address.to_string(), path))
}

/// Resolve a member gossip still knows, live or not, to one of its API URLs.
///
/// A live member resolves as in [`target_node_api_url`]; otherwise
/// [`KnownMembers`] supplies the address of a suspect or dead one. For reads
/// and reversals only: injecting into a node the cluster has lost stays
/// refused.
// `Response` is large but it IS the HTTP reply to send on failure.
#[allow(clippy::result_large_err)]
async fn known_node_api_url(
    state: &ApiState,
    known: Option<&KnownMembers>,
    target_node: &str,
    path: &str,
) -> Result<String, Response> {
    let live = target_node_api_url(state, target_node, path).await;
    let Some(known) = known.filter(|_| live.is_err()) else {
        return live;
    };
    let address = known
        .0
        .read()
        .await
        .iter()
        .find(|member| member.node_id == crate::meat::NodeId::new(target_node))
        .map(|member| member.address);
    match address {
        Some(address) => Ok(state.cluster_http.url(&address.to_string(), path)),
        None => live,
    }
}

/// Preserve the end user's credential so the target node repeats every
/// authentication and server-policy check.
fn copy_forwarded_auth(
    mut request: reqwest::RequestBuilder,
    headers: &HeaderMap,
) -> reqwest::RequestBuilder {
    for name in [
        axum::http::header::AUTHORIZATION,
        axum::http::header::COOKIE,
    ] {
        if let Some(value) = headers.get(&name) {
            request = request.header(name.as_str(), value.as_bytes());
        }
    }
    request
}

/// Complete a forwarded node request with one deadline and a bounded body.
async fn send_node_request(
    target_node: &str,
    request: reqwest::RequestBuilder,
    operation: &str,
) -> Response {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let response = match tokio::time::timeout_at(deadline, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("failed to forward node {operation} to {target_node}: {error}"),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                format!("node {operation} request to {target_node} timed out"),
            )
                .into_response();
        }
    };
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if response
        .content_length()
        .is_some_and(|length| length > MAX_FAULT_FORWARD_RESPONSE_BYTES as u64)
    {
        return (
            StatusCode::BAD_GATEWAY,
            "target node response exceeded the 64 KiB limit",
        )
            .into_response();
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = match tokio::time::timeout_at(deadline, stream.next()).await {
        Ok(chunk) => chunk,
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                format!("node {operation} response from {target_node} timed out"),
            )
                .into_response();
        }
    } {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("failed to read node {operation} response from {target_node}: {error}"),
                )
                    .into_response();
            }
        };
        if body.len().saturating_add(chunk.len()) > MAX_FAULT_FORWARD_RESPONSE_BYTES {
            return (
                StatusCode::BAD_GATEWAY,
                "target node response exceeded the 64 KiB limit",
            )
                .into_response();
        }
        body.extend_from_slice(&chunk);
    }
    (status, body).into_response()
}

#[derive(Debug, Default, Deserialize)]
struct FaultClearQuery {
    node: Option<String>,
    #[serde(default)]
    acknowledged: bool,
}

/// Clear a specific fault by ID.
async fn fault_clear_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    known: Option<axum::Extension<KnownMembers>>,
    headers: HeaderMap,
    Path(id): Path<u64>,
    Query(query): Query<FaultClearQuery>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_user(
        auth.as_deref(),
        crate::sesame::types::ApiRole::Deployer,
    ) {
        return resp;
    }
    let (principal, role) = auth
        .as_deref()
        .map(|auth| (auth.principal_id.as_str(), auth.role))
        .unwrap_or(("local-bootstrap", crate::sesame::types::ApiRole::Admin));
    let caller = crate::testkit::safety::OperationAuthorisation {
        principal,
        role,
        acknowledged: query.acknowledged,
    };
    let allow_workload_fault = state
        .static_capabilities
        .test_policy
        .authorise_reversal(
            crate::testkit::safety::OperationPermission::InjectWorkloadFaults,
            &caller,
        )
        .is_ok();
    let allow_node_pressure = state
        .static_capabilities
        .test_policy
        .authorise_reversal(
            crate::testkit::safety::OperationPermission::SaturateCapacity,
            &caller,
        )
        .is_ok();
    let allow_node_fault =
        if let Some(target_node) = query.node.as_deref().filter(|target| !target.is_empty()) {
            // Node routing is not itself authority. Preserve the three independent
            // reversal grants and let the owning agent inspect the actual fault
            // before it removes anything.
            let allow_node_fault = state
                .static_capabilities
                .test_policy
                .authorise_reversal(
                    crate::testkit::safety::OperationPermission::AlterNodeState,
                    &caller,
                )
                .is_ok();
            if state
                .node_name
                .as_deref()
                .is_some_and(|name| name != target_node)
            {
                return forward_node_fault_clear(
                    &state,
                    known.as_deref(),
                    target_node,
                    &headers,
                    id,
                    query.acknowledged,
                )
                .await;
            }
            allow_node_fault
        } else {
            false
        };
    let has_any_reversal_grant = allow_workload_fault || allow_node_fault || allow_node_pressure;
    if query.node.is_some() && !has_any_reversal_grant {
        return (
            StatusCode::FORBIDDEN,
            "cluster policy does not allow reversal of this fault class",
        )
            .into_response();
    }
    if query.node.is_none() && !allow_workload_fault {
        return (
            StatusCode::FORBIDDEN,
            "workload fault reversal requires inject_workload_faults authorisation",
        )
            .into_response();
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::ClearFault {
        fault_id: id,
        allow_workload_fault,
        allow_node_fault,
        allow_node_pressure,
        response,
    })
    .await
    {
        Ok(Ok(clearance)) => {
            if let Some(sequence) = clearance.reservation
                && !wait_for_node_fault_release(&state, sequence).await
            {
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(serde_json::json!({
                        "error": format!(
                            "fault {id} is reversed on this node, but the cluster has not yet \
                             released its reservation; retry the clear before injecting again"
                        )
                    })),
                )
                    .into_response();
            }
            record_fault_audit(
                &state,
                FaultAudit {
                    action: "fault.cleared",
                    principal,
                    severity: crate::bun::events::EventSeverity::Info,
                    app: None,
                    node: query.node,
                    details: std::collections::BTreeMap::from([(
                        "fault_id".to_string(),
                        id.to_string(),
                    )]),
                    message: format!("fault {id} cleared by principal {principal}"),
                },
            )
            .await;
            Json(serde_json::json!({ "message": clearance.message })).into_response()
        }
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(response) => response,
    }
}

/// How long a clear waits for the council to release a node fault's
/// reservation. It stays under the 5-second deadline a forwarding node gives
/// the owning node, so a forwarded clear reports this node's own verdict.
const NODE_FAULT_RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// Wait until the council no longer holds the reservation a cleared node fault
/// owned, or the deadline passes. Returns whether it was released.
///
/// The leader's reaper releases a reservation only after it has fenced the
/// target node through its own live membership view. So once this returns
/// `true`, the leader that will judge the next node fault has already seen
/// this node back, and the single experiment slot is free again.
async fn wait_for_node_fault_release(state: &ApiState, sequence: u64) -> bool {
    let Some(council) = &state.council else {
        return true;
    };
    let deadline = tokio::time::Instant::now() + NODE_FAULT_RELEASE_TIMEOUT;
    loop {
        let released = council
            .desired_state()
            .await
            .node_fault_reservations
            .active
            .is_none_or(|grant| grant.sequence != sequence);
        if released {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Route manual reversal to the node which owns the local fault id.
async fn forward_node_fault_clear(
    state: &ApiState,
    known: Option<&KnownMembers>,
    target_node: &str,
    headers: &HeaderMap,
    fault_id: u64,
    acknowledged: bool,
) -> Response {
    let path = format!("/v1/fault/{fault_id}");
    // A node-killed target is dead to gossip but still holds its fault.
    let url = match known_node_api_url(state, known, target_node, &path).await {
        Ok(url) => url,
        Err(response) => return response,
    };
    let forwarded = state.cluster_http.client().delete(url).query(&[
        ("node", target_node),
        ("acknowledged", if acknowledged { "true" } else { "false" }),
    ]);
    send_node_request(
        target_node,
        copy_forwarded_auth(forwarded, headers),
        "fault reversal",
    )
    .await
}

/// Clear all active faults.
async fn fault_clear_all_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_user(
        auth.as_deref(),
        crate::sesame::types::ApiRole::Deployer,
    ) {
        return resp;
    }
    let (principal, role) = auth
        .as_deref()
        .map(|auth| (auth.principal_id.as_str(), auth.role))
        .unwrap_or(("local-bootstrap", crate::sesame::types::ApiRole::Admin));
    if let Err(error) = state.static_capabilities.test_policy.authorise_reversal(
        crate::testkit::safety::OperationPermission::InjectWorkloadFaults,
        &crate::testkit::safety::OperationAuthorisation {
            principal,
            role,
            acknowledged: false,
        },
    ) {
        return (StatusCode::FORBIDDEN, error.to_string()).into_response();
    }
    // `?service=NAME` clears only that service's faults; no query clears all
    // workload faults. An *empty* `?service=` is neither: every node-class
    // fault carries an empty `target_service`, so it would match them all —
    // reject it rather than let this Deployer-authorised path reverse Admin
    // faults by omission.
    let target = match params.get("service") {
        Some(service) if service.is_empty() => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "service must be non-empty; omit ?service to clear all workload faults"
                })),
            )
                .into_response();
        }
        Some(service) => match params.get("namespace") {
            // Confined clear: scope-check the named namespace, so a Deployer
            // scoped to one tenant cannot reverse another tenant's same-named
            // service faults (AUTH1).
            Some(namespace) => {
                if let Err(response) =
                    crate::sesame::auth::authorize_scoped(auth.as_deref(), service, namespace)
                {
                    return response;
                }
                Some((service.clone(), Some(namespace.clone())))
            }
            // Cross-namespace clear: reversing a service's faults in every
            // namespace is a cluster-wide action, so a scoped token is refused
            // and told to name a namespace it may touch (C3, as for reads).
            None => {
                if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
                    return response;
                }
                Some((service.clone(), None))
            }
        },
        None => None,
    };
    let command = |response| match target {
        Some((service, namespace)) => AgentCommand::ClearFaultsByService {
            service,
            namespace,
            response,
        },
        None => AgentCommand::ClearAllFaults { response },
    };
    match ask_agent(&state.cmd_tx, command).await {
        Ok(Ok(msg)) => {
            // Workload faults are routed to the nodes that run their targets,
            // so a clear has to reach those nodes too. Peers get `local=true`
            // and the caller's own credential, so each repeats every check.
            let msg = if params.get("local").is_some_and(|local| local == "true") {
                msg
            } else {
                let mut messages = vec![msg];
                messages.extend(clear_faults_on_peers(&state, &headers, &params).await);
                messages.join("; ")
            };
            let service = params.get("service").cloned();
            let mut details = std::collections::BTreeMap::new();
            let action = if let Some(service) = &service {
                details.insert("target_service".to_string(), service.clone());
                "fault.cleared-by-service"
            } else {
                "fault.cleared-all-workload"
            };
            record_fault_audit(
                &state,
                FaultAudit {
                    action,
                    principal,
                    severity: crate::bun::events::EventSeverity::Info,
                    app: service,
                    node: None,
                    details,
                    message: format!("{msg} by principal {principal}"),
                },
            )
            .await;
            Json(serde_json::json!({ "message": msg })).into_response()
        }
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(response) => response,
    }
}

/// Send a clear-all or clear-by-service to every other live member and
/// describe each answer. A peer that can't be reached is reported, not fatal:
/// its faults still expire on their own.
async fn clear_faults_on_peers(
    state: &ApiState,
    headers: &HeaderMap,
    params: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    let local_name = local_node_name(state);
    let members = match &state.membership {
        Some(membership) => membership.read().await.clone(),
        None => return Vec::new(),
    };
    let mut query: Vec<(&str, &str)> = params
        .iter()
        .filter(|(key, _)| matches!(key.as_str(), "service" | "namespace"))
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    query.push(("local", "true"));
    let mut messages = Vec::new();
    for member in members
        .into_iter()
        .filter(|member| member.node_id.0 != local_name)
    {
        let node = member.node_id.0;
        let url = state
            .cluster_http
            .url(&member.address.to_string(), "/v1/fault");
        let request = state.cluster_http.client().delete(url).query(&query);
        let response =
            send_node_request(&node, copy_forwarded_auth(request, headers), "fault clear").await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), MAX_FAULT_FORWARD_RESPONSE_BYTES)
            .await
            .unwrap_or_default();
        let text = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
        messages.push(if status.is_success() {
            format!("{node}: {text}")
        } else {
            format!("{node}: not cleared ({status}): {text}")
        });
    }
    messages
}

#[derive(Debug, Default, Deserialize)]
struct FaultListQuery {
    #[serde(default)]
    cluster: bool,
}

/// Every node's active faults, as `GET /v1/fault?cluster=true` returns them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterFaultList {
    /// Active faults, each tagged with the node that holds it.
    pub faults: Vec<crate::smoker::types::FaultSummary>,
    /// One message per node whose faults couldn't be read.
    pub warnings: Vec<String>,
}

/// List active faults: this node's by default, every node's with
/// `?cluster=true`.
async fn fault_list_handler(
    State(state): State<ApiState>,
    Query(query): Query<FaultListQuery>,
) -> Response {
    if query.cluster {
        let (faults, warnings) = collect_cluster_faults(&state, CLUSTER_STATUS_TIMEOUT).await;
        return Json(ClusterFaultList { faults, warnings }).into_response();
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::ListFaults {
        response,
    })
    .await
    {
        Ok(summaries) => Json(serde_json::json!(summaries)).into_response(),
        Err(response) => response,
    }
}

/// Resolve a service name to its VIP and backends.
async fn resolve_handler(State(state): State<ApiState>, Path(name): Path<String>) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Resolve {
        app_name: name.clone(),
        response,
    })
    .await
    {
        Ok(Some(info)) => Json(serde_json::json!(info)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("service {name:?} not found") })),
        )
            .into_response(),
        Err(response) => response,
    }
}

/// List all registered services.
async fn resolve_all_handler(State(state): State<ApiState>) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::ResolveAll {
        response,
    })
    .await
    {
        Ok(entries) => Json(serde_json::json!(entries)).into_response(),
        Err(response) => response,
    }
}

/// List all ingress routes.
async fn routes_handler(State(state): State<ApiState>) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Routes { response }).await {
        Ok(routes) => Json(serde_json::json!(routes)).into_response(),
        Err(response) => response,
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
    /// Restrict to one app's samples, matched against the `app` label
    /// (`namespace/app`). Set by the single-app cross-node fan-out so each
    /// node answers with only that app's local data; absent for node-wide
    /// dashboard queries.
    app: Option<String>,
    /// Keep only the newest N samples of each series (per-app queries).
    per_series: Option<u32>,
}

/// Window the per-app endpoint reads when the caller gives no `start`.
///
/// Callers want "what's happening now"; reading from the epoch made every
/// unbounded query scan (and cap) the whole retention period.
const APP_METRICS_DEFAULT_WINDOW_SECS: u64 = 15 * 60;

/// `GET /v1/metrics?name=X&start=S&end=E` — query time-series data.
///
/// Reads across every app and namespace on the node, so a scoped token is
/// refused (C3) and pointed at `/v1/metrics/app/{app}/{namespace}`, which can
/// filter. The cross-node fan-out presents the service token, which passes.
async fn metrics_query_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    let Some(mayo) = &state.mayo else {
        return Json(serde_json::json!({"error": "metrics not enabled"})).into_response();
    };

    let store = mayo.read().await;
    let name = params.name.as_deref().unwrap_or("*");
    let start = params.start.unwrap_or(0);
    // Clamped below u64::MAX: DataFusion 45's interval analysis
    // overflows (debug-build panic) computing the cardinality of a
    // full-domain unsigned range like `timestamp <= u64::MAX`.
    let end = params.end.unwrap_or(i64::MAX as u64).min(i64::MAX as u64);

    // When an `app` filter is present this is a leaf of the single-app
    // cross-node fan-out: answer with only that app's local samples. Every
    // caller-supplied string reaches the SQL literal, so escape each (OBS1).
    if let Some(app) = &params.app {
        let name = (name != "*").then_some(name);
        return match store
            .query_app(app, name, start, end, params.per_series)
            .await
        {
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
        };
    }

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
async fn metrics_summary_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
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
    ask_agent(&state.cmd_tx, |response| AgentCommand::Status { response })
        .await
        .unwrap_or_default()
}

/// Build dashboard app rows from instance statuses.
fn statuses_to_dashboard_apps(
    statuses: &[InstanceStatus],
    desired: &[crate::bun::diagnostics::DesiredAppEvidence],
) -> Vec<DashboardApp> {
    let mut rows = std::collections::BTreeMap::new();
    for app in desired {
        rows.insert(
            (app.namespace.clone(), app.app.clone()),
            DashboardApp {
                name: app.app.clone(),
                namespace: app.namespace.clone(),
                instances_running: 0,
                instances_desired: app.desired_replicas as usize,
                state: "pending".to_string(),
            },
        );
    }
    for instance in statuses {
        let Some(row) = rows.get_mut(&(instance.namespace.clone(), instance.app_name.clone()))
        else {
            continue;
        };
        if instance.state == "running" {
            row.instances_running += 1;
        }
        if matches!(instance.state.as_str(), "failed" | "unhealthy") {
            row.state = "unhealthy".into();
        }
    }
    for row in rows.values_mut() {
        if row.state != "unhealthy" && row.instances_running == row.instances_desired {
            row.state = if row.instances_desired == 0 {
                "stopped"
            } else {
                "running"
            }
            .into();
        }
    }
    rows.into_values().collect()
}

async fn gather_dashboard_apps(state: &ApiState) -> Result<Vec<DashboardApp>, String> {
    let (statuses, desired) =
        tokio::try_join!(cluster_statuses(state), gather_desired_apps(state))?;
    let statuses: Vec<_> = statuses.into_iter().map(|row| row.instance).collect();
    Ok(statuses_to_dashboard_apps(&statuses, &desired))
}

/// Build the dashboard data from current agent state.
async fn gather_dashboard_data(state: &ApiState) -> Result<DashboardData, String> {
    let apps = gather_dashboard_apps(state).await?;

    let (alert_count, alerts) = if let Some(ref evaluator) = state.alerts {
        let eval = evaluator.read().await;
        let firing = eval.firing_alerts();
        let count = firing.len();
        let alert_rows = firing
            .iter()
            .map(|a| crate::brioche::dashboard::DashboardAlert {
                labels: a.labels.clone(),
                name: a.rule_name.clone(),
                severity: format!("{:?}", a.severity),
                description: a.description.clone(),
            })
            .collect();
        (count, alert_rows)
    } else {
        (0, vec![])
    };

    let nodes = gather_dashboard_nodes(state).await;
    // The node count follows the real membership when we have it. A
    // standalone node with no gossip table still shows itself as one node.
    let node_count = if nodes.is_empty() { 1 } else { nodes.len() };

    Ok(DashboardData {
        cluster_name: String::new(),
        node_count,
        app_count: apps.len(),
        alert_count,
        apps,
        nodes,
        alerts,
    })
}

/// Build the dashboard node rows from the live gossip membership (AUTH7).
///
/// The membership table only holds nodes gossip currently considers alive, so
/// every row here is a live member. When the council is up we also count each
/// node's assigned apps from the desired state, giving the same per-node app
/// totals the placements endpoint serves. No membership table (standalone)
/// yields an empty list, and the caller falls back to a single-node view.
async fn gather_dashboard_nodes(state: &ApiState) -> Vec<crate::brioche::dashboard::DashboardNode> {
    let Some(membership) = &state.membership else {
        return vec![];
    };
    let members = membership.read().await;

    // App counts per node, when we can see the desired state.
    let mut app_counts: std::collections::HashMap<crate::meat::NodeId, usize> =
        std::collections::HashMap::new();
    if let Some(council) = &state.council {
        let desired = council.desired_state().await;
        for placements in desired.scheduling.values() {
            for placement in placements {
                *app_counts.entry(placement.node_id.clone()).or_insert(0) += 1;
            }
        }
    }

    members
        .iter()
        .map(|member| crate::brioche::dashboard::DashboardNode {
            name: member.node_id.0.clone(),
            state: "alive".to_string(),
            app_count: app_counts.get(&member.node_id).copied().unwrap_or(0),
        })
        .collect()
}

/// Return an HTML response.
fn html_response(html: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/html; charset=utf-8".parse().unwrap());
    (StatusCode::OK, headers, html).into_response()
}

/// `GET /` — serve the Brioche cluster overview dashboard.
async fn dashboard_handler(State(state): State<ApiState>) -> Response {
    match gather_dashboard_data(&state).await {
        Ok(data) => html_response(render_dashboard(&data)),
        Err(error) => unavailable_response(error),
    }
}

/// Names of the metrics an app's instances reported in the last five
/// minutes, for choosing its page's charts. Empty if the query fails or
/// takes more than three seconds: the page renders without those charts
/// rather than waiting on a slow node.
async fn scraped_metric_names(state: &ApiState, app: &str, namespace: &str) -> Vec<String> {
    let start = crate::mayo::types::Sample::now(0.0)
        .timestamp
        .saturating_sub(300);
    let query = app_metric_rows(state, app, namespace, None, start, i64::MAX as u64, Some(1));
    let Ok(Ok(result)) = tokio::time::timeout(std::time::Duration::from_secs(3), query).await
    else {
        return Vec::new();
    };
    let names: std::collections::BTreeSet<String> =
        result.data.into_iter().map(|row| row.metric_name).collect();
    names.into_iter().collect()
}

/// `GET /ui/app/{app}/{namespace}` — app detail page.
async fn app_detail_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    let (rows, desired) =
        match tokio::try_join!(cluster_statuses(&state), gather_desired_apps(&state)) {
            Ok(result) => result,
            Err(error) => return unavailable_response(error),
        };
    let instances: Vec<InstanceStatus> = rows
        .into_iter()
        .map(|row| row.instance)
        .filter(|instance| instance.app_name == app && instance.namespace == namespace)
        .collect();
    let summary = statuses_to_dashboard_apps(&instances, &desired)
        .into_iter()
        .find(|row| row.name == app && row.namespace == namespace);
    let (overall_state, desired_instances) = summary
        .map(|row| (row.state, row.instances_desired))
        .unwrap_or_else(|| ("unknown".to_string(), 0));

    let env = if let Some(council) = &state.council {
        let desired = council.desired_state().await;
        desired
            .apps
            .iter()
            .find(|(id, _)| id.name == app && id.namespace == namespace)
            .map(|(_, spec)| safe_env(&spec.env))
            .unwrap_or_default()
    } else {
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (response, receiver) = oneshot::channel();
            state
                .cmd_tx
                .send(AgentCommand::AppConfig {
                    app_name: app.clone(),
                    namespace: namespace.clone(),
                    response,
                })
                .await
                .map_err(|_| "agent unavailable")?;
            receiver
                .await
                .map_err(|_| "agent did not return app configuration")
        })
        .await;
        match result {
            Ok(Ok(Some(spec))) => safe_env(&spec.env),
            Ok(Ok(None)) => Vec::new(),
            Ok(Err(error)) => return unavailable_response(error.to_string()),
            Err(_) => return unavailable_response("app configuration query timed out".to_string()),
        }
    };

    // Get deploy history
    let deploy_history = if let Some(ref history) = state.deploy_history {
        let h = history.read().await;
        h.iter()
            .filter(|e| e.app_id.name == app && e.app_id.namespace == namespace)
            .cloned()
            .collect()
    } else {
        vec![]
    };

    let charts = crate::brioche::app_detail::app_charts(
        &app,
        &namespace,
        &scraped_metric_names(&state, &app, &namespace).await,
    );

    let data = AppDetailData {
        app_name: app,
        namespace,
        state: overall_state,
        desired_instances,
        instances,
        env,
        deploy_history,
        charts,
    };

    html_response(render_app_detail(&data))
}

/// `GET /ui/node/{name}` — node detail page.
async fn node_detail_handler(State(state): State<ApiState>, Path(name): Path<String>) -> Response {
    // M14: the old handler ignored `name` entirely — it rendered *this* node's
    // instances with a hard-coded `state: "alive"`, so clicking node B showed
    // node A's workloads labelled as B. Only this node knows its own running
    // instances, so show the instance list only when the request is for this
    // node; for any other node show its presence in gossip but no (misattributed)
    // workloads. Cross-node instance detail would need a fan-out and is left as a
    // follow-up.
    let is_self = state.node_name.as_deref() == Some(name.as_str());
    let in_membership = match &state.membership {
        Some(m) => m.read().await.iter().any(|info| info.node_id.0 == name),
        None => false,
    };
    let node_state = if is_self || in_membership {
        "alive"
    } else {
        "unknown"
    }
    .to_string();
    let statuses = if is_self {
        gather_statuses(&state).await
    } else {
        Vec::new()
    };

    let data = NodeDetailData {
        name,
        state: node_state,
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

/// `GET /ui/gitops` — Lettuce GitOps status page: current sync phase,
/// coordinator, last applied commit, and recent sync history (E).
async fn gitops_handler(State(state): State<ApiState>) -> Response {
    let sync = match &state.council {
        Some(council) => council
            .desired_state()
            .await
            .gitops_sync_state
            .unwrap_or_default(),
        None => crate::lettuce::types::SyncState::default(),
    };
    html_response(crate::brioche::gitops::render_gitops(&sync))
}

/// `GET /ui/fragment/apps` — apps table HTML fragment for HTMX swap.
async fn fragment_apps_handler(State(state): State<ApiState>) -> Response {
    match gather_dashboard_apps(&state).await {
        Ok(apps) => html_response(fragments::render_apps_table_fragment(&apps)),
        Err(error) => unavailable_response(error),
    }
}

/// `GET /ui/fragment/nodes` — nodes table HTML fragment for HTMX swap.
async fn fragment_nodes_handler(State(state): State<ApiState>) -> Response {
    // AUTH7: reflect the real gossip membership, not a hardcoded empty list.
    let nodes = gather_dashboard_nodes(&state).await;
    html_response(fragments::render_nodes_table_fragment(&nodes))
}

/// `GET /ui/fragment/alerts` — alerts table HTML fragment for HTMX swap.
async fn fragment_alerts_handler(State(state): State<ApiState>) -> Response {
    let alerts = if let Some(ref evaluator) = state.alerts {
        let eval = evaluator.read().await;
        eval.firing_alerts()
            .iter()
            .map(|a| crate::brioche::dashboard::DashboardAlert {
                labels: a.labels.clone(),
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
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    let statuses = match cluster_statuses(&state).await {
        Ok(rows) => rows.into_iter().map(|row| row.instance).collect::<Vec<_>>(),
        Err(error) => return unavailable_response(error),
    };
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
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::AppConfig {
        app_name: app,
        namespace,
        response,
    })
    .await
    {
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
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // C3: this endpoint exposes the whole `logs` table. `LogStore::query`
    // filters by tenant; arbitrary SQL cannot be made to, so a scoped token
    // is refused rather than served another tenant's logs.
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
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
    // OBS5: bounded access — read-only, `logs`-table only, row- and
    // memory-capped. A rejected query is a 400, not a 500.
    match store.query_sql_json_bounded(sql).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e @ crate::ketchup::types::KetchupError::QueryRejected { .. }) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `POST /v1/logs/export` request body.
#[derive(serde::Deserialize)]
struct LogsExportRequest {
    /// Where the Parquet files go — a path on the agent host, `file://`,
    /// `s3://` or `gs://`. Resolved agent-side, with the agent's credentials.
    destination: String,
}

/// `POST /v1/logs/export` — export this node's Parquet log store now.
///
/// Serialises with periodic, pressure and offline exporters through the same
/// checkpoint lock. Success includes durable acknowledgement persistence.
async fn logs_export_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(request): Json<LogsExportRequest>,
) -> Response {
    // Admin: this writes files wherever the destination points, using the
    // agent host's filesystem and object-store credentials.
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    let Some(log_store) = &state.log_store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "log store not enabled"})),
        )
            .into_response();
    };
    let data_dir = log_store.read().await.data_dir().to_path_buf();
    let mut checkpoint = crate::ketchup::export::ExportCheckpoint::default();
    let node_id = state
        .node_name
        .clone()
        .unwrap_or_else(|| "local".to_string());

    match crate::ketchup::export::export_logs(
        &data_dir,
        &request.destination,
        &node_id,
        &mut checkpoint,
    )
    .await
    {
        Ok(result) => Json(serde_json::json!({
            "files_exported": result.files_exported,
            "bytes_written": result.bytes_written,
            "node_id": node_id,
            "checkpoint_saved": true,
        }))
        .into_response(),
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
        return Json(crate::mayo::alert::AlertsResponse { alerts: Vec::new() });
    };
    let evaluator = alerts.read().await;
    Json(crate::mayo::alert::AlertsResponse {
        alerts: evaluator.all_statuses(),
    })
}

/// `GET /v1/metrics/keys` — list all distinct metric names.
async fn metrics_keys_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
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
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    let Some(rollup_store) = &state.rollup_store else {
        return Json(Vec::<MetricsQueryRow>::new()).into_response();
    };

    let store = rollup_store.read().await;
    let start = params.start.unwrap_or(0);
    // Clamped below u64::MAX: DataFusion 45's interval analysis
    // overflows (debug-build panic) computing the cardinality of a
    // full-domain unsigned range like `timestamp <= u64::MAX`.
    let end = params.end.unwrap_or(i64::MAX as u64).min(i64::MAX as u64);

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

/// Return worker identity with each contribution so overlapping aggregators cannot double-count it.
async fn metrics_owned_rollup_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
    }
    let Some(store) = &state.rollup_store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no rollup store configured",
        )
            .into_response();
    };
    match store
        .read()
        .await
        .query_owned_rows(
            params.name.as_deref(),
            params.start.unwrap_or(0),
            params.end.unwrap_or(i64::MAX as u64),
        )
        .await
    {
        Ok(rows) => Json(rows).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

/// Resolve the base URLs of every council aggregator (Raft voter) from the
/// live gossip membership.
///
/// Each aggregator holds only its assigned workers' rollups, so a cluster-wide
/// query must reach all of them and sum the partial aggregates. Returns `None`
/// when this node has no council or no membership table (the standalone case),
/// or when no voter can be mapped to a live member — the caller then reads its
/// own local rollup store instead. This node's own URL is included when it is a
/// voter, so a single-node council fans out to just itself.
async fn resolve_council_urls(state: &ApiState) -> Option<Vec<String>> {
    let council = state.council.as_ref()?;
    let membership = state.membership.as_ref()?;
    let metrics = council.metrics().borrow().clone();
    let voters: std::collections::BTreeSet<_> =
        metrics.membership_config.membership().voter_ids().collect();
    if voters.is_empty() {
        return None;
    }
    let members = membership.read().await;
    let urls: Vec<String> = members
        .iter()
        .filter(|member| {
            voters.contains(&crate::cluster::identity::raft_id_from_name(
                &member.node_id.0,
            ))
        })
        .map(|member| state.cluster_http.url(&member.address.to_string(), ""))
        .collect();
    if urls.is_empty() { None } else { Some(urls) }
}

/// `GET /v1/metrics/cluster?name=X&start=S&end=E` — cluster-wide query.
///
/// Fans out to all council aggregators' `/v1/metrics/rollup/owned` endpoints,
/// deduplicates worker contributions before summing, and returns the combined data
/// with any warnings about unresponsive aggregators. Falls back to reading the
/// local rollup store when there is no council to fan out to (single-node).
async fn metrics_cluster_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    let start = params.start.unwrap_or(0);
    // Clamped below u64::MAX: DataFusion 45's interval analysis
    // overflows (debug-build panic) computing the cardinality of a
    // full-domain unsigned range like `timestamp <= u64::MAX`.
    let end = params.end.unwrap_or(i64::MAX as u64).min(i64::MAX as u64);

    // Retain worker identity until after deduplication: reassignment leaves
    // overlapping history on old and new aggregators.
    if let Some(urls) = resolve_council_urls(&state).await {
        let query = MetricsQuery {
            metric_name: params.name.clone(),
            start,
            end,
            app: None,
            per_series: None,
        };
        let timeout = std::time::Duration::from_secs(10);
        let result = crate::mayo::query_fanout::fan_out_cluster_query(
            &query,
            &urls,
            state.cluster_http.client(),
            timeout,
            state.service_token.as_deref(),
        )
        .await;
        return Json(result).into_response();
    }

    // Single-node / no-council fallback: read the local rollup store directly,
    // which is equivalent to fanning out to just ourselves.
    let Some(rollup_store) = &state.rollup_store else {
        return Json(MetricsQueryResult {
            data: vec![],
            warnings: vec![QueryWarning::NodeUnresponsive {
                node_id: "no rollup store configured".to_string(),
            }],
        })
        .into_response();
    };

    let store = rollup_store.read().await;
    let result = store
        .query_owned_rows(params.name.as_deref(), start, end)
        .await;
    match result {
        Ok(rows) => {
            Json(crate::mayo::query_fanout::merge_owned_rollups(vec![rows])).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// One app's metric rows, wherever its instances run.
///
/// When the placement map is visible (council + membership), fans out to the
/// nodes running the app, hitting each one's app-filtered `/v1/metrics` leaf
/// and merge-sorting the per-instance rows. Falls back to the local metrics
/// store otherwise (single-node, or no placement info) — which is the same as
/// fanning out to just this node. `Err` carries a store failure message.
async fn app_metric_rows(
    state: &ApiState,
    app: &str,
    namespace: &str,
    name: Option<&str>,
    start: u64,
    end: u64,
    per_series: Option<u32>,
) -> Result<MetricsQueryResult, String> {
    // Cross-node fan-out: each node keeps only its own instances' samples, so
    // reading just this node's store misses instances scheduled elsewhere.
    if let (Some(council), Some(membership)) = (&state.council, &state.membership) {
        use crate::meat::types::AppId;
        let desired = council.desired_state().await;
        let app_id = AppId::new(app, namespace);
        let node_ids: Vec<crate::meat::NodeId> = desired
            .scheduling
            .get(&app_id)
            .map(|placements| placements.iter().map(|p| p.node_id.clone()).collect())
            .unwrap_or_default();

        if !node_ids.is_empty() {
            let members = membership.read().await;
            let urls: Vec<String> = node_ids
                .iter()
                .filter_map(|id| members.iter().find(|m| m.node_id == *id))
                .map(|m| state.cluster_http.url(&m.address.to_string(), ""))
                .collect();
            drop(members);

            if !urls.is_empty() {
                let query = MetricsQuery {
                    metric_name: name.map(str::to_string),
                    start,
                    end,
                    // The leaf filters on the `app` label, stored as `namespace/app`.
                    app: Some(format!("{namespace}/{app}")),
                    per_series,
                };
                let timeout = std::time::Duration::from_secs(10);
                return Ok(crate::mayo::query_fanout::fan_out_app_query(
                    &query,
                    &urls,
                    state.cluster_http.client(),
                    timeout,
                    state.service_token.as_deref(),
                )
                .await);
            }
        }
    }

    let Some(mayo) = &state.mayo else {
        return Ok(MetricsQueryResult {
            data: vec![],
            warnings: vec![],
        });
    };

    // Filter by app label in the local store. Both the app/namespace path
    // segments and the caller-supplied `name` reach the SQL literal, which
    // `query_app` escapes (OBS1): without that a crafted `?name=x' OR '1'='1`
    // or an app name carrying a quote would break out of the literal and drop
    // the tenant/time predicate, leaking other apps' metrics.
    let rows = mayo
        .read()
        .await
        .query_app(&format!("{namespace}/{app}"), name, start, end, per_series)
        .await
        .map_err(|error| error.to_string())?;
    Ok(MetricsQueryResult {
        data: rows
            .into_iter()
            .map(|(timestamp, metric_name, labels, value)| MetricsQueryRow {
                timestamp,
                metric_name,
                labels,
                value,
            })
            .collect(),
        warnings: vec![],
    })
}

/// The query window a per-app request names: `start` defaults to fifteen
/// minutes ago, `end` to now.
fn app_query_window(start: Option<u64>, end: Option<u64>) -> (u64, u64) {
    let start = start.unwrap_or_else(|| {
        crate::mayo::types::Sample::now(0.0)
            .timestamp
            .saturating_sub(APP_METRICS_DEFAULT_WINDOW_SECS)
    });
    // Clamped below u64::MAX: DataFusion 45's interval analysis
    // overflows (debug-build panic) computing the cardinality of a
    // full-domain unsigned range like `timestamp <= u64::MAX`.
    let end = end.unwrap_or(i64::MAX as u64).min(i64::MAX as u64);
    (start, end)
}

fn metrics_error_response(error: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": error })),
    )
        .into_response()
}

/// `GET /v1/metrics/app/{app}/{namespace}?name=X&start=S&end=E&per_series=N`
/// — one app's raw metric rows, across every node running it.
async fn metrics_app_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    let (start, end) = app_query_window(params.start, params.end);
    match app_metric_rows(
        &state,
        &app,
        &namespace,
        params.name.as_deref(),
        start,
        end,
        params.per_series,
    )
    .await
    {
        Ok(result) => Json(result).into_response(),
        Err(error) => metrics_error_response(error),
    }
}

#[derive(Deserialize)]
struct AppChartParams {
    /// Metric to draw; a histogram's base name for `kind=mean`.
    name: String,
    /// How rows become lines.
    kind: crate::mayo::series::ChartKind,
    start: Option<u64>,
    end: Option<u64>,
}

/// What the dashboard's chart script draws: series lined up on one time
/// axis, plus any fan-out warnings.
#[derive(Debug, Serialize, Deserialize)]
struct AppChartResponse {
    #[serde(flatten)]
    chart: crate::mayo::series::ChartData,
    warnings: Vec<crate::mayo::rollup::QueryWarning>,
}

/// `GET /v1/metrics/app/{app}/{namespace}/chart?name=X&kind=gauge|rate|mean`
/// — one metric as one line per instance, ready to draw.
///
/// `gauge` draws values, `rate` draws a counter's per-second rate, and
/// `mean` draws `rate(X_sum) / rate(X_count)`, a histogram's mean.
async fn metrics_app_chart_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(params): Query<AppChartParams>,
) -> Response {
    use crate::mayo::series::{self, ChartKind};

    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    let (start, end) = app_query_window(params.start, params.end);
    let fetch = |name: String| {
        let state = &state;
        let app = &app;
        let namespace = &namespace;
        async move { app_metric_rows(state, app, namespace, Some(&name), start, end, None).await }
    };
    let response = match params.kind {
        ChartKind::Gauge | ChartKind::Rate => {
            fetch(params.name.clone())
                .await
                .map(|result| AppChartResponse {
                    chart: series::instance_chart(params.kind, &result.data),
                    warnings: result.warnings,
                })
        }
        ChartKind::Mean => {
            match tokio::try_join!(
                fetch(format!("{}_sum", params.name)),
                fetch(format!("{}_count", params.name))
            ) {
                Ok((sum, count)) => {
                    let mut warnings = sum.warnings;
                    warnings.extend(count.warnings);
                    Ok(AppChartResponse {
                        chart: series::mean_chart(&sum.data, &count.data),
                        warnings,
                    })
                }
                Err(error) => Err(error),
            }
        }
    };
    match response {
        Ok(response) => Json(response).into_response(),
        Err(error) => metrics_error_response(error),
    }
}

// ---------------------------------------------------------------------------
// Deploy endpoints
// ---------------------------------------------------------------------------

/// Request node-local cooperative cancellation under the same authority as apply.
async fn deploy_cancel_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    let snapshot = match deploy_operation_snapshot(&state).await {
        Ok(snapshot) => snapshot,
        Err(response) => return response,
    };
    let Some(operation) = snapshot
        .active_deploys
        .iter()
        .chain(&snapshot.history)
        .find(|operation| operation.id.as_str() == id)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "deploy operation not found on this node"})),
        )
            .into_response();
    };
    let permissions = match &state.council {
        Some(council) => council.desired_state().await.permissions,
        None => std::collections::BTreeMap::new(),
    };
    for target in &operation.targets {
        if let Err(response) =
            crate::sesame::auth::authorize_scoped(auth.as_deref(), &target.name, &target.namespace)
        {
            return response;
        }
        if let Err(response) = crate::sesame::auth::authorize_permission(
            auth.as_deref(),
            crate::config::PermissionAction::Deploy,
            &target.name,
            &target.namespace,
            &permissions,
        ) {
            return response;
        }
    }
    let (response, result) = oneshot::channel();
    let request = async {
        state
            .cmd_tx
            .send(AgentCommand::CancelDeploy {
                operation_id: id.into(),
                response,
            })
            .await
            .ok()?;
        result.await.ok()
    };
    match tokio::time::timeout(std::time::Duration::from_secs(2), request).await {
        Ok(Some(Some(operation))) => {
            let status = if operation.outcome.is_some() {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            (status, Json(operation)).into_response()
        }
        Ok(Some(None)) => {
            (StatusCode::NOT_FOUND, "deploy operation no longer retained").into_response()
        }
        Ok(None) => (StatusCode::SERVICE_UNAVAILABLE, "agent unavailable").into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "cancellation receipt unknown; query or retry the same operation ID",
        )
            .into_response(),
    }
}

/// `GET /v1/deploys/active` — list active deploys.
async fn deploys_active_handler(State(state): State<ApiState>) -> Response {
    match deploy_operation_snapshot(&state).await {
        Ok(snapshot) => Json(crate::bun::deploy_operations::ActiveDeployOperations {
            active_deploys: snapshot.active_deploys,
        })
        .into_response(),
        Err(response) => response,
    }
}

/// `GET /v1/deploys/operations` — active operations and bounded recent
/// terminal history, using the same stable record shape for both.
async fn deploys_operations_handler(State(state): State<ApiState>) -> Response {
    match deploy_operation_snapshot(&state).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(response) => response,
    }
}

// `Response` is large but it IS the HTTP reply to send on failure —
// boxing it would tax every call site for a value that lives one frame.
#[allow(clippy::result_large_err)]
async fn deploy_operation_snapshot(
    state: &ApiState,
) -> Result<crate::bun::deploy_operations::DeployOperationSnapshot, Response> {
    let (response, result) = oneshot::channel();
    state
        .cmd_tx
        .send(AgentCommand::DeployOperations { response })
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "agent unavailable"})),
            )
                .into_response()
        })?;
    tokio::time::timeout(std::time::Duration::from_secs(2), result)
        .await
        .map_err(|_| {
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({"error": "agent deploy-state query timed out"})),
            )
                .into_response()
        })?
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "agent unavailable"})),
            )
                .into_response()
        })
}

#[derive(Deserialize)]
struct NamespaceQuery {
    namespace: Option<String>,
}

/// `GET /v1/deploys/history/{app}` — deploy history for an app.
///
/// The namespace rides in as a query parameter rather than a path segment
/// so the route (and every client bookmarking it) keeps its shape. It
/// matters for more than tidiness: since DEP1 two apps of the same name can
/// coexist in different namespaces, so filtering on the bare name returned
/// both tenants' history to whoever asked (C3).
async fn deploys_history_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(app): Path<String>,
    Query(query): Query<NamespaceQuery>,
) -> Response {
    let namespace = query.namespace.as_deref().unwrap_or("default");
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, namespace) {
        return resp;
    }
    let Some(history) = &state.deploy_history else {
        return Json(serde_json::json!({"app": app, "namespace": namespace, "history": []}))
            .into_response();
    };
    let all = history.read().await;
    let filtered: Vec<&DeployHistoryEntry> = all
        .iter()
        .filter(|e| e.app_id.name == app && e.app_id.namespace == namespace)
        .collect();
    Json(serde_json::json!({"app": app, "namespace": namespace, "history": filtered}))
        .into_response()
}

/// `POST /v1/rollback/{app}/{namespace}` — redeploy the app's previous
/// successful spec (X3).
///
/// "Previous" means the last-but-one distinct successful deploy: the
/// most recent completed entry is the *current* version, so rollback
/// targets the one before it. Re-applies through the same path as
/// `apply` (Raft in cluster mode, local deploy otherwise).
async fn rollback_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    let Some(history) = &state.deploy_history else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "deploy history unavailable"})),
        )
            .into_response();
    };

    // Successful deploys for this app, newest first, that carry a spec.
    let target_spec = {
        let all = history.read().await;
        let mut successful: Vec<&DeployHistoryEntry> = all
            .iter()
            .filter(|e| {
                e.app_id.name == app
                    && e.app_id.namespace == namespace
                    && e.result == crate::meat::deploy_types::DeployResult::Completed
                    && e.spec.is_some()
            })
            .collect();
        successful.reverse(); // newest first
        // [0] is the current version; [1] is the rollback target.
        successful.get(1).and_then(|e| e.spec.clone()).map(|s| *s)
    };

    let Some(spec) = target_spec else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("no previous successful deploy to roll {app} back to")
            })),
        )
            .into_response();
    };

    // Re-apply the previous spec through the standard deploy path.
    let mut config = Config::default();
    config.app.insert(app.clone(), spec);
    let raw = toml::to_string(&config).unwrap_or_default();

    if let Some(council) = &state.council {
        return cluster_apply(
            state.clone(),
            Arc::clone(council),
            config,
            raw,
            None,
            HeaderMap::new(),
            None,
        )
        .await;
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
            Json(serde_json::json!({"error": "agent unavailable"})),
        )
            .into_response();
    }
    let stream = ReceiverStream::new(event_rx).map(|e| {
        Ok::<_, std::convert::Infallible>(
            Event::default().data(serde_json::to_string(&e).unwrap_or_default()),
        )
    });
    Sse::new(stream).into_response()
}

/// `GET /v1/images` — list committed images using current cluster authority,
/// trimmed to the repositories the caller's token scope may pull.
async fn images_handler(
    State(state): State<ApiState>,
    authority: Option<axum::Extension<crate::pickle::authority::RegistryReadAuthority>>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    use crate::pickle::authority::{RegistryQuery, RegistryQueryResponse};
    let mut images = if let Some(authority) = authority {
        match authority
            .forwarder
            .query(
                state.council.as_ref(),
                authority.node_id,
                RegistryQuery::Images,
            )
            .await
        {
            Ok(RegistryQueryResponse::Images(images)) => images,
            Ok(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "invalid registry image-list response",
                )
                    .into_response();
            }
            Err(error) => {
                return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
            }
        }
    } else if let Some(council) = &state.council {
        if let Err(error) = council.security_state_linearizable().await {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
        council.manifest_catalog().await.images()
    } else if state.static_capabilities.cluster_mode {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "registry authority is unavailable",
        )
            .into_response();
    } else if let Some(catalog) = &state.pickle_catalog {
        catalog.read().await.images()
    } else {
        Vec::new()
    };
    // The registry refuses a scoped token another namespace's repositories;
    // listing them here would hand over their names, tags and digests anyway.
    images.retain(|image| {
        crate::pickle::registry_auth::check_repository_scope(
            auth.as_deref(),
            &image.repository,
            crate::pickle::registry_auth::RepositoryAccess::Read,
        )
        .is_ok()
    });
    Json(serde_json::json!({ "images": images })).into_response()
}

/// GitOps webhook handler (public, HMAC-authenticated).
///
/// Accepts POST from git hosting providers (GitHub, GitLab, Gitea). The
/// route is public because providers can't present a Reliaburger bearer
/// token, so the request is authenticated here instead: the HMAC-SHA256
/// signature over the raw body must match the `[gitops] webhook_secret`,
/// the delivery id must not be a replay, and the rate limit must not be
/// exceeded (GIT3). Only then is the sync loop nudged.
///
/// Returns 202 on success, 401 on a bad/missing signature or replay, 429
/// when rate-limited, and 503 when GitOps or the webhook secret isn't
/// configured.
async fn gitops_webhook_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(tx) = &state.gitops_webhook_tx else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "gitops not configured" })),
        )
            .into_response();
    };

    // Fail closed: without a configured secret we can't authenticate the
    // caller, and a public unauthenticated trigger is a DoS lever.
    let Some(validator) = &state.gitops_webhook_validator else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "webhook secret not configured" })),
        )
            .into_response();
    };

    // Reserve before recording the delivery ID: a full queue must remain
    // retryable, and a closed receiver must never produce a success response.
    let permit = match tx.try_reserve() {
        Ok(permit) => permit,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": format!("gitops sync queue unavailable: {error}")
                })),
            )
                .into_response();
        }
    };

    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok());
    let gitlab_token = headers.get("x-gitlab-token").and_then(|v| v.to_str().ok());
    let delivery_id = header_delivery_id(&headers);
    let branch = header_branch(&headers).unwrap_or_else(|| "main".to_string());

    let mut guard = validator.lock().await;
    let result = if let Some(token) = gitlab_token {
        // GitLab sends the shared secret verbatim in `X-Gitlab-Token`.
        guard.validate_gitlab(&body, token, delivery_id.as_deref(), &branch)
    } else {
        // GitHub/Gitea sign the body: `X-Hub-Signature-256: sha256=<hex>`.
        guard.validate(&body, signature, delivery_id.as_deref(), &branch)
    };
    drop(guard);

    match result {
        Ok(_) => {
            permit.send(());
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "message": "sync queued" })),
            )
                .into_response()
        }
        Err(e) => {
            let message = e.to_string();
            // A rate-limit rejection is a 429; everything else (bad
            // signature, replay, missing header) is a 401.
            let code = if message.contains("rate limit") {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::UNAUTHORIZED
            };
            (code, Json(serde_json::json!({ "error": message }))).into_response()
        }
    }
}

/// The provider's delivery id header, if any (GitHub / Gitea).
fn header_delivery_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-github-delivery")
        .or_else(|| headers.get("x-gitlab-event-uuid"))
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

/// The branch a push targeted, parsed from `X-Reliaburger-Branch` if the
/// caller set it. Providers don't send the branch in a header, so this is
/// advisory; the sync loop tracks the configured branch regardless.
fn header_branch(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-reliaburger-branch")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
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

/// Attach an operator's detached image signature (from `relish sign`) to a
/// manifest via Raft. The body is a [`crate::pickle::signing::SignatureSubmission`];
/// the private key never reaches the cluster.
async fn identity_sign_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    // AUTH4: a user-management route. The service principal must not sign
    // images, even though it can present the cluster token.
    if let Err(resp) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
    }
    let submission: crate::pickle::signing::SignatureSubmission = match serde_json::from_str(&body)
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid JSON: {e}") })),
            )
                .into_response();
        }
    };

    match ask_agent(&state.cmd_tx, |response| AgentCommand::SignImage {
        submission,
        response,
    })
    .await
    {
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
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
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
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
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
        Ok(crate::council::CouncilResponse::Refused { reason }) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response(),
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
    headers: HeaderMap,
    body: String,
) -> Response {
    // AUTH4: the highest-value lateral-movement target. A stolen service
    // token must not be able to mint fresh user tokens.
    if let Err(resp) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
    }
    #[derive(serde::Deserialize, serde::Serialize)]
    struct CreateRequest {
        name: String,
        role: String,
        #[serde(default)]
        apps: Option<Vec<String>>,
        #[serde(default)]
        namespaces: Option<Vec<String>>,
        #[serde(default)]
        ttl_days: Option<u64>,
        #[serde(default)]
        lease_id: Option<String>,
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

    // The internal service principal name is reserved: a user token minted with
    // it would match `SYSTEM_PRINCIPAL` and bypass scope confinement (AUTH4).
    if req.name == crate::sesame::auth::SYSTEM_PRINCIPAL {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("token name {:?} is reserved", req.name)
            })),
        )
            .into_response();
    }

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

    let lease_owner = if req.lease_id.is_some() {
        let user =
            match authenticated_test_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin) {
                Ok(user) => user,
                Err(response) => return response,
            };
        if let Err(response) = crate::sesame::auth::require_unscoped(Some(user)) {
            return response;
        }
        if let Err(response) = test_operation_authorisation(&state, user) {
            return response;
        }
        if !council.is_leader().await {
            return forward_test_lease_request(
                &state,
                council,
                reqwest::Method::POST,
                "/v1/token/create",
                &headers,
                Some(&req),
            )
            .await;
        }
        Some(user.principal_id.clone())
    } else {
        None
    };
    let lease = if let Some(lease_id) = &req.lease_id {
        let Some(lease) = find_test_lease(&state, lease_id).await else {
            return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
        };
        if let Err(error) = lease.authorise_owner(
            lease_owner.as_deref().unwrap_or_default(),
            crate::testkit::lease::now_unix_millis(),
        ) {
            return lease_error_response(error);
        }
        Some(lease)
    } else {
        None
    };

    let scope = crate::sesame::types::TokenScope {
        apps: req.apps.clone(),
        namespaces: req.namespaces.clone(),
    };
    let mut expires_at = match req.ttl_days {
        None => None,
        Some(days) => {
            let expiry = days
                .checked_mul(86_400)
                .filter(|seconds| *seconds > 0)
                .and_then(|seconds| {
                    std::time::SystemTime::now()
                        .checked_add(std::time::Duration::from_secs(seconds))
                });
            let Some(expiry) = expiry else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "ttl_days must be positive and produce a representable expiry"
                    })),
                )
                    .into_response();
            };
            Some(expiry)
        }
    };

    if let Some(lease) = &lease {
        let Some(bound) = std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::from_millis(lease.expires_at_unix_ms))
        else {
            return lease_error_response(crate::testkit::lease::LeaseError::InvalidExpiry);
        };
        expires_at = Some(expires_at.map_or(bound, |expiry| expiry.min(bound)));
    }

    // Argon2id hashing is deliberately slow + memory-hungry (M7): run it on the
    // blocking pool so it doesn't stall the async runtime worker.
    let name = req.name.clone();
    let created = match tokio::task::spawn_blocking(move || {
        crate::sesame::token::create_token(&name, role, scope, expires_at)
    })
    .await
    {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "token hashing task failed" })),
            )
                .into_response();
        }
    };

    let request = match (lease, lease_owner) {
        (Some(lease), Some(owner_id)) => crate::council::RaftRequest::TestLeaseApiToken {
            lease_id: lease.lease_id,
            owner_id,
            observed_at_unix_ms: crate::testkit::lease::now_unix_millis(),
            token: Box::new(created.token),
        },
        _ => crate::council::RaftRequest::CreateApiToken(created.token),
    };
    if let Err(response) = write_lease_request(council, request).await {
        return response;
    }
    Json(serde_json::json!({
        "name": req.name,
        "role": req.role,
        "token": created.plaintext,
    }))
    .into_response()
}

/// Create a short-lived, single-use node join token and persist its hash.
///
/// This is deliberately separate from API bearer-token management. The
/// plaintext exists only in this request and response; Raft receives the
/// SHA-256 hash, expiry and attestation policy.
async fn join_token_create_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
    }

    fn default_ttl_seconds() -> u64 {
        crate::sesame::join::DEFAULT_JOIN_TOKEN_TTL.as_secs()
    }

    #[derive(serde::Deserialize)]
    struct CreateRequest {
        #[serde(default = "default_ttl_seconds")]
        ttl_seconds: u64,
        /// The node id this token may enrol (M4). Required: a token is bound to
        /// exactly one node id so it cannot be replayed to impersonate another.
        #[serde(default)]
        node_id: String,
    }

    let req: CreateRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
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

    let ttl = std::time::Duration::from_secs(req.ttl_seconds);
    let (plaintext, join_token) = match crate::sesame::join::create_join_token(ttl, &req.node_id) {
        Ok(created) => created,
        Err(crate::sesame::join::JoinError::EmptyNodeId) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "node_id is required" })),
            )
                .into_response();
        }
        Err(crate::sesame::join::JoinError::InvalidTtl { .. }) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!(
                        "ttl_seconds must be between {} and {}",
                        crate::sesame::join::MIN_JOIN_TOKEN_TTL.as_secs(),
                        crate::sesame::join::MAX_JOIN_TOKEN_TTL.as_secs(),
                    )
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let expires_at = join_token
        .expires_at
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    match council
        .write(crate::council::RaftRequest::CreateJoinToken(join_token))
        .await
    {
        Ok(_) => Json(serde_json::json!({
            "token": plaintext,
            "ttl_seconds": req.ttl_seconds,
            "expires_at": expires_at,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": format!("failed to commit join token (try the cluster leader): {e}")
            })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Secret rotation endpoint
// ---------------------------------------------------------------------------

/// Read the active public encryption recipient from locally applied state.
///
/// Publishing a recipient grants no decryption authority. Scoped read-only
/// users may encrypt new values without gaining access to any private key.
async fn secret_public_key_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(response) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return response;
    }
    let Some(council) = &state.council else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no council available").into_response();
    };
    let security = council.security_state().await;
    let Some(keypair) = security
        .age_keypairs
        .iter()
        .filter(|key| key.scope == crate::sesame::types::AgeKeyScope::ClusterWide && !key.read_only)
        .max_by_key(|key| key.generation)
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no active cluster encryption key available",
        )
            .into_response();
    };
    Json(crate::sesame::types::SecretPublicKey {
        public_key: keypair.public_key.clone(),
        generation: keypair.generation,
    })
    .into_response()
}

/// Rotate or finalise secret encryption key via Raft.
async fn secret_rotate_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    // AUTH4: rotating the cluster's secret-encryption keys is user-admin
    // work, not something the service principal should ever do.
    if let Err(resp) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
    }
    #[derive(serde::Deserialize)]
    struct RotateRequest {
        #[serde(default)]
        finalize: bool,
    }

    // A malformed body must not silently become a (non-finalise) rotation —
    // that mutates cluster key state on a typo (PKI8). An empty body keeps the
    // convenient default; anything present must parse.
    let req: RotateRequest = if body.trim().is_empty() {
        RotateRequest { finalize: false }
    } else {
        match serde_json::from_str(&body) {
            Ok(req) => req,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("invalid rotate request: {e}") })),
                )
                    .into_response();
            }
        }
    };

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
            // Verify-before-retire (PKI8): the state machine refuses to
            // drop the old key while any stored secret is still sealed
            // under it, and names the offenders.
            Ok(crate::council::types::CouncilResponse::Refused { reason }) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": reason })),
            )
                .into_response(),
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
            // One rotation at a time (PKI8): an un-finalised rotation
            // must be finalised (or re-encrypted then finalised) first.
            Ok(crate::council::types::CouncilResponse::Refused { reason }) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": reason })),
            )
                .into_response(),
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
    #[tokio::test]
    async fn registry_query_response_refuses_an_oversized_catalogue() {
        use crate::pickle::authority::{MAX_REGISTRY_PROPOSAL_BYTES, RegistryQueryResponse};
        let mut catalog = crate::pickle::types::ManifestCatalog::default();
        catalog
            .repository_owners
            .insert("repository".into(), "x".repeat(MAX_REGISTRY_PROPOSAL_BYTES));
        assert_eq!(
            super::bounded_registry_query_response(RegistryQueryResponse::Repository(Box::new(
                catalog
            )))
            .await
            .status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            super::bounded_registry_query_response(RegistryQueryResponse::Repository(
                Default::default()
            ))
            .await
            .status(),
            axum::http::StatusCode::OK
        );
    }

    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::bun::agent::BunAgent;
    use crate::grill::mock::MockGrill;
    use crate::grill::port::PortAllocator;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn desired_app_diagnostics_filter_to_the_token_scope() {
        let auth = crate::sesame::auth::AuthContext {
            token_name: "tenant-a".to_string(),
            principal_id: "token:test".to_string(),
            role: crate::sesame::types::ApiRole::ReadOnly,
            scoped_apps: Some(vec!["api".to_string()]),
            scoped_namespaces: Some(vec!["tenant-a".to_string()]),
        };
        let evidence = |app: &str, namespace: &str| crate::bun::diagnostics::DesiredAppEvidence {
            app: app.to_string(),
            namespace: namespace.to_string(),
            desired_replicas: 1,
            scheduled_replicas: 1,
            placements: Default::default(),
            service_port: Some(8080),
        };

        let visible = filter_desired_apps_for_scope(
            vec![
                evidence("api", "tenant-a"),
                evidence("worker", "tenant-a"),
                evidence("api", "tenant-b"),
            ],
            Some(&auth),
        );

        assert_eq!(visible, vec![evidence("api", "tenant-a")]);
    }

    #[test]
    fn internal_path_names_are_single_dns_labels() {
        for valid in ["api", "api-v2", "a1"] {
            assert!(valid_path_label(valid), "rejected {valid:?}");
        }
        for invalid in ["", "API", "-api", "api-", "api.default", "api;id"] {
            assert!(!valid_path_label(invalid), "accepted {invalid:?}");
        }
    }

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

        let app = router(
            cmd_tx, None, None, None, None, None, None, None, None, None, None, None, 9117, None,
        );
        (app, shutdown)
    }

    /// `GET /v1/apps` → `BunClient::current_resources` → `generate_plan`:
    /// the full dry-run diff chain against a live standalone agent. Before
    /// the endpoint existed, every dry-run caller passed `current = None`,
    /// so the tested Update/Unchanged diff in plan.rs was dead in production.
    #[tokio::test]
    async fn current_apps_feeds_the_dry_run_diff() {
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
            None,
            None,
            None,
            9117,
            None,
        );

        // Deploy one app through the agent, as apply would.
        let config = crate::config::Config::parse("[app.web]\nimage = \"myapp:v1\"\n").unwrap();
        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        cmd_tx
            .send(AgentCommand::Deploy {
                config,
                events: ev_tx,
            })
            .await
            .unwrap();
        while ev_rx.recv().await.is_some() {}

        // Serve the router for a real client round-trip.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { serving.cancelled().await })
                .await
                .unwrap();
        });

        let client = crate::relish::client::BunClient::new(&format!("http://{address}"));
        let current = client.current_resources().await.unwrap();
        assert!(
            current
                .iter()
                .any(|r| r.resource == "app.web" && r.image.as_deref() == Some("myapp:v1")),
            "deployed app missing from /v1/apps: {current:?}"
        );

        // Same image diffs Unchanged; a bumped image diffs Update; the
        // running app absent from a config is reported, never "destroyed".
        let same = crate::config::Config::parse("[app.web]\nimage = \"myapp:v1\"\n").unwrap();
        let plan = crate::relish::plan::generate_plan(&same, Some(&current));
        assert_eq!((plan.to_update, plan.unchanged), (0, 1), "{plan:?}");

        let bumped = crate::config::Config::parse("[app.web]\nimage = \"myapp:v2\"\n").unwrap();
        let plan = crate::relish::plan::generate_plan(&bumped, Some(&current));
        assert_eq!((plan.to_create, plan.to_update), (0, 1), "{plan:?}");

        let unrelated = crate::config::Config::parse("[app.other]\nimage = \"o:v1\"\n").unwrap();
        let plan = crate::relish::plan::generate_plan(&unrelated, Some(&current));
        assert_eq!(plan.not_in_config, 1, "{plan:?}");

        shutdown.cancel();
        let _ = server.await;
    }

    /// Build a router whose local `MayoStore` already holds `samples`
    /// (`(metric_name, app_filter, value)` where `app_filter` is the
    /// `namespace/app` label written under the `app` key). Used to drive the
    /// per-app metrics endpoint through the real HTTP route.
    async fn test_setup_with_metrics(
        samples: &[(&str, &str, f64)],
    ) -> (Router, CancellationToken, tempfile::TempDir) {
        let now = crate::mayo::types::Sample::now(0.0).timestamp;
        let timed: Vec<(&str, &str, &str, u64, f64)> = samples
            .iter()
            .map(|(name, app, value)| (*name, *app, "instance-0", now, *value))
            .collect();
        test_setup_with_timed_metrics(&timed).await
    }

    /// Like [`test_setup_with_metrics`], with an explicit instance label
    /// and timestamp per sample: `(name, app label, instance, time, value)`.
    async fn test_setup_with_timed_metrics(
        samples: &[(&str, &str, &str, u64, f64)],
    ) -> (Router, CancellationToken, tempfile::TempDir) {
        use crate::mayo::types::{MetricKey, Sample};

        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());
        tokio::spawn(async move {
            agent.run().await;
        });

        let dir = tempfile::tempdir().unwrap();
        let mut store = MayoStore::new(dir.path().to_path_buf());
        for (name, app_filter, instance, timestamp, value) in samples {
            let mut labels = std::collections::BTreeMap::new();
            labels.insert("app".to_string(), app_filter.to_string());
            labels.insert("instance".to_string(), instance.to_string());
            let key = MetricKey::with_labels(*name, labels);
            store.insert(&key, Sample::at(*timestamp, *value));
        }
        store.flush().await.unwrap();
        let mayo = Some(Arc::new(RwLock::new(store)));

        let app = router(
            cmd_tx, mayo, None, None, None, None, None, None, None, None, None, None, 9117, None,
        );
        (app, shutdown, dir)
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
        setup_with_auth_and_readiness(
            tokens,
            service_token,
            crate::bun::readiness::ReadinessTracker::new(),
        )
        .await
    }

    async fn setup_with_auth_and_readiness(
        tokens: Vec<crate::sesame::types::ApiToken>,
        service_token: Option<String>,
        readiness: crate::bun::readiness::ReadinessTracker,
    ) -> (Router, CancellationToken) {
        setup_with_auth_readiness_and_leases(
            tokens,
            service_token,
            readiness,
            crate::bun::capabilities::StaticCapabilities::default(),
            None,
        )
        .await
    }

    async fn setup_with_auth_readiness_and_leases(
        tokens: Vec<crate::sesame::types::ApiToken>,
        service_token: Option<String>,
        readiness: crate::bun::readiness::ReadinessTracker,
        static_capabilities: crate::bun::capabilities::StaticCapabilities,
        local_test_leases: Option<crate::testkit::lease::LocalLeaseStore>,
    ) -> (Router, CancellationToken) {
        setup_with_auth_readiness_leases_and_events(
            tokens,
            service_token,
            readiness,
            static_capabilities,
            local_test_leases,
            None,
        )
        .await
    }

    async fn setup_with_auth_readiness_leases_and_events(
        tokens: Vec<crate::sesame::types::ApiToken>,
        service_token: Option<String>,
        readiness: crate::bun::readiness::ReadinessTracker,
        static_capabilities: crate::bun::capabilities::StaticCapabilities,
        local_test_leases: Option<crate::testkit::lease::LocalLeaseStore>,
        events: Option<Arc<RwLock<crate::bun::events::EventStore>>>,
    ) -> (Router, CancellationToken) {
        setup_with_auth_leases_events_and_council(
            tokens,
            service_token,
            readiness,
            static_capabilities,
            local_test_leases,
            events,
            None,
        )
        .await
    }

    async fn setup_with_auth_leases_events_and_council(
        tokens: Vec<crate::sesame::types::ApiToken>,
        service_token: Option<String>,
        readiness: crate::bun::readiness::ReadinessTracker,
        static_capabilities: crate::bun::capabilities::StaticCapabilities,
        local_test_leases: Option<crate::testkit::lease::LocalLeaseStore>,
        events: Option<Arc<RwLock<crate::bun::events::EventStore>>>,
        council: Option<Arc<crate::council::CouncilNode>>,
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
        let app = router_with_upgrade(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            council,
            Some(store),
            service_token,
            None,
            None,
            None,
            None,
            9117,
            events,
            None,
            None,
            "default".to_string(),
            None,
            900,
            crate::cluster::ClusterHttp::plaintext(),
            5050,
            "http",
            256 * 1024 * 1024,
            false,
            static_capabilities,
            readiness,
            local_test_leases,
            None,
        );
        (app, shutdown)
    }

    fn a_user_token(
        role: crate::sesame::types::ApiRole,
    ) -> (crate::sesame::types::ApiToken, String) {
        named_user_token("u", role)
    }

    fn named_user_token(
        name: &str,
        role: crate::sesame::types::ApiRole,
    ) -> (crate::sesame::types::ApiToken, String) {
        let created = crate::sesame::token::create_token(
            name,
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
    async fn external_path_refuses_the_open_bootstrap_window() {
        let (app, shutdown) = test_setup();
        let body = serde_json::json!({
            "source": "api",
            "source_namespace": "default",
            "destination": "example.com",
            "destination_namespace": "default",
            "port": 443
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/path")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn external_path_needs_admin_policy_and_exact_destination() {
        use crate::testkit::safety::{ClusterSafetyClass, OperationPermission};

        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let mut capabilities = crate::bun::capabilities::StaticCapabilities::default();
        capabilities.test_policy.safety_class = ClusterSafetyClass::Staging;
        capabilities
            .test_policy
            .allowed_operations
            .insert(OperationPermission::ProbeExternalDestination);
        capabilities.test_policy.external_probe_allowlist = vec!["example.com:443".to_string()];
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            capabilities,
            None,
        )
        .await;

        let body = |port| {
            serde_json::json!({
                "source": "api",
                "source_namespace": "default",
                "destination": "example.com",
                "destination_namespace": "default",
                "port": port
            })
            .to_string()
        };
        assert_eq!(
            post_status(app.clone(), "/v1/path", &plaintext, &body(80)).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            post_status(app.clone(), "/v1/path", &plaintext, &body(0)).await,
            StatusCode::BAD_REQUEST
        );
        // The exact allowlisted destination passes the policy boundary and
        // reaches the local-source check. No workload was seeded, hence 404.
        assert_eq!(
            post_status(app, "/v1/path", &plaintext, &body(443)).await,
            StatusCode::NOT_FOUND
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn websocket_upgrade_requires_a_token() {
        let (token, _plaintext) = a_user_token(crate::sesame::types::ApiRole::ReadOnly);
        let (app, shutdown) = setup_with_auth(vec![token], None).await;
        assert_eq!(
            get_status(app, "/v1/ws/events", None).await,
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

    fn lease_static_capabilities() -> crate::bun::capabilities::StaticCapabilities {
        crate::bun::capabilities::StaticCapabilities {
            test_policy: crate::testkit::safety::ClusterTestPolicy {
                safety_class: crate::testkit::safety::ClusterSafetyClass::Development,
                allowed_operations: std::collections::BTreeSet::from([
                    crate::testkit::safety::OperationPermission::ProvisionIsolatedWorkloads,
                ]),
                max_lease_seconds: 60,
                ..crate::testkit::safety::ClusterTestPolicy::default()
            },
            ..crate::bun::capabilities::StaticCapabilities::default()
        }
    }

    fn capacity_static_capabilities() -> crate::bun::capabilities::StaticCapabilities {
        crate::bun::capabilities::StaticCapabilities {
            test_policy: crate::testkit::safety::ClusterTestPolicy {
                safety_class: crate::testkit::safety::ClusterSafetyClass::Development,
                allowed_operations: std::collections::BTreeSet::from([
                    crate::testkit::safety::OperationPermission::ProvisionIsolatedWorkloads,
                    crate::testkit::safety::OperationPermission::SaturateCapacity,
                ]),
                max_lease_seconds: 60,
                ..crate::testkit::safety::ClusterTestPolicy::default()
            },
            ..crate::bun::capabilities::StaticCapabilities::default()
        }
    }

    fn node_fault_static_capabilities() -> crate::bun::capabilities::StaticCapabilities {
        crate::bun::capabilities::StaticCapabilities {
            test_policy: crate::testkit::safety::ClusterTestPolicy {
                safety_class: crate::testkit::safety::ClusterSafetyClass::Development,
                allowed_operations: std::collections::BTreeSet::from([
                    crate::testkit::safety::OperationPermission::AlterNodeState,
                ]),
                ..crate::testkit::safety::ClusterTestPolicy::default()
            },
            ..crate::bun::capabilities::StaticCapabilities::default()
        }
    }

    fn node_pressure_static_capabilities() -> crate::bun::capabilities::StaticCapabilities {
        crate::bun::capabilities::StaticCapabilities {
            test_policy: crate::testkit::safety::ClusterTestPolicy {
                safety_class: crate::testkit::safety::ClusterSafetyClass::Development,
                allowed_operations: std::collections::BTreeSet::from([
                    crate::testkit::safety::OperationPermission::SaturateCapacity,
                ]),
                max_node_pressure_cpu_percent: 80,
                max_node_pressure_memory_percent: 90,
                ..crate::testkit::safety::ClusterTestPolicy::default()
            },
            ..crate::bun::capabilities::StaticCapabilities::default()
        }
    }

    fn workload_fault_static_capabilities() -> crate::bun::capabilities::StaticCapabilities {
        crate::bun::capabilities::StaticCapabilities {
            test_policy: crate::testkit::safety::ClusterTestPolicy {
                safety_class: crate::testkit::safety::ClusterSafetyClass::Development,
                allowed_operations: std::collections::BTreeSet::from([
                    crate::testkit::safety::OperationPermission::InjectWorkloadFaults,
                ]),
                ..crate::testkit::safety::ClusterTestPolicy::default()
            },
            ..crate::bun::capabilities::StaticCapabilities::default()
        }
    }

    fn workload_fault_body(acknowledged: bool, injected_by: &str) -> String {
        serde_json::to_string(&crate::smoker::types::FaultRequest {
            fault_type: crate::smoker::types::FaultType::DnsNxdomain,
            target_service: "web".to_string(),
            namespace: None,
            target_instance: None,
            target_node: None,
            duration: std::time::Duration::from_secs(30),
            injected_by: injected_by.to_string(),
            reason: Some("fault authorisation test".to_string()),
            include_leader: false,
            override_safety: false,
            acknowledged,
        })
        .unwrap()
    }

    fn council_partition_body(acknowledged: bool) -> String {
        serde_json::to_string(&crate::smoker::types::FaultRequest {
            fault_type: crate::smoker::types::FaultType::CouncilPartition {
                peers: vec!["node-b".to_string()],
            },
            target_service: String::new(),
            namespace: None,
            target_instance: None,
            target_node: Some("node-a".to_string()),
            duration: std::time::Duration::from_secs(30),
            injected_by: "untrusted-client-value".to_string(),
            reason: Some("api policy test".to_string()),
            include_leader: true,
            override_safety: false,
            acknowledged,
        })
        .unwrap()
    }

    fn node_kill_body(acknowledged: bool) -> String {
        serde_json::to_string(&crate::smoker::types::FaultRequest {
            fault_type: crate::smoker::types::FaultType::NodeKill {
                kill_containers: false,
            },
            target_service: String::new(),
            namespace: None,
            target_instance: None,
            target_node: Some("node-a".to_string()),
            duration: std::time::Duration::from_secs(30),
            injected_by: "untrusted-client-value".to_string(),
            reason: Some("api policy test".to_string()),
            include_leader: false,
            override_safety: false,
            acknowledged,
        })
        .unwrap()
    }

    fn node_pressure_body(acknowledged: bool) -> String {
        serde_json::to_string(&crate::smoker::types::FaultRequest {
            fault_type: crate::smoker::types::FaultType::NodePressure {
                cpu_percentage: 80,
                memory_percentage: 90,
            },
            target_service: String::new(),
            namespace: None,
            target_instance: None,
            target_node: Some("node-a".to_string()),
            duration: std::time::Duration::from_secs(30),
            injected_by: "untrusted-client-value".to_string(),
            reason: Some("api pressure policy test".to_string()),
            include_leader: false,
            override_safety: false,
            acknowledged,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn deployer_cannot_alter_node_state_even_with_grant_and_acknowledgement() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            node_fault_static_capabilities(),
            None,
        )
        .await;

        let status = post_status(app, "/v1/fault", &plaintext, &node_kill_body(true)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn deployer_cannot_partition_a_council_member() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            node_fault_static_capabilities(),
            None,
        )
        .await;

        let status = post_status(app, "/v1/fault", &plaintext, &council_partition_body(true)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn council_partition_requires_explicit_acknowledgement() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            node_fault_static_capabilities(),
            None,
        )
        .await;

        let (status, body) = post_authenticated(
            app,
            "/v1/fault",
            &plaintext,
            &council_partition_body(false),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(String::from_utf8_lossy(&body).contains("acknowledgement"));
        shutdown.cancel();
    }

    #[tokio::test]
    async fn admin_cannot_alter_node_state_without_the_server_grant() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let (app, shutdown) = setup_with_auth(vec![token], None).await;

        let status = post_status(app, "/v1/fault", &plaintext, &node_kill_body(true)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn admin_node_fault_requires_explicit_acknowledgement() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            node_fault_static_capabilities(),
            None,
        )
        .await;

        let (status, body) =
            post_authenticated(app, "/v1/fault", &plaintext, &node_kill_body(false), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(String::from_utf8_lossy(&body).contains("acknowledgement"));
        shutdown.cancel();
    }

    #[tokio::test]
    async fn admin_with_node_grant_and_acknowledgement_needs_cluster_evidence() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            node_fault_static_capabilities(),
            None,
        )
        .await;

        let (status, body) =
            post_authenticated(app, "/v1/fault", &plaintext, &node_kill_body(true), None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(String::from_utf8_lossy(&body).contains("council evidence"));
        shutdown.cancel();
    }

    #[tokio::test]
    async fn node_pressure_uses_capacity_permission_and_explicit_acknowledgement() {
        let (deployer, deployer_plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (admin, admin_plaintext) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![deployer, admin],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            node_pressure_static_capabilities(),
            None,
        )
        .await;

        assert_eq!(
            post_status(
                app.clone(),
                "/v1/fault",
                &deployer_plaintext,
                &node_pressure_body(true)
            )
            .await,
            StatusCode::FORBIDDEN
        );
        let (status, body) = post_authenticated(
            app.clone(),
            "/v1/fault",
            &admin_plaintext,
            &node_pressure_body(false),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(String::from_utf8_lossy(&body).contains("acknowledgement"));

        // Authorisation now succeeds; this unit router then fails closed
        // because it has no live council evidence for the target node.
        assert_eq!(
            post_status(
                app,
                "/v1/fault",
                &admin_plaintext,
                &node_pressure_body(true)
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn deployer_cannot_route_a_clear_without_any_reversal_grant() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            node_fault_static_capabilities(),
            None,
        )
        .await;

        assert_eq!(
            delete_authenticated(app, "/v1/fault/1?node=node-a&acknowledged=true", &plaintext,)
                .await,
            StatusCode::FORBIDDEN
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn node_routing_does_not_require_destructive_acknowledgement() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            node_fault_static_capabilities(),
            None,
        )
        .await;

        assert_eq!(
            delete_authenticated(app, "/v1/fault/1?node=node-a", &plaintext).await,
            StatusCode::OK
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn authorised_node_fault_reversal_reaches_the_owning_agent() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            node_fault_static_capabilities(),
            None,
        )
        .await;

        assert_eq!(
            delete_authenticated(app, "/v1/fault/1?node=node-a&acknowledged=true", &plaintext,)
                .await,
            StatusCode::OK
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn fault_principal_comes_from_authentication_not_the_request_body() {
        let (token, plaintext) = named_user_token("alice", crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            workload_fault_static_capabilities(),
            None,
        )
        .await;
        let body = workload_fault_body(true, "mallory");
        assert_eq!(
            post_status(app.clone(), "/v1/fault", &plaintext, &body).await,
            StatusCode::OK
        );

        let (status, body) = get_authenticated(app, "/v1/fault", &plaintext).await;
        assert_eq!(status, StatusCode::OK);
        let faults: Vec<crate::smoker::types::FaultSummary> =
            serde_json::from_slice(&body).unwrap();
        assert_eq!(faults[0].injected_by, "alice");
        shutdown.cancel();
    }

    #[tokio::test]
    async fn deployer_cannot_inject_a_workload_fault_without_the_server_grant() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth(vec![token], None).await;
        let status = post_status(
            app,
            "/v1/fault",
            &plaintext,
            &workload_fault_body(true, "untrusted"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn workload_fault_grant_still_requires_explicit_acknowledgement() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            workload_fault_static_capabilities(),
            None,
        )
        .await;
        let status = post_status(
            app,
            "/v1/fault",
            &plaintext,
            &workload_fault_body(false, "untrusted"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn workload_fault_grant_allows_an_acknowledging_deployer() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            workload_fault_static_capabilities(),
            None,
        )
        .await;
        let status = post_status(
            app,
            "/v1/fault",
            &plaintext,
            &workload_fault_body(true, "untrusted"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn workload_fault_clear_needs_the_grant_but_not_injection_acknowledgement() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth(vec![token], None).await;
        assert_eq!(
            delete_authenticated(app, "/v1/fault/999", &plaintext).await,
            StatusCode::FORBIDDEN
        );
        shutdown.cancel();

        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            workload_fault_static_capabilities(),
            None,
        )
        .await;
        assert_eq!(
            delete_authenticated(app, "/v1/fault/999", &plaintext).await,
            StatusCode::OK
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn injected_and_cleared_faults_emit_authenticated_structured_audit_events() {
        let (token, plaintext) = named_user_token("alice", crate::sesame::types::ApiRole::Deployer);
        let events = Arc::new(RwLock::new(crate::bun::events::EventStore::new()));
        let (app, shutdown) = setup_with_auth_readiness_leases_and_events(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            workload_fault_static_capabilities(),
            None,
            Some(Arc::clone(&events)),
        )
        .await;
        let (status, body) = post_authenticated(
            app.clone(),
            "/v1/fault",
            &plaintext,
            &workload_fault_body(true, "mallory"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let summary: crate::smoker::types::FaultSummary = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            delete_authenticated(app, &format!("/v1/fault/{}", summary.id), &plaintext).await,
            StatusCode::OK
        );

        let recorded = events.read().await.recent(10, None, None);
        assert_eq!(recorded.len(), 2);
        let injected = &recorded[0];
        assert_eq!(injected.action.as_deref(), Some("fault.injected"));
        assert!(
            injected
                .principal
                .as_deref()
                .is_some_and(|principal| principal.starts_with("token:")),
            "audit principal must identify the authenticated credential"
        );
        assert_eq!(
            injected.details.get("fault_type").map(String::as_str),
            Some("DnsNxdomain")
        );
        assert_eq!(
            injected.details.get("duration_seconds").map(String::as_str),
            Some("30")
        );
        assert!(!injected.message.contains("mallory"));
        let cleared = &recorded[1];
        assert_eq!(cleared.action.as_deref(), Some("fault.cleared"));
        assert_eq!(cleared.principal, injected.principal);
        assert_eq!(
            cleared.details.get("fault_id"),
            Some(&summary.id.to_string())
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn lease_policy_denies_provisioning_by_default() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth(vec![token], None).await;
        let (status, _) = post_authenticated(
            app,
            "/v1/test/leases",
            &plaintext,
            r#"{"ttl_seconds":30}"#,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn node_job_lease_scope_is_fenced_and_stays_on_its_receiving_node() {
        let council = seeded_council("node-jobs").await;
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (other, other_text) =
            named_user_token("other", crate::sesame::types::ApiRole::Deployer);
        let (mut scoped, scoped_text) =
            named_user_token("scoped", crate::sesame::types::ApiRole::Deployer);
        scoped.scope.apps = Some(vec!["batch".into()]);
        let tokens = vec![token, other, scoped];
        let (app, shutdown) = setup_with_auth_leases_events_and_council(
            tokens.clone(),
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            None,
            None,
            Some(Arc::clone(&council)),
        )
        .await;
        let body = r#"{"ttl_seconds":60,"scope":"node_jobs"}"#;
        assert_eq!(
            post_authenticated(app.clone(), "/v1/test/leases", &scoped_text, body, None)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (status, bytes) =
            post_authenticated(app.clone(), "/v1/test/leases", &plaintext, body, None).await;
        assert_eq!(status, StatusCode::CREATED);
        let lease: crate::testkit::lease::TestLease = serde_json::from_slice(&bytes).unwrap();
        assert!(council.desired_state().await.test_leases.is_empty());
        for body in [
            format!(
                r#"{{"ttl_seconds":60,"scope":"node_jobs","namespace":"{}"}}"#,
                lease.namespace
            ),
            format!(r#"{{"ttl_seconds":60,"namespace":"{}"}}"#, lease.namespace),
        ] {
            assert_eq!(
                post_authenticated(app.clone(), "/v1/test/leases", &plaintext, &body, None)
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            post_authenticated(
                app.clone(),
                "/v1/apply",
                &plaintext,
                "[app.web]\nimage = 'test:v1'",
                Some(&lease.lease_id)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let job = "[job.batch]\nimage = 'test:v1'";
        assert_eq!(
            post_authenticated(
                app.clone(),
                "/v1/apply",
                &other_text,
                job,
                Some(&lease.lease_id)
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            post_authenticated(
                app.clone(),
                "/v1/apply",
                &plaintext,
                job,
                Some(&lease.lease_id)
            )
            .await
            .0,
            StatusCode::OK
        );
        let path = format!("/v1/test/leases/{}", lease.lease_id);
        assert_eq!(
            post_authenticated(
                app.clone(),
                &format!("{path}/renew"),
                &other_text,
                r#"{"ttl_seconds":60}"#,
                None
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            delete_authenticated(app.clone(), &path, &other_text).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            post_authenticated(
                app.clone(),
                &format!("{path}/renew"),
                &plaintext,
                r#"{"ttl_seconds":60}"#,
                None
            )
            .await
            .0,
            StatusCode::OK
        );

        // An uninitialised council has no leader to forward to. All node-job
        // routes must still answer from this node's own store.
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
        use crate::council::{
            CouncilNode, log_store::MemLogStore, state_machine::CouncilStateMachine,
        };
        let uninitialised = Arc::new(
            CouncilNode::new(
                2,
                crate::council::types::CouncilConfig::default(),
                InMemoryRaftNetworkFactory::new(2, InMemoryRaftRouter::new()),
                MemLogStore::new(),
                CouncilStateMachine::new(),
                None,
            )
            .await
            .unwrap(),
        );
        let (wrong_node, wrong_shutdown) = setup_with_auth_leases_events_and_council(
            tokens,
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            None,
            None,
            Some(Arc::clone(&uninitialised)),
        )
        .await;
        assert_eq!(
            get_authenticated(wrong_node.clone(), &path, &plaintext)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post_authenticated(
                wrong_node.clone(),
                &format!("{path}/renew"),
                &plaintext,
                r#"{"ttl_seconds":60}"#,
                None
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            delete_authenticated(wrong_node.clone(), &path, &plaintext).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post_authenticated(
                wrong_node.clone(),
                "/v1/apply",
                &plaintext,
                job,
                Some(&lease.lease_id)
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post_authenticated(wrong_node, "/v1/test/leases", &plaintext, body, None)
                .await
                .0,
            StatusCode::CREATED
        );
        assert_eq!(
            get_authenticated(app.clone(), &path, &plaintext).await.0,
            StatusCode::OK
        );
        assert_eq!(
            delete_authenticated(app, &path, &plaintext).await,
            StatusCode::NO_CONTENT
        );
        assert!(council.desired_state().await.test_leases.is_empty());
        shutdown.cancel();
        wrong_shutdown.cancel();
        council.raft().shutdown().await.unwrap();
        uninitialised.raft().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn node_job_lease_persists_jobs_and_reclaims_their_schedule() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("leases.json");
        let store = crate::testkit::lease::LocalLeaseStore::open(path.clone())
            .await
            .unwrap();
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            Some(store),
        )
        .await;
        let (status, body) = post_authenticated(
            app.clone(),
            "/v1/test/leases",
            &plaintext,
            r#"{"ttl_seconds":60,"scope":"node_jobs"}"#,
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let lease: crate::testkit::lease::TestLease = serde_json::from_slice(&body).unwrap();
        assert!(lease.lease_id.starts_with("node-jobs-"));
        assert!(lease.namespace.starts_with("rbtest-node-"));
        let (status, body) = post_authenticated(
            app.clone(),
            "/v1/apply",
            &plaintext,
            r#"[job.batch]
image = "test:v1"
[job.scheduled]
image = "test:v1"
schedule = "* * * * *"
"#,
            Some(&lease.lease_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let reopened = crate::testkit::lease::LocalLeaseStore::open(path)
            .await
            .unwrap();
        let owned = reopened.get(&lease.lease_id).await.unwrap();
        let owned_json = serde_json::to_value(&owned).unwrap();
        assert_eq!(owned.resources.len(), 2);
        assert!(
            owned_json["resources"]
                .as_array()
                .unwrap()
                .iter()
                .all(|resource| resource["kind"] == "job")
        );
        assert_eq!(
            delete_authenticated(
                app.clone(),
                &format!("/v1/test/leases/{}", lease.lease_id),
                &plaintext
            )
            .await,
            StatusCode::NO_CONTENT
        );
        let (_, status) = get_authenticated(app.clone(), "/v1/status", &plaintext).await;
        let instances: serde_json::Value = serde_json::from_slice(&status).unwrap();
        assert!(
            !String::from_utf8_lossy(&status).contains(&lease.namespace),
            "{instances}"
        );
        assert_eq!(
            get_authenticated(
                app,
                &format!("/v1/test/leases/{}", lease.lease_id),
                &plaintext
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn lease_reads_forward_user_authority_and_refuse_an_isolated_leader() {
        use crate::council::log_store::MemLogStore;
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
        use crate::council::state_machine::CouncilStateMachine;
        use crate::council::types::{CouncilConfig, CouncilNodeInfo};
        use crate::council::{CouncilNode, CouncilResponse, RaftRequest};
        let network = InMemoryRaftRouter::new();
        let mut nodes = Vec::new();
        let mut listeners = Vec::new();
        let mut members = std::collections::BTreeMap::new();
        for id in 1..=3 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            members.insert(
                id,
                CouncilNodeInfo {
                    addr: std::net::SocketAddr::new(address.ip(), address.port() - 3),
                    name: format!("node-{id}"),
                },
            );
            listeners.push(listener);
            let node = Arc::new(
                CouncilNode::new(
                    id,
                    CouncilConfig::default(),
                    InMemoryRaftNetworkFactory::new(id, network.clone()),
                    MemLogStore::new(),
                    CouncilStateMachine::new(),
                    None,
                )
                .await
                .unwrap(),
            );
            network.register(id, node.raft().clone()).await;
            nodes.push(node);
        }
        nodes[0].initialize(members).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !nodes[0].is_leader().await {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let (owner, owner_key) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (stranger, stranger_key) =
            named_user_token("stranger", crate::sesame::types::ApiRole::Deployer);
        let now = crate::testkit::lease::now_unix_millis();
        let lease = crate::testkit::lease::TestLease::new(
            "read-quorum".into(),
            crate::sesame::auth::authenticate(&owner_key, std::slice::from_ref(&owner))
                .unwrap()
                .principal_id,
            owner.name.clone(),
            "rbtest-read-quorum".into(),
            now,
            now + 60_000,
        )
        .unwrap();
        assert!(!matches!(
            nodes[0]
                .write(RaftRequest::TestLeaseCreate(lease))
                .await
                .unwrap(),
            CouncilResponse::Refused { .. }
        ));
        let leader_port = listeners[0].local_addr().unwrap().port();
        let mut routers = Vec::new();
        let mut stops = Vec::new();
        let mut servers = Vec::new();
        for (node, listener) in nodes.iter().zip(listeners) {
            let (commands, _receiver) = mpsc::channel(4);
            let store = crate::sesame::auth::new_token_store();
            *store.write().await = vec![owner.clone(), stranger.clone()];
            let router = router(
                commands,
                None,
                None,
                None,
                None,
                None,
                Some(node.clone()),
                Some(store),
                Some("internal".into()),
                None,
                None,
                None,
                leader_port,
                None,
            );
            let stop = CancellationToken::new();
            routers.push(router.clone());
            let cancelled = stop.clone();
            servers.push(tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(async move { cancelled.cancelled().await })
                    .await
                    .unwrap();
            }));
            stops.push(stop);
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while nodes[1].current_leader().await != Some(1) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let path = "/v1/test/leases/read-quorum";
        assert_eq!(
            get_authenticated(routers[1].clone(), path, &owner_key)
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            get_authenticated(routers[1].clone(), path, &stranger_key)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let looped = routers[1]
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .header("authorization", format!("Bearer {owner_key}"))
                    .header("x-reliaburger-lease-forwarded", "1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(looped.status(), StatusCode::SERVICE_UNAVAILABLE);
        network.partition(1, 2).await;
        network.partition(1, 3).await;
        // The old leader still knows the record and believes it leads. Without
        // quorum, neither presence nor absence is cleanup evidence.
        for path in [path, "/v1/test/leases/missing"] {
            assert_eq!(
                get_authenticated(routers[0].clone(), path, &owner_key)
                    .await
                    .0,
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
        assert_eq!(
            get_authenticated(routers[0].clone(), "/v1/placements/worker", "internal")
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        for stop in stops {
            stop.cancel();
        }
        for node in nodes {
            node.shutdown().await.unwrap();
        }
        for server in servers {
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn lease_created_through_a_lagging_follower_is_in_its_replica_when_returned() {
        use crate::council::CouncilNode;
        use crate::council::log_store::MemLogStore;
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
        use crate::council::state_machine::CouncilStateMachine;
        use crate::council::types::{CouncilConfig, CouncilNodeInfo};
        let network = InMemoryRaftRouter::new();
        let mut nodes = Vec::new();
        let mut listeners = Vec::new();
        let mut members = std::collections::BTreeMap::new();
        for id in 1..=3 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            members.insert(
                id,
                CouncilNodeInfo {
                    addr: std::net::SocketAddr::new(address.ip(), address.port() - 3),
                    name: format!("node-{id}"),
                },
            );
            listeners.push(listener);
            let node = Arc::new(
                CouncilNode::new(
                    id,
                    CouncilConfig::default(),
                    InMemoryRaftNetworkFactory::new(id, network.clone()),
                    MemLogStore::new(),
                    CouncilStateMachine::new(),
                    None,
                )
                .await
                .unwrap(),
            );
            network.register(id, node.raft().clone()).await;
            nodes.push(node);
        }
        nodes[0].initialize(members).await.unwrap();
        let leader_port = listeners[0].local_addr().unwrap().port();
        let (owner, owner_key) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let mut routers = Vec::new();
        let mut stops = Vec::new();
        let mut servers = Vec::new();
        for (node, listener) in nodes.iter().zip(listeners) {
            let (commands, _receiver) = mpsc::channel(4);
            let store = crate::sesame::auth::new_token_store();
            *store.write().await = vec![owner.clone()];
            let router = router_with_upgrade(
                commands,
                None,
                None,
                None,
                None,
                None,
                Some(node.clone()),
                Some(store),
                None,
                None,
                None,
                None,
                None,
                leader_port,
                None,
                None,
                None,
                "default".to_string(),
                None,
                900,
                crate::cluster::ClusterHttp::plaintext(),
                5050,
                "http",
                256 * 1024 * 1024,
                false,
                lease_static_capabilities(),
                crate::bun::readiness::ReadinessTracker::new(),
                None,
                None,
            );
            let stop = CancellationToken::new();
            routers.push(router.clone());
            let cancelled = stop.clone();
            servers.push(tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(async move { cancelled.cancelled().await })
                    .await
                    .unwrap();
            }));
            stops.push(stop);
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !nodes[0].is_leader().await || nodes[2].current_leader().await != Some(1) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        // Node 3 misses replication for well under an election timeout, so
        // the leader and node 2 commit the lease without it.
        network.partition(1, 3).await;
        let healer = {
            let network = network.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                network.heal().await;
            })
        };
        let (status, body) = post_authenticated(
            routers[2].clone(),
            "/v1/test/leases",
            &owner_key,
            r#"{"ttl_seconds":60}"#,
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let lease: crate::testkit::lease::TestLease = serde_json::from_slice(&body).unwrap();
        // The caller's next request, an apply under this lease, checks node
        // 3's own replica before it forwards.
        assert!(
            nodes[2]
                .desired_state()
                .await
                .test_leases
                .contains_key(&lease.lease_id),
            "node 3 returned a lease its own replica did not hold yet"
        );

        healer.await.unwrap();
        for stop in stops {
            stop.cancel();
        }
        for node in nodes {
            node.shutdown().await.unwrap();
        }
        for server in servers {
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn decommission_requires_unscoped_operator_attestation_and_records_its_principal() {
        let council = seeded_council("decommission").await;
        let (admin, admin_key) = named_user_token("operator", crate::sesame::types::ApiRole::Admin);
        let expected_principal =
            crate::sesame::auth::authenticate(&admin_key, std::slice::from_ref(&admin))
                .unwrap()
                .principal_id;
        let (mut scoped, scoped_key) =
            named_user_token("scoped", crate::sesame::types::ApiRole::Admin);
        scoped.scope.namespaces = Some(vec!["default".into()]);
        let (deployer, deployer_key) =
            named_user_token("deployer", crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_leases_events_and_council(
            vec![admin, scoped, deployer],
            Some("internal".into()),
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            None,
            None,
            Some(council.clone()),
        )
        .await;
        let body = serde_json::json!({"node_id":"worker", "workloads_stopped":true, "reason":"powered off for maintenance"}).to_string();
        let path = "/v1/nodes/decommission";
        for (key, expected) in [
            (&scoped_key, StatusCode::FORBIDDEN),
            (&deployer_key, StatusCode::FORBIDDEN),
            (&"internal".into(), StatusCode::FORBIDDEN),
            (&"unknown".into(), StatusCode::UNAUTHORIZED),
        ] {
            assert_eq!(
                post_authenticated(app.clone(), path, key, &body, None)
                    .await
                    .0,
                expected
            );
        }
        for body in [
            r#"{"node_id":"worker","workloads_stopped":false,"reason":"maintenance"}"#,
            r#"{"node_id":"worker","workloads_stopped":true,"reason":" "}"#,
        ] {
            assert_eq!(
                post_authenticated(app.clone(), path, &admin_key, body, None)
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        let (status, bytes) = post_authenticated(app.clone(), path, &admin_key, &body, None).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record["retired_by"], expected_principal);
        assert_eq!(record["node_id"], "worker");
        let (status, again) = post_authenticated(app.clone(), path, &admin_key, &body, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&again).unwrap(),
            record
        );
        assert_eq!(
            get_authenticated(app, "/v1/placements/worker", "internal")
                .await
                .0,
            StatusCode::GONE
        );
        shutdown.cancel();
        council.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn credential_free_placements_serve_discovery_but_a_service_token_requires_authentication()
     {
        let council = seeded_council("endpoint-consumer-development").await;
        for service in [None, Some("internal".to_string())] {
            let protected = service.is_some();
            let (app, shutdown) = setup_with_auth_leases_events_and_council(
                vec![],
                service,
                crate::bun::readiness::ReadinessTracker::new(),
                lease_static_capabilities(),
                None,
                None,
                Some(council.clone()),
            )
            .await;
            let node = if protected {
                "protected-worker"
            } else {
                "development-worker"
            };
            assert_eq!(
                get_status(app, &format!("/v1/placements/{node}"), None).await,
                if protected {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::OK
                }
            );
            // Neither poll carries a TLS identity, so neither may owe receipts.
            assert!(
                !council
                    .desired_state()
                    .await
                    .endpoint_consumers
                    .contains(node)
            );
            shutdown.cancel();
        }
        council.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn placements_expose_only_the_consumers_original_withdrawal_generations() {
        use crate::council::{CouncilResponse, RaftRequest};
        use crate::onion::catalog::{CatalogBackend, EndpointCatalog};
        use crate::onion::service_id::ServiceId;

        let council = seeded_council("withdrawal-instructions").await;
        let (app, shutdown) = setup_with_auth_leases_events_and_council(
            vec![],
            Some("internal".into()),
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            None,
            None,
            Some(council.clone()),
        )
        .await;
        // Only TLS-authenticated polls register consumers, so enrol directly.
        for consumer in ["worker", "other-worker"] {
            council
                .write(RaftRequest::RegisterEndpointConsumer {
                    node_id: consumer.into(),
                })
                .await
                .unwrap();
            assert_eq!(
                get_authenticated(
                    app.clone(),
                    &format!("/v1/placements/{consumer}"),
                    "internal"
                )
                .await
                .0,
                StatusCode::OK
            );
        }
        let mut catalog = EndpointCatalog::default();
        let mut originals = Vec::new();
        for generation in 1..=3_u64 {
            catalog = catalog
                .reconcile([(
                    ServiceId::new("default", "web"),
                    8080,
                    vec![CatalogBackend {
                        execution: Some(crate::grill::RuntimeExecution {
                            instance_id: crate::grill::InstanceId("default__web-0".into()),
                            generation: format!("{generation:064x}").try_into().unwrap(),
                        }),
                        node_id: "producer".into(),
                        node_ip: "127.0.0.1".parse().unwrap(),
                        host_port: 18080 + generation as u16,
                        healthy: true,
                    }],
                )])
                .unwrap();
            assert!(matches!(
                council
                    .write(RaftRequest::PublishEndpoints {
                        expected_generation: generation - 1,
                        catalog: Box::new(catalog.clone()),
                    })
                    .await
                    .unwrap(),
                CouncilResponse::Applied { .. }
            ));
            originals.push(serde_json::to_value(&catalog.services["default__web"]).unwrap());
            if generation == 2 {
                council
                    .write(RaftRequest::RegisterEndpointConsumer {
                        node_id: "late-worker".into(),
                    })
                    .await
                    .unwrap();
                let (status, bytes) =
                    get_authenticated(app.clone(), "/v1/placements/late-worker", "internal").await;
                assert_eq!(status, StatusCode::OK);
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(body["endpoint_generation"], 2);
                assert_eq!(body["endpoint_withdrawals"], serde_json::json!([]));
            }
        }
        let before = council.desired_state().await;
        for (consumer, expected) in [
            ("worker", vec![1, 2]),
            ("other-worker", vec![1, 2]),
            ("late-worker", vec![2]),
        ] {
            let (status, bytes) = get_authenticated(
                app.clone(),
                &format!("/v1/placements/{consumer}"),
                "internal",
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["endpoint_generation"], 3);
            assert_eq!(
                body["endpoint_catalog"],
                serde_json::to_value(&catalog).unwrap()
            );
            let instructions = body["endpoint_withdrawals"].as_array().unwrap();
            assert_eq!(instructions.len(), expected.len());
            for (instruction, generation) in instructions.iter().zip(expected) {
                assert_eq!(instruction["generation"], generation);
                assert_eq!(
                    instruction["services"]["default__web"]["service"],
                    originals[generation as usize - 1]
                );
                assert_eq!(instruction["services"]["default__web"]["retire_vip"], false);
                assert!(
                    instruction.get("consumers").is_none(),
                    "do not expose another consumer's obligations"
                );
            }
            // The receiving worker must retain the same exact instructions when decoding.
            let decoded: crate::cluster::orchestrate::NodeAssignments =
                serde_json::from_slice(&bytes).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), body);
        }
        let after = council.desired_state().await;
        assert_eq!(before.last_applied_log, after.last_applied_log);
        assert_eq!(
            before.endpoint_withdrawals, after.endpoint_withdrawals,
            "serving instructions is not a cleanup acknowledgement"
        );
        assert!(matches!(
            council
                .write(RaftRequest::PublishEndpoints {
                    expected_generation: 3,
                    catalog: Box::new(EndpointCatalog::default()),
                })
                .await
                .unwrap(),
            CouncilResponse::Applied { .. }
        ));
        let (status, bytes) =
            get_authenticated(app, "/v1/placements/late-worker", "internal").await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["endpoint_generation"], 4);
        assert_eq!(
            body["endpoint_catalog"],
            serde_json::to_value(EndpointCatalog::default()).unwrap()
        );
        let instructions = body["endpoint_withdrawals"].as_array().unwrap();
        assert_eq!(instructions.len(), 2);
        assert_eq!(instructions[1]["generation"], 3);
        assert_eq!(
            instructions[1]["services"]["default__web"]["service"],
            originals[2]
        );
        assert_eq!(
            instructions[1]["services"]["default__web"]["retire_vip"],
            true
        );
        shutdown.cancel();
        council.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unparseable_peer_identity_is_refused_before_any_route() {
        let council = seeded_council("unparseable-peer").await;
        let (app, shutdown) = setup_with_auth_leases_events_and_council(
            vec![],
            Some("internal".into()),
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            None,
            None,
            Some(council.clone()),
        )
        .await;
        // The handshake verifier normally rejects this first; if anything
        // slips past it, the retirement check must not wave it through.
        let app = app.layer(axum::Extension(crate::sesame::renewal::TlsPeerCertificate(
            Vec::from(b"not a certificate".as_slice()).into(),
        )));
        assert_eq!(
            get_status(app, "/v1/health", None).await,
            StatusCode::FORBIDDEN
        );
        shutdown.cancel();
        council.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn placements_serve_plaintext_discovery_without_registering_a_consumer() {
        let council = seeded_council("endpoint-consumer").await;
        let (token, user_key) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let (app, shutdown) = setup_with_auth_leases_events_and_council(
            vec![token],
            Some("internal".into()),
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            None,
            None,
            Some(council.clone()),
        )
        .await;
        let path = "/v1/placements/worker";
        assert_eq!(
            get_authenticated(app.clone(), path, &user_key).await.0,
            StatusCode::FORBIDDEN
        );
        assert!(council.desired_state().await.endpoint_consumers.is_empty());
        assert_eq!(
            get_authenticated(app.clone(), path, "internal").await.0,
            StatusCode::OK
        );
        // Receipts need a TLS identity; tests/suite/endpoint_withdrawal.rs covers
        // registration for authenticated consumers.
        assert!(
            council.desired_state().await.endpoint_consumers.is_empty(),
            "a plaintext poll registered an obligation nobody can discharge"
        );
        let invalid_peer =
            app.clone()
                .layer(axum::Extension(crate::sesame::renewal::TlsPeerCertificate(
                    Vec::from(b"invalid certificate".as_slice()).into(),
                )));
        assert_eq!(
            get_authenticated(invalid_peer, "/v1/placements/imposter", "internal")
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert!(
            !council
                .desired_state()
                .await
                .endpoint_consumers
                .contains("imposter")
        );
        let applied = council.desired_state().await.last_applied_log;
        assert_eq!(
            get_authenticated(app, path, "internal").await.0,
            StatusCode::OK
        );
        assert_eq!(
            council.desired_state().await.last_applied_log,
            applied,
            "an unchanged placement poll must not write another registration"
        );
        shutdown.cancel();
        council.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cluster_lease_delete_waits_for_system_retirement_acknowledgements() {
        use crate::council::{CouncilResponse, RaftRequest};
        use crate::meat::{AppId, NodeId, Placement, Resources, SchedulingDecision};
        let council = seeded_council("lease-retirement").await;
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let service = "retirement-service-secret";
        let (app, shutdown) = setup_with_auth_leases_events_and_council(
            vec![token],
            Some(service.into()),
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            None,
            None,
            Some(council.clone()),
        )
        .await;
        let (status, body) = post_authenticated(
            app.clone(),
            "/v1/test/leases",
            &plaintext,
            r#"{"ttl_seconds":60}"#,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let lease: crate::testkit::lease::TestLease = serde_json::from_slice(&body).unwrap();
        let app_id = AppId::new("web", &lease.namespace);
        let config = crate::config::Config::parse("[app.web]\nimage = \"test:v1\"\n").unwrap();
        let mut spec = config.app["web"].clone();
        spec.namespace = Some(lease.namespace.clone());
        for request in [
            RaftRequest::TestLeaseAppSpec {
                lease_id: lease.lease_id.clone(),
                observed_at_unix_ms: crate::testkit::lease::now_unix_millis(),
                app_id: app_id.clone(),
                spec: Box::new(spec),
            },
            RaftRequest::SchedulingDecision(SchedulingDecision {
                app_id: app_id.clone(),
                placements: vec![Placement {
                    node_id: NodeId::new("worker"),
                    resources: Resources::new(500, 1024, 0),
                }],
            }),
        ] {
            assert!(!matches!(
                council.write(request).await.unwrap(),
                CouncilResponse::Refused { .. }
            ));
        }
        let path = format!("/v1/test/leases/{}", lease.lease_id);
        assert_eq!(
            delete_authenticated(app.clone(), &path, &plaintext).await,
            StatusCode::ACCEPTED
        );
        let (status, body) = get_authenticated(app.clone(), "/v1/placements/worker", service).await;
        assert_eq!(status, StatusCode::OK);
        let assignments: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(assignments["retirements"].as_array().map(Vec::len), Some(1));
        let acknowledgement = serde_json::json!({ "lease_id": lease.lease_id,
            "placement": {"app_id": app_id, "node_id": "worker"} })
        .to_string();
        assert_eq!(
            post_authenticated(
                app.clone(),
                "/v1/test/leases/retired",
                &plaintext,
                &acknowledgement,
                None
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            post_authenticated(
                app.clone(),
                "/v1/test/leases/retired",
                "unknown",
                &acknowledgement,
                None
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        for _ in 0..2 {
            assert_eq!(
                post_authenticated(
                    app.clone(),
                    "/v1/test/leases/retired",
                    service,
                    &acknowledgement,
                    None
                )
                .await
                .0,
                StatusCode::NO_CONTENT
            );
        }
        assert_eq!(
            delete_authenticated(app.clone(), &path, &plaintext).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            get_authenticated(app, &path, &plaintext).await.0,
            StatusCode::NOT_FOUND
        );
        shutdown.cancel();
        council.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lease_owns_apply_and_release_confirms_cleanup() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            None,
        )
        .await;
        let (status, body) = post_authenticated(
            app.clone(),
            "/v1/test/leases",
            &plaintext,
            r#"{"ttl_seconds":30}"#,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let lease: crate::testkit::lease::TestLease = serde_json::from_slice(&body).unwrap();

        let (status, body) = post_authenticated(
            app.clone(),
            "/v1/apply",
            &plaintext,
            r#"
                [app.probe]
                image = "test:v1"
            "#,
            Some(&lease.lease_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

        let (status, body) = get_authenticated(
            app.clone(),
            &format!("/v1/test/leases/{}", lease.lease_id),
            &plaintext,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let owned: crate::testkit::lease::TestLease = serde_json::from_slice(&body).unwrap();
        assert_eq!(owned.resources.len(), 1);
        assert!(
            owned
                .resources
                .contains(&crate::testkit::lease::LeasedResource::App {
                    app_id: crate::meat::AppId::new("probe", &lease.namespace),
                })
        );

        assert_eq!(
            delete_authenticated(
                app.clone(),
                &format!("/v1/test/leases/{}", lease.lease_id),
                &plaintext,
            )
            .await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            get_authenticated(
                app,
                &format!("/v1/test/leases/{}", lease.lease_id),
                &plaintext,
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn capacity_apply_requires_admin_and_server_capacity_grant() {
        let (deployer_token, deployer_plaintext) =
            a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (deployer_app, deployer_shutdown) = setup_with_auth_readiness_and_leases(
            vec![deployer_token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            capacity_static_capabilities(),
            None,
        )
        .await;
        let (_, body) = post_authenticated(
            deployer_app.clone(),
            "/v1/test/leases",
            &deployer_plaintext,
            r#"{"ttl_seconds":30}"#,
            None,
        )
        .await;
        let lease: crate::testkit::lease::TestLease = serde_json::from_slice(&body).unwrap();
        let (status, _) = post_capacity_apply(
            deployer_app,
            &deployer_plaintext,
            &lease.lease_id,
            &lease.namespace,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        deployer_shutdown.cancel();

        let (ungranted_token, ungranted_plaintext) =
            a_user_token(crate::sesame::types::ApiRole::Admin);
        let (ungranted_app, ungranted_shutdown) = setup_with_auth_readiness_and_leases(
            vec![ungranted_token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            None,
        )
        .await;
        let (_, body) = post_authenticated(
            ungranted_app.clone(),
            "/v1/test/leases",
            &ungranted_plaintext,
            r#"{"ttl_seconds":30}"#,
            None,
        )
        .await;
        let lease: crate::testkit::lease::TestLease = serde_json::from_slice(&body).unwrap();
        let (status, _) = post_capacity_apply(
            ungranted_app,
            &ungranted_plaintext,
            &lease.lease_id,
            &lease.namespace,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        ungranted_shutdown.cancel();

        let (admin_token, admin_plaintext) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let (admin_app, admin_shutdown) = setup_with_auth_readiness_and_leases(
            vec![admin_token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            capacity_static_capabilities(),
            None,
        )
        .await;
        let (_, body) = post_authenticated(
            admin_app.clone(),
            "/v1/test/leases",
            &admin_plaintext,
            r#"{"ttl_seconds":30}"#,
            None,
        )
        .await;
        let lease: crate::testkit::lease::TestLease = serde_json::from_slice(&body).unwrap();
        let (status, body) = post_capacity_apply(
            admin_app,
            &admin_plaintext,
            &lease.lease_id,
            &lease.namespace,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            String::from_utf8_lossy(&body)
        );
        assert!(String::from_utf8_lossy(&body).contains("live cluster scheduler"));
        admin_shutdown.cancel();
    }

    #[tokio::test]
    async fn lease_ttl_is_server_bounded() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            None,
        )
        .await;
        assert_eq!(
            post_authenticated(
                app.clone(),
                "/v1/test/leases",
                &plaintext,
                r#"{"ttl_seconds":0}"#,
                None,
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post_authenticated(
                app,
                "/v1/test/leases",
                &plaintext,
                r#"{"ttl_seconds":61}"#,
                None,
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn lease_mutations_require_the_exact_credential_and_renew_active_records() {
        let (owner_token, owner_plaintext) =
            named_user_token("owner", crate::sesame::types::ApiRole::Deployer);
        let (other_token, other_plaintext) =
            named_user_token("owner", crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![owner_token, other_token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            None,
        )
        .await;
        let (_, body) = post_authenticated(
            app.clone(),
            "/v1/test/leases",
            &owner_plaintext,
            r#"{"ttl_seconds":30}"#,
            None,
        )
        .await;
        let lease: crate::testkit::lease::TestLease = serde_json::from_slice(&body).unwrap();
        let renew_path = format!("/v1/test/leases/{}/renew", lease.lease_id);
        assert_eq!(
            post_authenticated(
                app.clone(),
                &renew_path,
                &other_plaintext,
                r#"{"ttl_seconds":40}"#,
                None,
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let (status, body) = post_authenticated(
            app.clone(),
            &renew_path,
            &owner_plaintext,
            r#"{"ttl_seconds":40}"#,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let renewed: crate::testkit::lease::TestLease = serde_json::from_slice(&body).unwrap();
        assert!(renewed.expires_at_unix_ms > lease.expires_at_unix_ms);
        assert_eq!(
            delete_authenticated(
                app,
                &format!("/v1/test/leases/{}", lease.lease_id),
                &other_plaintext,
            )
            .await,
            StatusCode::FORBIDDEN
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn reserved_test_namespace_cannot_bypass_a_lease() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Deployer);
        let (app, shutdown) = setup_with_auth_readiness_and_leases(
            vec![token],
            None,
            crate::bun::readiness::ReadinessTracker::new(),
            lease_static_capabilities(),
            None,
        )
        .await;
        let (job_status, _) = post_authenticated(
            app.clone(),
            "/v1/apply",
            &plaintext,
            "[job.probe]\nimage = \"test:v1\"\nnamespace = \"rbtest-unleased\"\n",
            None,
        )
        .await;
        assert_eq!(
            job_status,
            StatusCode::CONFLICT,
            "unleased test jobs must not reach the agent"
        );
        let (status, body) = post_authenticated(
            app.clone(),
            "/v1/apply",
            &plaintext,
            r#"
                [app.probe]
                image = "test:v1"
                namespace = "rbtest-unleased"
            "#,
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let (status, body) = post_authenticated(
            app,
            "/v1/apply",
            &plaintext,
            r#"
                [namespace.rbtest-unleased]
                max_apps = 1
            "#,
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{}",
            String::from_utf8_lossy(&body)
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
    async fn readiness_and_capability_evidence_require_a_token() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::ReadOnly);
        let (app, shutdown) = setup_with_auth(vec![token], None).await;
        assert_eq!(
            get_status(app.clone(), "/v1/readiness", None).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get_status(app.clone(), "/v1/capabilities", None).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get_status(app.clone(), "/v1/capabilities/cluster", None).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get_status(app.clone(), "/v1/readiness", Some(&plaintext)).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            get_status(app, "/v1/capabilities", Some(&plaintext)).await,
            StatusCode::OK
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn readiness_returns_ok_only_after_every_critical_owner_is_ready() {
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::ReadOnly);
        let readiness = crate::bun::readiness::ReadinessTracker::new();
        readiness.register("agent", true).await;
        readiness.register("registry", true).await;
        readiness.ready("agent").await;
        let (app, shutdown) =
            setup_with_auth_and_readiness(vec![token], None, readiness.clone()).await;
        assert_eq!(
            get_status(app.clone(), "/v1/readiness", Some(&plaintext)).await,
            StatusCode::SERVICE_UNAVAILABLE
        );

        readiness.ready("registry").await;
        assert_eq!(
            get_status(app, "/v1/readiness", Some(&plaintext)).await,
            StatusCode::OK
        );
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
            None,
            None,
            None,
            9117,
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
    async fn admin_token_may_create_a_join_token() {
        let (app, shutdown, tok) =
            setup_with_role("role-admin-join", crate::sesame::types::ApiRole::Admin).await;
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/join-token/create")
                    .header("authorization", format!("Bearer {tok}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"ttl_seconds":900,"node_id":"node-02"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["token"]
                .as_str()
                .is_some_and(|token| token.starts_with("rbrg_join_1_"))
        );
        assert_eq!(json["ttl_seconds"], 900);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn join_token_requires_a_node_id() {
        // M4: a token must be bound to a node id. A request without one is a 400,
        // not a token that could enrol anyone.
        let (app, shutdown, tok) =
            setup_with_role("role-admin-join-noid", crate::sesame::types::ApiRole::Admin).await;
        let status =
            post_status(app, "/v1/join-token/create", &tok, r#"{"ttl_seconds":900}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn non_admin_token_cannot_create_a_join_token() {
        let (app, shutdown, tok) = setup_with_role(
            "role-deployer-join",
            crate::sesame::types::ApiRole::Deployer,
        )
        .await;
        let status =
            post_status(app, "/v1/join-token/create", &tok, r#"{"ttl_seconds":900}"#).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn service_principal_cannot_create_a_join_token() {
        let (user, _plaintext) = a_user_token(crate::sesame::types::ApiRole::ReadOnly);
        let (app, shutdown) =
            setup_with_auth(vec![user], Some("rbrg_service_join_test".to_string())).await;
        let status = post_status(
            app,
            "/v1/join-token/create",
            "rbrg_service_join_test",
            r#"{"ttl_seconds":900}"#,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn join_token_ttl_is_bounded() {
        for (tag, ttl) in [("zero", 0), ("too-long", 3_601)] {
            let (app, shutdown, tok) = setup_with_role(
                &format!("join-ttl-{tag}"),
                crate::sesame::types::ApiRole::Admin,
            )
            .await;
            let status = post_status(
                app,
                "/v1/join-token/create",
                &tok,
                &serde_json::json!({ "ttl_seconds": ttl, "node_id": "node-02" }).to_string(),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "ttl={ttl}");
            shutdown.cancel();
        }
    }

    #[tokio::test]
    async fn secret_rotate_rejects_a_malformed_body() {
        // PKI8: a garbage body must not silently default to a (non-finalise)
        // rotation and mutate cluster key state on a typo. The admin passes the
        // role guard, so a 400 here is the parse gate, not authorisation.
        let (app, shutdown, tok) =
            setup_with_role("rotate-malformed", crate::sesame::types::ApiRole::Admin).await;
        let status = post_status(app, "/v1/secret/rotate", &tok, "not json at all").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn secret_rotate_accepts_an_empty_body_as_a_rotation() {
        // The convenience default: no body means "rotate" (not finalise). It
        // must reach the council, so it is anything but a 400.
        let (app, shutdown, tok) =
            setup_with_role("rotate-empty", crate::sesame::types::ApiRole::Admin).await;
        let status = post_status(app, "/v1/secret/rotate", &tok, "").await;
        assert_ne!(status, StatusCode::BAD_REQUEST);
        shutdown.cancel();
    }

    /// Like `seeded_council`, but the node holds the cluster's wrapping IKM
    /// so the rotate endpoint can mint real keypairs.
    async fn seeded_council_with_ikm(tag: &str) -> Arc<crate::council::CouncilNode> {
        use std::collections::BTreeMap;

        use crate::council::log_store::MemLogStore;
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
        use crate::council::state_machine::CouncilStateMachine;
        use crate::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest};

        let dir = std::env::temp_dir().join(format!("rb-api-seeded-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        let init = crate::sesame::init::initialize_cluster("apitest", "node-1", &dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let raft_router = InMemoryRaftRouter::new();
        let network = InMemoryRaftNetworkFactory::new(1, raft_router.clone());
        let node = crate::council::CouncilNode::new(
            1,
            CouncilConfig::default(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            Some(init.master_secret),
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

    /// Build an admin-authorised router around an existing council.
    async fn router_for_council(
        council: Arc<crate::council::CouncilNode>,
    ) -> (Router, CancellationToken, String) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());
        tokio::spawn(async move {
            agent.run().await;
        });
        let (token, plaintext) = a_user_token(crate::sesame::types::ApiRole::Admin);
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
            None,
            None,
            None,
            9117,
            None,
        );
        (app, shutdown, plaintext)
    }

    #[tokio::test]
    async fn secret_public_key_exposes_only_current_public_material_to_scoped_readers() {
        let council = seeded_council_with_ikm("public-key").await;
        let reader = crate::sesame::token::create_token(
            "public-key-reader",
            crate::sesame::types::ApiRole::ReadOnly,
            crate::sesame::types::TokenScope {
                apps: Some(vec!["web".into()]),
                namespaces: Some(vec!["team-a".into()]),
            },
            None,
        )
        .unwrap();
        let store = crate::sesame::auth::new_token_store();
        store.write().await.push(reader.token);
        let (tx, _rx) = mpsc::channel(1);
        let app = router(
            tx,
            None,
            None,
            None,
            None,
            None,
            Some(council.clone()),
            Some(store),
            None,
            None,
            None,
            None,
            0,
            None,
        );
        assert_eq!(
            get_status(app.clone(), "/v1/secret/public-key", None).await,
            StatusCode::UNAUTHORIZED
        );
        for generation in 0..=1 {
            if generation == 1 {
                let (key, _) = crate::sesame::secret::generate_age_keypair(
                    crate::sesame::types::AgeKeyScope::ClusterWide,
                    council.wrapping_ikm().unwrap(),
                    generation,
                )
                .unwrap();
                council
                    .write(crate::council::RaftRequest::RotateSecretKey {
                        scope: crate::sesame::types::AgeKeyScope::ClusterWide,
                        new_keypair: key,
                    })
                    .await
                    .unwrap();
            }
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/v1/secret/public-key")
                        .header("authorization", format!("Bearer {}", reader.plaintext))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                json.as_object().unwrap().len(),
                2,
                "public key responses must not serialise the stored keypair"
            );
            assert_eq!(json["generation"], generation);
            let security = council.security_state().await;
            let keypair = security.cluster_age_keypair().unwrap();
            assert_eq!(json["public_key"], keypair.public_key);
            let encrypted = crate::sesame::secret::encrypt_secret(
                "probe",
                json["public_key"].as_str().unwrap(),
            )
            .unwrap();
            let identity = crate::sesame::secret::unwrap_age_identity(
                keypair,
                council.wrapping_ikm().unwrap(),
            )
            .unwrap();
            assert_eq!(
                crate::sesame::secret::decrypt_secret(&encrypted, &identity).unwrap(),
                "probe"
            );
        }
    }

    #[tokio::test]
    async fn secret_public_key_refuses_when_cluster_keys_are_unavailable() {
        let (app, shutdown) = setup_with_auth(vec![], None).await;
        assert_eq!(
            get_status(app, "/v1/secret/public-key", None).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        shutdown.cancel();
    }

    /// PKI8 end-to-end: finalising while a stored secret is still sealed
    /// under the retiring generation comes back as a 409 naming the secret.
    #[tokio::test]
    async fn secret_rotate_finalize_refusal_surfaces_as_conflict() {
        let council = seeded_council_with_ikm("rotate-verify").await;

        // A deployed app with an encrypted secret, sealed under gen 0…
        let spec: crate::config::app::AppSpec = toml::from_str(
            r#"
            image = "t:v1"
            [env]
            DB_PASSWORD = "ENC[AGE:c2VhbGVk]"
            "#,
        )
        .unwrap();
        council
            .write(crate::council::RaftRequest::AppSpec {
                app_id: crate::meat::types::AppId::new("web", "default"),
                spec: Box::new(spec),
            })
            .await
            .unwrap();

        let (app, shutdown, tok) = router_for_council(council).await;

        // …a rotation starts (gen 1)…
        let status = post_status(app.clone(), "/v1/secret/rotate", &tok, "{}").await;
        assert_eq!(status, StatusCode::OK, "starting the rotation succeeds");

        // …and an early finalize is refused with the offender named.
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/secret/rotate")
                    .header("authorization", format!("Bearer {tok}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"finalize": true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap()
                .contains("default/web/DB_PASSWORD"),
            "the refusal names the stale secret: {json}"
        );
        shutdown.cancel();
    }

    /// PKI8 end-to-end: a second rotation while one is un-finalised is a 409.
    #[tokio::test]
    async fn secret_rotate_second_rotation_surfaces_as_conflict() {
        let council = seeded_council_with_ikm("rotate-concurrent").await;
        let (app, shutdown, tok) = router_for_council(council).await;

        let status = post_status(app.clone(), "/v1/secret/rotate", &tok, "{}").await;
        assert_eq!(status, StatusCode::OK);
        let status = post_status(app, "/v1/secret/rotate", &tok, "{}").await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a rotation is already in flight"
        );
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
    async fn deploy_cancellation_checks_every_target_scope_and_is_idempotent() {
        let (admin, admin_secret) = a_user_token(crate::sesame::types::ApiRole::Admin);
        let scoped = crate::sesame::token::create_token(
            "cancel-scoped",
            crate::sesame::types::ApiRole::Deployer,
            crate::sesame::types::TokenScope {
                apps: None,
                namespaces: Some(vec!["team-a".into()]),
            },
            None,
        )
        .unwrap();
        let secret = scoped.plaintext.clone();
        let (app, shutdown) = setup_with_auth(vec![admin, scoped.token], None).await;
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/apply")
                    .header("authorization", format!("Bearer {admin_secret}"))
                    .body(Body::from(
                        "[app.web]\nimage = 'web:v1'\nnamespace = 'team-b'\n",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        let operation_id = text
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|line| serde_json::from_str::<ApplyEvent>(line.trim()).ok())
            .find_map(|event| match event {
                ApplyEvent::Accepted { operation_id } => Some(operation_id),
                _ => None,
            })
            .unwrap();
        let path = format!("/v1/deploys/operations/{operation_id}/cancel");
        assert_eq!(
            post_status(app.clone(), &path, &secret, "").await,
            StatusCode::FORBIDDEN
        );
        for _ in 0..2 {
            assert_eq!(
                post_status(app.clone(), &path, &admin_secret, "").await,
                StatusCode::OK
            );
        }
        shutdown.cancel();
    }

    #[tokio::test]
    async fn deploy_cancellation_requires_deployer_and_reports_unknown_ids() {
        for (role, expected) in [
            (
                crate::sesame::types::ApiRole::ReadOnly,
                StatusCode::FORBIDDEN,
            ),
            (
                crate::sesame::types::ApiRole::Deployer,
                StatusCode::NOT_FOUND,
            ),
        ] {
            let (app, shutdown, token) = setup_with_role("cancel-role", role).await;
            let status =
                post_status(app, "/v1/deploys/operations/unknown/cancel", &token, "").await;
            assert_eq!(status, expected);
            shutdown.cancel();
        }
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

    /// Build a router whose store holds a Deployer token scoped to namespace
    /// `ns`, so AUTH1 scope enforcement can be exercised end-to-end.
    async fn setup_scoped_to_namespace(ns: &str) -> (Router, CancellationToken, String) {
        setup_scoped_with_role(ns, crate::sesame::types::ApiRole::Deployer).await
    }

    async fn setup_scoped_with_role(
        ns: &str,
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
        let scope = crate::sesame::types::TokenScope {
            apps: None,
            namespaces: Some(vec![ns.to_string()]),
        };
        let created = crate::sesame::token::create_token("scoped", role, scope, None).unwrap();
        let store = crate::sesame::auth::new_token_store();
        store.write().await.push(created.token);
        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            None, // no council: local path
            Some(store),
            None,
            None,
            None,
            None,
            9117,
            None,
        );
        (app, shutdown, created.plaintext)
    }

    /// Every route that upgrades, rolls back or re-elects the cluster.
    const CLUSTER_ADMIN_ROUTES: [&str; 7] = [
        "/v1/upgrade/apply",
        "/v1/upgrade/rollback",
        "/v1/upgrade/start",
        "/v1/upgrade/resume",
        "/v1/upgrade/abort",
        "/v1/upgrade/cluster-rollback",
        "/v1/cluster/elect",
    ];

    /// A namespace-scoped Admin clears the role gate, but these routes act on
    /// every node and every tenant: it must not start a cluster-wide upgrade.
    #[tokio::test]
    async fn scoped_admin_is_refused_cluster_wide_upgrades_and_elections() {
        for path in CLUSTER_ADMIN_ROUTES {
            let (app, shutdown, tok) =
                setup_scoped_with_role("team-a", crate::sesame::types::ApiRole::Admin).await;
            let status = post_status(app, path, &tok, "{}").await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "scoped Admin allowed on {path}"
            );
            shutdown.cancel();
        }
    }

    /// …while an unscoped Admin gets past authorisation (whatever the
    /// handler then makes of an empty body).
    #[tokio::test]
    async fn unscoped_admin_passes_the_cluster_wide_gate() {
        for path in CLUSTER_ADMIN_ROUTES {
            let (app, shutdown, tok) =
                setup_with_role("cluster-admin", crate::sesame::types::ApiRole::Admin).await;
            let status = post_status(app, path, &tok, "{}").await;
            assert!(
                status != StatusCode::FORBIDDEN && status != StatusCode::UNAUTHORIZED,
                "unscoped Admin refused on {path}: {status}"
            );
            shutdown.cancel();
        }
    }

    /// The status a node answers an upgrade directive with when preparing
    /// it fails with `error`, via a stand-in agent.
    async fn upgrade_apply_status(error: crate::upgrade::UpgradeError) -> StatusCode {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(4);
        let mut error = Some(error);
        tokio::spawn(async move {
            while let Some(command) = cmd_rx.recv().await {
                if let AgentCommand::UpgradeApply { response, .. } = command
                    && let Some(error) = error.take()
                {
                    let _ = response.send(Err(crate::bun::BunError::Upgrade(error)));
                }
            }
        });
        let created = crate::sesame::token::create_token(
            "cluster-admin",
            crate::sesame::types::ApiRole::Admin,
            crate::sesame::types::TokenScope::default(),
            None,
        )
        .unwrap();
        let store = crate::sesame::auth::new_token_store();
        store.write().await.push(created.token);
        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(store),
            None,
            None,
            None,
            None,
            9117,
            None,
        );
        let directive = crate::upgrade::types::UpgradeDirective {
            upgrade_id: "up-1".to_string(),
            target_version: "v0.2.0".parse().unwrap(),
            binary_sha256: "abc".to_string(),
            embedded_signature: String::new(),
            external_signature: None,
            source: crate::upgrade::types::BinarySource::Pickle {
                registry_address: "10.0.0.1:5050".to_string(),
            },
            network_provenance: true,
            allow_downgrade: false,
        };
        post_status(
            app,
            "/v1/upgrade/apply",
            &created.plaintext,
            &serde_json::to_string(&directive).unwrap(),
        )
        .await
    }

    /// A registry that isn't serving is "not right now": 503, which the
    /// orchestrator retries. A blob it doesn't hold, or bytes that don't
    /// verify, are a refusal: 409, which pauses the run.
    #[tokio::test]
    async fn upgrade_apply_answers_503_only_when_the_binary_source_is_unavailable() {
        use crate::upgrade::UpgradeError;
        let unavailable = UpgradeError::FetchUnavailable {
            url: "https://10.0.0.1:5050/v2/reliaburger-bun/blobs/sha256:abc".to_string(),
            reason: "error sending request".to_string(),
        };
        assert_eq!(
            upgrade_apply_status(unavailable).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let missing = UpgradeError::FetchFailed {
            url: "https://10.0.0.1:5050/v2/reliaburger-bun/blobs/sha256:abc".to_string(),
            reason: "status 404 Not Found".to_string(),
        };
        assert_eq!(upgrade_apply_status(missing).await, StatusCode::CONFLICT);
        assert_eq!(
            upgrade_apply_status(UpgradeError::EmbeddedSignatureInvalid).await,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn scoped_deployer_is_refused_stopping_outside_its_namespace() {
        // AUTH1: a Deployer scoped to `a` clears the role gate but is refused
        // on namespace `b`.
        let (app, shutdown, tok) = setup_scoped_to_namespace("a").await;
        let status = post_status(app, "/v1/stop/web/b", &tok, "").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn scoped_deployer_is_allowed_stopping_inside_its_namespace() {
        let (app, shutdown, tok) = setup_scoped_to_namespace("a").await;
        // In-scope: it passes authorisation (may 404 on a missing app, but
        // never 403).
        let status = post_status(app, "/v1/stop/web/a", &tok, "").await;
        assert_ne!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn scoped_deployer_is_refused_applying_an_out_of_scope_app() {
        // AUTH1 on apply: the manifest's namespace is out of scope.
        let (app, shutdown, tok) = setup_scoped_to_namespace("a").await;
        let manifest = "[app.web]\nimage = \"x:1\"\nnamespace = \"b\"\n";
        let status = post_status(app, "/v1/apply", &tok, manifest).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    fn deployer_context() -> crate::sesame::auth::AuthContext {
        crate::sesame::auth::AuthContext {
            token_name: "ci".into(),
            principal_id: "ci-credential".into(),
            role: crate::sesame::types::ApiRole::Deployer,
            scoped_apps: None,
            scoped_namespaces: None,
        }
    }

    async fn apply_as_context(
        app: &Router,
        auth: crate::sesame::auth::AuthContext,
        manifest: &str,
    ) -> StatusCode {
        let mut request = axum::http::Request::post("/v1/apply")
            .body(Body::from(manifest.to_owned()))
            .unwrap();
        request.extensions_mut().insert(auth);
        app.clone().oneshot(request).await.unwrap().status()
    }

    async fn workload_admission_fixture(
        tag: &str,
    ) -> (
        Router,
        Arc<crate::council::CouncilNode>,
        mpsc::Receiver<AgentCommand>,
    ) {
        let council = seeded_council(tag).await;
        let (tx, rx) = mpsc::channel(16);
        let app = router(
            tx,
            None,
            None,
            None,
            None,
            None,
            Some(council.clone()),
            None,
            None,
            None,
            None,
            None,
            0,
            None,
        );
        (app, council, rx)
    }

    #[tokio::test]
    async fn administrative_manifests_require_unscoped_user_admin_before_any_write() {
        let (app, council, mut commands) = workload_admission_fixture("manifest-admin").await;
        let declarations = [
            "[permission.ci]\nactions = [\"deploy\", \"host-exec\"]\napps = [\"*\"]\n",
            "[namespace.default]\nmax_apps = 1000\n",
        ];
        for declaration in declarations {
            for mixed in [false, true] {
                let manifest = if mixed {
                    format!(
                        "{declaration}[app.web]\nimage = \"test:v1\"\n[job.work]\nimage = \"test:v1\"\n"
                    )
                } else {
                    declaration.to_owned()
                };
                for auth in [
                    deployer_context(),
                    {
                        let mut auth = deployer_context();
                        auth.role = crate::sesame::types::ApiRole::Admin;
                        auth.scoped_namespaces = Some(vec!["default".into()]);
                        auth
                    },
                    {
                        let mut auth = deployer_context();
                        auth.role = crate::sesame::types::ApiRole::Admin;
                        auth.token_name = crate::sesame::auth::SYSTEM_PRINCIPAL.into();
                        auth
                    },
                ] {
                    assert_eq!(
                        apply_as_context(&app, auth, &manifest).await,
                        StatusCode::FORBIDDEN
                    );
                    let desired = council.desired_state().await;
                    assert!(desired.permissions.is_empty());
                    assert!(desired.namespaces.is_empty());
                    assert!(desired.apps.is_empty());
                    assert!(matches!(
                        commands.try_recv(),
                        Err(mpsc::error::TryRecvError::Empty)
                    ));
                }
            }
        }
        let mut auth = deployer_context();
        auth.role = crate::sesame::types::ApiRole::Admin;
        let mut request = axum::http::Request::post("/v1/apply")
            .body(Body::from(declarations.join("")))
            .unwrap();
        request.extensions_mut().insert(auth);
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("error"));
        let desired = council.desired_state().await;
        assert!(desired.permissions.contains_key("ci"));
        assert_eq!(desired.namespaces["default"].max_apps, Some(1000));
        council.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn explicit_job_rerun_preserves_authority_and_refuses_mixed_manifests() {
        let (app, council, mut commands) = workload_admission_fixture("rerun-admission").await;
        let manifest = "[job.work]\nimage = 'test:v1'\nnamespace = 'team'\n";
        let mut scoped = deployer_context();
        scoped.scoped_namespaces = Some(vec!["other".into()]);
        let mut system = deployer_context();
        system.token_name = crate::sesame::auth::SYSTEM_PRINCIPAL.into();
        for (auth, header, body, expected) in [
            (
                scoped,
                "acknowledged",
                manifest.to_string(),
                StatusCode::FORBIDDEN,
            ),
            (
                system,
                "acknowledged",
                manifest.to_string(),
                StatusCode::FORBIDDEN,
            ),
            (
                deployer_context(),
                "true",
                manifest.to_string(),
                StatusCode::BAD_REQUEST,
            ),
            (
                deployer_context(),
                "acknowledged",
                format!("{manifest}[app.web]\nimage = 'test:v1'\n"),
                StatusCode::BAD_REQUEST,
            ),
            (
                deployer_context(),
                "acknowledged",
                format!("{manifest}schedule = '* * * * *'\n"),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let mut request = axum::http::Request::post("/v1/apply")
                .header("x-reliaburger-rerun-jobs", header)
                .body(Body::from(body))
                .unwrap();
            request.extensions_mut().insert(auth);
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                expected
            );
            assert!(commands.try_recv().is_err());
            assert!(council.desired_state().await.apps.is_empty());
        }
        let mut request = axum::http::Request::post("/v1/apply")
            .header("x-reliaburger-rerun-jobs", "acknowledged")
            .body(Body::from(manifest))
            .unwrap();
        request.extensions_mut().insert(deployer_context());
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
        assert!(
            matches!(commands.recv().await, Some(AgentCommand::RerunJobs { config, .. }) if config.job.len() == 1)
        );
        assert!(council.desired_state().await.apps.is_empty());
        council.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn job_apply_checks_namespace_and_app_scope_before_enqueuing_work() {
        let (app, council, mut commands) = workload_admission_fixture("job-scope").await;
        for (apps, namespaces) in [
            (None, Some(vec!["allowed".into()])),
            (Some(vec!["allowed".into()]), None),
        ] {
            let mut auth = deployer_context();
            auth.scoped_apps = apps;
            auth.scoped_namespaces = namespaces;
            assert_eq!(
                apply_as_context(
                    &app,
                    auth,
                    "[job.denied]\nimage = \"test:v1\"\nnamespace = \"denied\"\n"
                )
                .await,
                StatusCode::FORBIDDEN
            );
            assert!(matches!(
                commands.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
        let mut auth = deployer_context();
        auth.scoped_apps = Some(vec!["allowed".into()]);
        auth.scoped_namespaces = Some(vec!["allowed".into()]);
        assert_eq!(
            apply_as_context(
                &app,
                auth,
                "[job.allowed]\nimage = \"test:v1\"\nnamespace = \"allowed\"\n"
            )
            .await,
            StatusCode::OK
        );
        assert!(matches!(
            commands.try_recv(),
            Ok(AgentCommand::Deploy { .. })
        ));
        council.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn workload_manifests_cannot_reference_leased_images_without_ownership() {
        let (app, council, mut commands) =
            workload_admission_fixture("leased-image-admission").await;
        for fragment in [
            "[app.bad]\nimage = 'rbtest-run1/web:latest'\n",
            "[app.bad]\nimage = 'ordinary:v1'\n[[app.bad.init]]\nimage = 'registry.example:5050/rbtest-run1/web:latest'\n",
            "[job.bad]\nimage = 'rbtest-run1/web:latest'\n",
        ] {
            let manifest = format!("[app.safe]\nimage = 'ordinary:v1'\n{fragment}");
            assert_eq!(
                apply_as_context(&app, deployer_context(), &manifest).await,
                StatusCode::FORBIDDEN
            );
            assert!(council.desired_state().await.apps.is_empty());
            assert!(matches!(
                commands.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
        council.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn out_of_scope_job_refuses_the_entire_manifest_before_app_commit() {
        let (app, council, mut commands) = workload_admission_fixture("mixed-job-scope").await;
        let mut auth = deployer_context();
        auth.scoped_namespaces = Some(vec!["allowed".into()]);
        let manifest = "[app.allowed]\nimage = \"test:v1\"\nnamespace = \"allowed\"\n[job.denied]\nimage = \"test:v1\"\nnamespace = \"denied\"\n";
        assert_eq!(
            apply_as_context(&app, auth, manifest).await,
            StatusCode::FORBIDDEN
        );
        assert!(council.desired_state().await.apps.is_empty());
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        council.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn workload_apply_checks_deploy_and_host_execution_permission_for_jobs_and_apps() {
        let (app, council, mut commands) = workload_admission_fixture("job-permissions").await;
        for (actions, fragments, expected) in [
            (
                vec!["logs"],
                vec!["image = \"test:v1\""],
                StatusCode::FORBIDDEN,
            ),
            (
                vec!["deploy"],
                vec!["script = \"echo hello\"", "exec = \"/bin/true\""],
                StatusCode::FORBIDDEN,
            ),
            (
                vec!["deploy", "host-exec"],
                vec!["script = \"echo hello\"", "exec = \"/bin/true\""],
                StatusCode::OK,
            ),
        ] {
            council
                .write(crate::council::RaftRequest::PermissionSpec {
                    name: "ci".into(),
                    spec: Box::new(crate::config::PermissionSpec {
                        actions: actions.into_iter().map(str::to_string).collect(),
                        apps: vec!["*".into()],
                        namespaces: None,
                    }),
                })
                .await
                .unwrap();
            for fragment in &fragments {
                // Apps are cluster-scheduled; exercise their refusal paths here.
                // The positive local-job path also proves the grant opens the gate.
                let kinds: &[&str] = if expected == StatusCode::OK {
                    &["job"]
                } else {
                    &["app", "job"]
                };
                for kind in kinds {
                    let manifest = format!("[{kind}.work]\n{fragment}\n");
                    assert_eq!(
                        apply_as_context(&app, deployer_context(), &manifest).await,
                        expected,
                        "{manifest}"
                    );
                    if expected == StatusCode::OK {
                        assert!(matches!(
                            commands.try_recv(),
                            Ok(AgentCommand::Deploy { .. })
                        ));
                    } else {
                        assert!(matches!(
                            commands.try_recv(),
                            Err(mpsc::error::TryRecvError::Empty)
                        ));
                        assert!(council.desired_state().await.apps.is_empty());
                    }
                }
            }
        }
        council.shutdown().await.unwrap();
    }

    /// A completed history entry for `web` in the `default` namespace; tests
    /// override the fields they care about.
    fn sample_history_entry(image: &str) -> DeployHistoryEntry {
        use crate::meat::types::AppId;
        DeployHistoryEntry {
            id: crate::meat::deploy_types::DeployId(1),
            app_id: AppId {
                name: "web".to_string(),
                namespace: "default".to_string(),
            },
            image: image.to_string(),
            result: crate::meat::deploy_types::DeployResult::Completed,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            completed_at: std::time::SystemTime::UNIX_EPOCH,
            steps_completed: 1,
            steps_total: 1,
            spec: None,
        }
    }

    /// Router with a seeded deploy history and no token store, so the
    /// namespace filter can be checked without an auth layer in the way.
    async fn setup_with_deploy_history(
        history: Arc<RwLock<Vec<DeployHistoryEntry>>>,
    ) -> (Router, CancellationToken) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());
        tokio::spawn(async move {
            agent.run().await;
        });
        let app = router(
            cmd_tx,
            None,
            None,
            Some(history),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            9117,
            None,
        );
        (app, shutdown)
    }

    /// GET a URI and return the response body as a string.
    async fn get_body(app: Router, uri: &str) -> String {
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// Router whose `ApiState` reports the given static capabilities, so the
    /// endpoint can be driven without a live cluster.
    async fn setup_with_capabilities(
        statics: crate::bun::capabilities::StaticCapabilities,
        mayo: bool,
    ) -> (Router, CancellationToken, tempfile::TempDir) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());
        tokio::spawn(async move {
            agent.run().await;
        });
        // A real store, because the point is that `Some(..)` on `ApiState`
        // is what the endpoint reads — not a flag we could fake.
        let mayo_dir = tempfile::tempdir().unwrap();
        let mayo_store = if mayo {
            Some(Arc::new(RwLock::new(MayoStore::new(
                mayo_dir.path().to_path_buf(),
            ))))
        } else {
            None
        };
        let app = router_with_upgrade(
            cmd_tx,
            mayo_store,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            9117,
            None,
            None,
            None,
            "default".to_string(),
            None,
            900,
            crate::cluster::ClusterHttp::plaintext(),
            5050,
            "http",
            256 * 1024 * 1024,
            false,
            statics,
            crate::bun::readiness::ReadinessTracker::new(),
            None,
            None,
        );
        (app, shutdown, mayo_dir)
    }

    /// The endpoint must read the *live* `ApiState`, not a snapshot taken at
    /// construction: a subsystem that was never built shows as `false`, and
    /// that is what tells a caller "skipped" rather than "broken".
    #[tokio::test]
    async fn capabilities_reports_wired_subsystems() {
        let (app, shutdown, _mayo_dir) = setup_with_capabilities(
            crate::bun::capabilities::StaticCapabilities {
                container_runtime: "process".to_string(),
                ..Default::default()
            },
            true,
        )
        .await;
        let body = get_body(app.clone(), "/v1/capabilities").await;
        let capabilities: crate::bun::capabilities::ClusterCapabilities =
            serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));

        assert!(capabilities.metrics, "mayo was wired: {body}");
        assert!(!capabilities.council, "no council was wired: {body}");
        assert!(!capabilities.registry);
        assert_eq!(capabilities.container_runtime, "process");
        assert!(!capabilities.version.is_empty());
        assert_eq!(
            capabilities.schema_version,
            crate::bun::capabilities::CAPABILITY_SCHEMA_VERSION
        );
        assert!(capabilities.readiness.is_some());
        assert!(capabilities.placement.is_some());
        assert!(capabilities.expires_at_unix_ms > capabilities.observed_at_unix_ms);

        let body = get_body(app, "/v1/capabilities/cluster").await;
        let cluster: crate::bun::capabilities::ClusterCapabilityReport =
            serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
        assert_eq!(cluster.nodes.len(), 1);
        assert!(matches!(
            cluster.nodes[0],
            crate::bun::capabilities::CollectedNodeCapability::Evidence { .. }
        ));
        shutdown.cancel();
    }

    #[tokio::test]
    async fn capabilities_reports_the_environment_tag() {
        let (app, shutdown, _mayo_dir) = setup_with_capabilities(
            crate::bun::capabilities::StaticCapabilities {
                environment: Some("production".to_string()),
                container_runtime: "runc".to_string(),
                ..Default::default()
            },
            false,
        )
        .await;
        let body = get_body(app, "/v1/capabilities").await;
        let capabilities: crate::bun::capabilities::ClusterCapabilities =
            serde_json::from_str(&body).unwrap();
        assert_eq!(capabilities.environment.as_deref(), Some("production"));
        assert!(capabilities.is_production());
        shutdown.cancel();
    }

    #[tokio::test]
    async fn capabilities_default_to_no_environment() {
        let (app, shutdown, _mayo_dir) = setup_with_capabilities(
            crate::bun::capabilities::StaticCapabilities::default(),
            false,
        )
        .await;
        let body = get_body(app, "/v1/capabilities").await;
        let capabilities: crate::bun::capabilities::ClusterCapabilities =
            serde_json::from_str(&body).unwrap();
        assert_eq!(capabilities.environment, None);
        assert!(!capabilities.is_production());
        // A standalone node is a cluster of one, and knows it isn't clustered.
        assert!(!capabilities.cluster);
        assert_eq!(capabilities.node_count, 1);
        shutdown.cancel();
    }

    /// Every route that addresses one app used to check the caller's *role*
    /// and stop there, so a legitimately issued token scoped to one namespace
    /// could read every other tenant's logs, env, status and metrics (C3).
    /// Reads are the interesting half: `stop`/`exec`/`rollback` were scoped
    /// from the start, which is what made the gap easy to miss.
    #[tokio::test]
    async fn scoped_token_is_refused_reading_another_namespace() {
        let (app, shutdown, tok) = setup_scoped_to_namespace("team-a").await;
        for uri in [
            "/v1/status/web/team-b",
            "/v1/logs/web/team-b",
            "/v1/logs/entries/web/team-b",
            "/v1/logs/query/web/team-b",
            "/v1/metrics/app/web/team-b",
            "/v1/snapshots/team-b/web",
            "/v1/deploys/history/web?namespace=team-b",
            "/ui/app/web/team-b",
            "/ui/app/web/team-b/env",
            "/ui/fragment/app/web/team-b/instances",
        ] {
            assert_eq!(
                get_status(app.clone(), uri, Some(&tok)).await,
                StatusCode::FORBIDDEN,
                "{uri} served a namespace the token has no scope for"
            );
        }
        shutdown.cancel();
    }

    /// The mirror image: the same routes must not start refusing work the
    /// token *is* scoped for. (A missing app 404s; what matters is never 403.)
    #[tokio::test]
    async fn scoped_token_still_reads_inside_its_namespace() {
        let (app, shutdown, tok) = setup_scoped_to_namespace("team-a").await;
        for uri in [
            "/v1/status/web/team-a",
            "/v1/logs/web/team-a",
            "/v1/logs/entries/web/team-a",
            "/v1/metrics/app/web/team-a",
            "/v1/deploys/history/web?namespace=team-a",
            "/ui/app/web/team-a",
            "/ui/app/web/team-a/env",
            "/ui/fragment/app/web/team-a/instances",
        ] {
            assert_ne!(
                get_status(app.clone(), uri, Some(&tok)).await,
                StatusCode::FORBIDDEN,
                "{uri} refused an in-scope read"
            );
        }
        shutdown.cancel();
    }

    /// The WebSocket log stream needs a real server: `WebSocketUpgrade` is an
    /// extractor and rejects a hand-built request with 426 before the handler
    /// body runs, so `oneshot` can never reach the scope check. Bind an
    /// ephemeral port, do an actual handshake, and the upgrade must be refused.
    #[tokio::test]
    async fn scoped_token_is_refused_streaming_another_namespaces_logs() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let (app, shutdown, tok) = setup_scoped_to_namespace("team-a").await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { serving.cancelled().await })
                .await
                .unwrap();
        });

        let mut request = format!("ws://{address}/v1/ws/logs/web/team-b")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {tok}").parse().unwrap());
        let error = tokio_tungstenite::connect_async(request)
            .await
            .expect_err("the log stream upgraded for an out-of-scope namespace");
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), StatusCode::FORBIDDEN);
            }
            other => panic!("expected an HTTP refusal, got {other:?}"),
        }

        shutdown.cancel();
        let _ = server.await;
    }

    /// `/v1/logs/sql` takes no app or namespace to check a scope against, and
    /// arbitrary SQL can't be rewritten into a tenant-filtered query. A scoped
    /// token is refused outright rather than served every tenant's logs (C3).
    #[tokio::test]
    async fn scoped_token_is_refused_raw_log_sql() {
        let (app, shutdown, tok) = setup_scoped_to_namespace("team-a").await;
        let status = get_status(app, "/v1/logs/sql?q=SELECT%20*%20FROM%20logs", Some(&tok)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    /// …while an unscoped token keeps the operator query it has always had.
    #[tokio::test]
    async fn unscoped_token_still_runs_raw_log_sql() {
        let (app, shutdown, tok) =
            setup_with_role("sql-unscoped", crate::sesame::types::ApiRole::Admin).await;
        let status = get_status(app, "/v1/logs/sql?q=SELECT%20*%20FROM%20logs", Some(&tok)).await;
        assert_ne!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    /// `/v1/logs/export` writes files with the agent's credentials wherever
    /// the destination points — Deployer must not reach it.
    #[tokio::test]
    async fn logs_export_requires_admin() {
        let (app, shutdown, tok) = setup_with_role(
            "logs-export-deployer",
            crate::sesame::types::ApiRole::Deployer,
        )
        .await;
        let status = post_status(
            app,
            "/v1/logs/export",
            &tok,
            r#"{"destination":"/tmp/nowhere"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    /// An Admin token on a node with no log store gets an honest 503, not a
    /// silent empty success.
    #[tokio::test]
    async fn logs_export_without_a_store_is_service_unavailable() {
        let (app, shutdown, tok) =
            setup_with_role("logs-export-nostore", crate::sesame::types::ApiRole::Admin).await;
        let status = post_status(
            app,
            "/v1/logs/export",
            &tok,
            r#"{"destination":"/tmp/nowhere"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        shutdown.cancel();
    }

    /// The export endpoint ships the store's Parquet files to the requested
    /// destination under the node's name and persists the Bun-owned export
    /// checkpoint (X8), so a repeat export ships nothing new.
    #[tokio::test]
    async fn logs_export_ships_parquet_and_persists_the_checkpoint() {
        let (cmd_tx, _cmd_rx) = mpsc::channel(32);
        let store_dir = tempfile::tempdir().unwrap();
        let mut store = crate::ketchup::log_store::LogStore::new(store_dir.path().to_path_buf());
        store.append(
            "web",
            "default",
            crate::ketchup::types::LogStream::Stdout,
            "hello",
        );
        store.flush().await.unwrap();
        let log_store = Some(Arc::new(RwLock::new(store)));

        let app = router(
            cmd_tx, None, log_store, None, None, None, None, None, None, None, None, None, 9117,
            None,
        );
        let destination = tempfile::tempdir().unwrap();
        let body = serde_json::json!({ "destination": destination.path() }).to_string();

        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/logs/export")
                    .header("content-type", "application/json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let outcome: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(outcome["files_exported"], 1, "{outcome}");
        assert_eq!(outcome["checkpoint_saved"], true, "{outcome}");

        // The file landed under the node's subdirectory…
        let node_dir = destination.path().join("local");
        let shipped: Vec<_> = std::fs::read_dir(&node_dir)
            .expect("node subdirectory must exist")
            .flatten()
            .collect();
        assert_eq!(shipped.len(), 1, "one parquet file must be shipped");
        // …and the checkpoint lives with the store, so a second export is a
        // no-op instead of a double-ship.
        assert!(
            store_dir
                .path()
                .join(crate::ketchup::export::CHECKPOINT_FILENAME)
                .exists()
        );
        let repeat = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/logs/export")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(repeat.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let outcome: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(outcome["files_exported"], 0, "{outcome}");
    }

    /// AUTH1 for faults: a Deployer scoped to one namespace clears the role
    /// gate but is refused injecting a fault into another tenant's same-named
    /// service, before the safety policy is even consulted. `/v1/fault` carries
    /// no `{app}` path segment, so the route-matrix scope test can't cover it.
    #[tokio::test]
    async fn scoped_token_is_refused_faulting_another_namespace() {
        let (app, shutdown, tok) = setup_scoped_to_namespace("team-a").await;
        let body = serde_json::json!({
            "fault_type": { "type": "Pause" },
            "target_service": "web",
            "namespace": "team-b",
            "duration": { "secs": 1, "nanos": 0 },
            "injected_by": "",
        })
        .to_string();
        let (status, body) = post_authenticated(app, "/v1/fault", &tok, &body, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("scope"),
            "expected a scope refusal, got: {text}"
        );
        shutdown.cancel();
    }

    /// The cluster-wide metrics and events endpoints read across every app and
    /// namespace, so — like `/v1/logs/sql` — a scoped token is refused (C3)
    /// rather than served every tenant's data.
    #[tokio::test]
    async fn scoped_token_is_refused_cluster_wide_metrics_and_events() {
        for path in [
            "/v1/metrics?name=node_cpu_usage_percent",
            "/v1/metrics/summary",
            "/v1/metrics/keys",
            "/v1/metrics/rollup",
            "/v1/metrics/rollup/owned",
            "/v1/metrics/cluster",
            "/v1/events",
        ] {
            let (app, shutdown, tok) = setup_scoped_to_namespace("team-a").await;
            let status = get_status(app, path, Some(&tok)).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "scoped token allowed on {path}"
            );
            shutdown.cancel();
        }
    }

    /// …while an unscoped token still reaches them.
    #[tokio::test]
    async fn unscoped_token_still_reads_cluster_wide_metrics_and_events() {
        for path in ["/v1/metrics/keys", "/v1/events"] {
            let (app, shutdown, tok) =
                setup_with_role("metrics-unscoped", crate::sesame::types::ApiRole::ReadOnly).await;
            let status = get_status(app, path, Some(&tok)).await;
            assert_ne!(
                status,
                StatusCode::FORBIDDEN,
                "unscoped token refused on {path}"
            );
            shutdown.cancel();
        }
    }

    /// Two apps of the same name in different namespaces have coexisted since
    /// DEP1; the history endpoint filtered on the bare name, so it returned
    /// both tenants' deploys to whoever asked.
    #[tokio::test]
    async fn deploy_history_is_filtered_by_namespace() {
        use crate::meat::types::AppId;

        let entry = |namespace: &str, image: &str| DeployHistoryEntry {
            app_id: AppId {
                name: "web".to_string(),
                namespace: namespace.to_string(),
            },
            ..sample_history_entry(image)
        };
        let history = Arc::new(RwLock::new(vec![
            entry("team-a", "a:1"),
            entry("team-b", "b:1"),
        ]));
        let (app, shutdown) = setup_with_deploy_history(history).await;

        let body = get_body(app, "/v1/deploys/history/web?namespace=team-a").await;
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let entries = parsed["history"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "history leaked across namespaces: {body}");
        assert_eq!(entries[0]["image"], "a:1");
        shutdown.cancel();
    }

    #[tokio::test]
    async fn readonly_token_is_refused_mutating_a_snapshot() {
        // AUTH2: snapshot mutation is no longer open to any authenticated
        // caller. A ReadOnly token is refused.
        let (app, shutdown, tok) =
            setup_with_role("auth2-snap", crate::sesame::types::ApiRole::ReadOnly).await;
        let status = post_status(app, "/v1/snapshots/default/web", &tok, "{}").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn readonly_token_is_refused_rolling_back() {
        // AUTH2: app rollback now requires a Deployer.
        let (app, shutdown, tok) =
            setup_with_role("auth2-rollback", crate::sesame::types::ApiRole::ReadOnly).await;
        let status = post_status(app, "/v1/rollback/web/default", &tok, "").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn dashboard_nodes_fragment_reflects_real_membership() {
        // AUTH7: the nodes fragment lists the live gossip members by name,
        // not a hardcoded empty list.
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());
        tokio::spawn(async move {
            agent.run().await;
        });
        let membership = Arc::new(RwLock::new(vec![
            NodeMembershipInfo {
                node_id: crate::meat::NodeId::new("node-alpha"),
                address: "127.0.0.1:9101".parse().unwrap(),
                api_advertised: true,
            },
            NodeMembershipInfo {
                node_id: crate::meat::NodeId::new("node-beta"),
                address: "127.0.0.1:9102".parse().unwrap(),
                api_advertised: true,
            },
        ]));
        // No token store, so the request is open; membership at position 11.
        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(membership),
            None,
            9117,
            None,
        );
        let (status, body) = get(app, "/ui/fragment/nodes").await;
        assert_eq!(status, StatusCode::OK);
        let html = String::from_utf8(body).unwrap();
        assert!(html.contains("node-alpha"), "html was: {html}");
        assert!(html.contains("node-beta"), "html was: {html}");
        shutdown.cancel();
    }

    /// Build a router around `council` whose store holds a user token plus a
    /// known service token, so AUTH4's system-principal restriction can be
    /// exercised end-to-end.
    async fn setup_with_service_token(
        tag: &str,
    ) -> (Router, CancellationToken, Arc<crate::council::CouncilNode>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());
        tokio::spawn(async move {
            agent.run().await;
        });
        let council = seeded_council(tag).await;
        // A user token exists so enforcement is on.
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
            Some(council.clone()),
            Some(store),
            Some("rbrg_service".to_string()),
            None,
            None,
            None,
            9117,
            None,
        );
        (app, shutdown, council)
    }

    #[tokio::test]
    async fn service_principal_is_refused_from_creating_tokens() {
        // AUTH4: a stolen service token must not mint user tokens.
        let (app, shutdown, _council) = setup_with_service_token("role-system-tok").await;
        let status = post_status(
            app,
            "/v1/token/create",
            "rbrg_service",
            &serde_json::json!({ "name": "x", "role": "deployer" }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn service_principal_is_refused_from_rotating_secrets() {
        // AUTH4: rotating the cluster's key material is not fan-out work.
        let (app, shutdown, _council) = setup_with_service_token("role-system-rot").await;
        let status = post_status(app, "/v1/secret/rotate", "rbrg_service", "{}").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn service_principal_is_accepted_on_a_system_route() {
        // AUTH4 must not weaken node-to-node fan-out: the service token still
        // clears a System-tagged route (here batch/run). It passes the auth
        // gate; the run itself may then fail on missing state, but not with a
        // 403 (the authorisation refusal).
        let (app, shutdown, _council) = setup_with_service_token("role-system-run").await;
        let status = post_status(
            app,
            "/v1/batch/run",
            "rbrg_service",
            &serde_json::json!({}).to_string(),
        )
        .await;
        assert_ne!(status, StatusCode::FORBIDDEN);
        shutdown.cancel();
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
            None,
            None,
            None,
            9117,
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
            None,
            None,
            None,
            9117,
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
    async fn join_token_create_returns_two_distinct_plaintexts_and_stores_only_hashes() {
        let (cmd_tx, _cmd_rx) = mpsc::channel(32);
        let council = seeded_council("join-token-create").await;
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
            None,
            None,
            None,
            9117,
            None,
        );

        let mut plaintexts = Vec::new();
        for index in 0..2 {
            let node_id = format!("node-{:02}", index + 2);
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/v1/join-token/create")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({ "ttl_seconds": 900, "node_id": node_id })
                                .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            plaintexts.push(json["token"].as_str().unwrap().to_string());
        }
        assert_ne!(plaintexts[0], plaintexts[1]);

        let stored = council.security_state().await.join_tokens;
        assert_eq!(stored.len(), 2, "init mints none; two new tokens");
        for plaintext in &plaintexts {
            assert!(
                stored
                    .iter()
                    .any(|token| crate::sesame::ca::verify_join_token(
                        plaintext,
                        &token.token_hash
                    )),
                "returned token must match one committed hash"
            );
        }
        let serialised = serde_json::to_string(&stored).unwrap();
        assert!(plaintexts.iter().all(|token| !serialised.contains(token)));
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
            None,
            None,
            None,
            9117,
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
            .clone()
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

        let operation_id = match events.first().expect("no SSE events in response") {
            super::ApplyEvent::Accepted { operation_id } => operation_id.clone(),
            other => panic!("expected Accepted event first, got {other:?}"),
        };

        // Should end with a Complete event
        let last = events.last().expect("no SSE events in response");
        match last {
            super::ApplyEvent::Complete { created, .. } => assert_eq!(*created, 1),
            other => panic!("expected Complete event, got {other:?}"),
        }

        let (status, body) = get(app.clone(), "/v1/deploys/active").await;
        assert_eq!(status, StatusCode::OK);
        let active: crate::bun::deploy_operations::ActiveDeployOperations =
            serde_json::from_slice(&body).unwrap();
        assert!(active.active_deploys.is_empty());

        let (status, body) = get(app, "/v1/deploys/operations").await;
        assert_eq!(status, StatusCode::OK);
        let operations: crate::bun::deploy_operations::DeployOperationSnapshot =
            serde_json::from_slice(&body).unwrap();
        assert_eq!(operations.history.len(), 1);
        assert_eq!(operations.history[0].id.as_str(), operation_id);
        assert_eq!(
            operations.history[0].outcome,
            Some(crate::bun::deploy_operations::DeployOperationOutcome::Completed)
        );

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
    async fn cluster_status_refuses_to_report_success_when_a_member_is_unreachable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        // Keep the port reserved but never serve HTTP: the entire response
        // (including its body) must have a deadline.
        let (cmd_tx, mut cmd_rx) = mpsc::channel(2);
        let worker = tokio::spawn(async move {
            if let Some(AgentCommand::Status { response }) = cmd_rx.recv().await {
                let _ = response.send(Vec::new());
            }
        });
        let members = Arc::new(RwLock::new(vec![NodeMembershipInfo {
            node_id: crate::meat::NodeId::new("unresponsive"),
            address,
            api_advertised: true,
        }]));
        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(members),
            None,
            9117,
            None,
        );
        // The per-member deadline is a Tokio timer, so paused time reaches
        // it as soon as the request is idle on the silent socket instead of
        // waiting five real seconds. The 7 s guard still fires later, so a
        // missing deadline fails rather than passes.
        tokio::time::pause();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(7),
            app.oneshot(
                axum::http::Request::builder()
                    .uri("/v1/status?cluster=true")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("status must be bounded")
        .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains("unresponsive"));
        worker.await.unwrap();
        drop(listener);
    }

    /// The cluster fan-out authenticates to peers with the node's own service
    /// token, which sees everything. What comes back must still be trimmed
    /// to the *caller's* scope, locally and cluster-wide (T1.9).
    #[tokio::test]
    async fn namespace_scoped_token_sees_only_its_namespace_in_status() {
        let status = |id: &str, namespace: &str| -> InstanceStatus {
            serde_json::from_value(serde_json::json!({
                "id": id, "app_name": "web", "namespace": namespace, "state": "running",
                "restart_count": 0, "host_port": null, "pid": null
            }))
            .unwrap()
        };
        let peer_statuses = vec![status("peer-a", "team-a"), status("peer-b", "team-b")];
        let peer = Router::new().route(
            "/v1/status",
            axum::routing::get(move || {
                let statuses = peer_statuses.clone();
                async move { Json(statuses) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_address = listener.local_addr().unwrap();
        let peer_server = tokio::spawn(async move {
            axum::serve(listener, peer).await.unwrap();
        });

        let (cmd_tx, mut cmd_rx) = mpsc::channel(4);
        let local_statuses = vec![status("local-a", "team-a"), status("local-b", "team-b")];
        let worker = tokio::spawn(async move {
            while let Some(command) = cmd_rx.recv().await {
                if let AgentCommand::Status { response } = command {
                    let _ = response.send(local_statuses.clone());
                }
            }
        });
        let created = crate::sesame::token::create_token(
            "tenant-a-reader",
            crate::sesame::types::ApiRole::ReadOnly,
            crate::sesame::types::TokenScope {
                apps: None,
                namespaces: Some(vec!["team-a".to_string()]),
            },
            None,
        )
        .unwrap();
        let store = crate::sesame::auth::new_token_store();
        store.write().await.push(created.token);
        let members = Arc::new(RwLock::new(vec![NodeMembershipInfo {
            node_id: crate::meat::NodeId::new("peer"),
            address: peer_address,
            api_advertised: true,
        }]));
        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(store),
            None,
            None,
            Some(members),
            None,
            9117,
            None,
        );

        let (code, body) = get_authenticated(app.clone(), "/v1/status", &created.plaintext).await;
        assert_eq!(code, StatusCode::OK);
        let local: Vec<InstanceStatus> = serde_json::from_slice(&body).unwrap();
        let local_ids: Vec<_> = local.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(local_ids, ["local-a"]);

        let (code, body) =
            get_authenticated(app, "/v1/status?cluster=true", &created.plaintext).await;
        assert_eq!(code, StatusCode::OK);
        let cluster: Vec<crate::bun::agent::ClusterInstanceStatus> =
            serde_json::from_slice(&body).unwrap();
        let mut cluster_ids: Vec<_> = cluster.iter().map(|s| s.instance.id.as_str()).collect();
        cluster_ids.sort_unstable();
        assert_eq!(cluster_ids, ["local-a", "peer-a"]);

        peer_server.abort();
        worker.abort();
    }

    /// The registry holds a scoped token to `<namespace>/<app>` repositories
    /// in its scope; the image list follows the same rule.
    #[tokio::test]
    async fn namespace_scoped_token_lists_only_its_namespace_images() {
        use crate::pickle::types::{Digest, ImageManifest, LayerDescriptor};
        let mut catalog = ManifestCatalog::default();
        for (index, repository) in ["team-a/web", "team-b/web", "web"].iter().enumerate() {
            let digest = Digest::from_sha256_hex(&format!("{index:064x}"));
            catalog.manifests.push((
                digest.as_str().to_string(),
                ImageManifest {
                    digest: digest.clone(),
                    config: LayerDescriptor {
                        digest,
                        size: 2,
                        media_type: "application/vnd.oci.image.config.v1+json".into(),
                    },
                    layers: Vec::new(),
                    repository: repository.to_string(),
                    tags: ["v1".to_string()].into(),
                    total_size: 2,
                    pushed_at: std::time::SystemTime::UNIX_EPOCH,
                    pushed_by: 1,
                    signature: None,
                },
            ));
        }
        let scoped = crate::sesame::token::create_token(
            "team-a-puller",
            crate::sesame::types::ApiRole::ReadOnly,
            crate::sesame::types::TokenScope {
                apps: None,
                namespaces: Some(vec!["team-a".into()]),
            },
            None,
        )
        .unwrap();
        let unscoped = crate::sesame::token::create_token(
            "puller",
            crate::sesame::types::ApiRole::ReadOnly,
            crate::sesame::types::TokenScope::default(),
            None,
        )
        .unwrap();
        let store = crate::sesame::auth::new_token_store();
        store.write().await.push(scoped.token);
        store.write().await.push(unscoped.token);
        let (cmd_tx, _cmd_rx) = mpsc::channel(4);
        let app = router(
            cmd_tx,
            None,
            None,
            None,
            Some(Arc::new(RwLock::new(catalog))),
            None,
            None,
            Some(store),
            None,
            None,
            None,
            None,
            9117,
            None,
        );

        let repositories = |body: Vec<u8>| -> Vec<String> {
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let mut names: Vec<String> = json["images"]
                .as_array()
                .unwrap()
                .iter()
                .map(|image| image["repository"].as_str().unwrap().to_string())
                .collect();
            names.sort_unstable();
            names
        };
        let (code, body) = get_authenticated(app.clone(), "/v1/images", &scoped.plaintext).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(repositories(body), ["team-a/web"]);
        let (code, body) = get_authenticated(app, "/v1/images", &unscoped.plaintext).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(repositories(body), ["team-a/web", "team-b/web", "web"]);
    }

    #[test]
    fn dashboard_shows_desired_replicas_and_counts_only_running_instances() {
        let mut running: InstanceStatus = serde_json::from_value(serde_json::json!({
            "id":"web-0", "app_name":"web", "namespace":"default", "state":"running",
            "restart_count":0,"host_port":null,"pid":null
        }))
        .unwrap();
        let mut failed = running.clone();
        failed.id = "web-1".into();
        failed.state = "failed".into();
        let desired = vec![
            crate::bun::diagnostics::DesiredAppEvidence {
                app: "web".into(),
                namespace: "default".into(),
                desired_replicas: 3,
                scheduled_replicas: 2,
                placements: Default::default(),
                service_port: None,
            },
            crate::bun::diagnostics::DesiredAppEvidence {
                app: "pending".into(),
                namespace: "default".into(),
                desired_replicas: 2,
                scheduled_replicas: 0,
                placements: Default::default(),
                service_port: None,
            },
        ];
        let rows = statuses_to_dashboard_apps(&[running.clone(), failed], &desired);
        let web = rows.iter().find(|row| row.name == "web").unwrap();
        assert_eq!((web.instances_running, web.instances_desired), (1, 3));
        assert_ne!(web.state, "running");
        let pending = rows.iter().find(|row| row.name == "pending").unwrap();
        assert_eq!(
            (pending.instances_running, pending.instances_desired),
            (0, 2)
        );
        running.state = "stopped".into();
        let rows = statuses_to_dashboard_apps(&[running], &desired);
        assert_eq!(
            rows.iter()
                .find(|row| row.name == "web")
                .unwrap()
                .instances_running,
            0
        );
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
            None,
            None,
            None,
            9117,
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
    async fn nodes_endpoint_advertises_only_resolved_peer_api_addresses() {
        let (tx, mut rx) = mpsc::channel(1);
        let worker = tokio::spawn(async move {
            let Some(AgentCommand::Nodes { response }) = rx.recv().await else {
                panic!("expected membership request");
            };
            response
                .send(
                    ["one", "guessed", "unknown"]
                        .into_iter()
                        .map(|id| super::super::agent::NodeStatus {
                            node_id: id.to_string(),
                            address: "127.0.0.1:7946".to_string(),
                            api_address: None,
                            state: "alive".to_string(),
                            incarnation: 1,
                            is_council: true,
                            is_leader: false,
                            labels: Default::default(),
                        })
                        .collect(),
                )
                .unwrap();
        });
        let membership = Arc::new(RwLock::new(vec![
            NodeMembershipInfo {
                node_id: crate::meat::NodeId::new("one"),
                address: "[::1]:19117".parse().unwrap(),
                api_advertised: true,
            },
            // Known to gossip, but its own directory extension hasn't
            // arrived: the address is only a port-offset guess.
            NodeMembershipInfo {
                node_id: crate::meat::NodeId::new("guessed"),
                address: "[::1]:19999".parse().unwrap(),
                api_advertised: false,
            },
        ]));
        let app = router(
            tx,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(membership),
            None,
            9117,
            None,
        );
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
        let nodes: Vec<crate::bun::agent::NodeStatus> = serde_json::from_slice(&body).unwrap();
        assert_eq!(nodes[0].api_address, Some("[::1]:19117".parse().unwrap()));
        // A guess is not evidence: clients building an upgrade plan would
        // hand it back as the node's address and be refused.
        assert_eq!(nodes[1].api_address, None);
        assert_eq!(nodes[2].api_address, None);
        worker.await.unwrap();
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
                    .body(Body::from(
                        serde_json::json!({"compatibility": crate::compatibility::CURRENT, "token": "abc123", "node_id": "node-02", "csr_b64": ""}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Without a council, join validation fails with a 400
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn incompatible_join_is_refused_before_csr_or_token_validation() {
        let (app, shutdown) = test_setup();
        let response = app.oneshot(axum::http::Request::builder()
            .method("POST").uri("/v1/cluster/join")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"compatibility":{"protocol":1,"state":1},"token":"unused","node_id":"old","csr_b64":"invalid!"}"#)).unwrap())
            .await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn join_route_is_reachable_without_a_bearer_token() {
        // The join route is public: a joiner has no bearer token yet, only a
        // join token in the body. It must not 401 — it reaches the handler
        // (and here fails validation at 400 because there is no council).
        let (app, shutdown) = test_setup();

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/cluster/join")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"compatibility": crate::compatibility::CURRENT, "token": "whatever", "node_id": "node-09", "csr_b64": ""}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        shutdown.cancel();
    }

    // --- UI auth (sessions + route lockdown) ---

    /// A router whose token store holds one user token of the given role.
    /// Returns the router, its shutdown handle, and the plaintext token.
    fn ui_setup(role: crate::sesame::types::ApiRole) -> (Router, CancellationToken, String) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(31000, 32000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());
        tokio::spawn(async move {
            agent.run().await;
        });

        let created = crate::sesame::token::create_token(
            "dash",
            role,
            crate::sesame::types::TokenScope::default(),
            None,
        )
        .unwrap();
        let plaintext = created.plaintext.clone();
        let store = crate::sesame::auth::new_token_store();
        // `store` starts non-empty so the bootstrap-open window is closed.
        store.try_write().unwrap().push(created.token);

        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(store),
            None,
            None,
            None,
            None,
            9117,
            None,
        );
        (app, shutdown, plaintext)
    }

    async fn ui_get(app: &Router, uri: &str, headers: &[(&str, &str)]) -> Response {
        let mut req = axum::http::Request::builder().method("GET").uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        app.clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    /// POST a token to /ui/session and return the `rb_session` id from the
    /// Set-Cookie header.
    async fn login(app: &Router, token: &str) -> String {
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/ui/session")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!("token={token}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SEE_OTHER,
            "login should redirect"
        );
        let cookie = resp
            .headers()
            .get("set-cookie")
            .expect("session cookie set")
            .to_str()
            .unwrap();
        assert!(
            cookie.contains("HttpOnly"),
            "cookie must be HttpOnly: {cookie}"
        );
        assert!(
            cookie.contains("SameSite=Strict"),
            "cookie must be SameSite=Strict"
        );
        crate::sesame::session::session_id_from_cookie_header(cookie)
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn dashboard_requires_auth_once_user_tokens_exist() {
        let (app, shutdown, _t) = ui_setup(crate::sesame::types::ApiRole::ReadOnly);
        // A browser navigation with no cookie is redirected to the login page.
        let resp = ui_get(&app, "/", &[("accept", "text/html")]).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers().get("location").unwrap(), "/ui/login");
        shutdown.cancel();
    }

    #[tokio::test]
    async fn dashboard_stays_open_during_the_bootstrap_window() {
        // test_setup has an empty token store → bootstrap-open.
        let (app, shutdown) = test_setup();
        let resp = ui_get(&app, "/", &[("accept", "text/html")]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn a_session_cookie_grants_read_only_access_to_fragments() {
        let (app, shutdown, token) = ui_setup(crate::sesame::types::ApiRole::ReadOnly);
        let id = login(&app, &token).await;
        let cookie = format!("rb_session={id}");
        let resp = ui_get(&app, "/ui/fragment/apps", &[("cookie", &cookie)]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn a_session_cookie_never_grants_write_access_even_for_an_admin_token() {
        // Even an Admin token, exchanged for a session, only reads: the session
        // context is always ReadOnly, so a write endpoint is forbidden.
        let (app, shutdown, token) = ui_setup(crate::sesame::types::ApiRole::Admin);
        let id = login(&app, &token).await;
        let cookie = format!("rb_session={id}");
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/apply")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn an_invalid_token_is_refused_at_the_session_route() {
        let (app, shutdown, _t) = ui_setup(crate::sesame::types::ApiRole::ReadOnly);
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/ui/session")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("token=rbrg_nope"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn static_assets_and_health_stay_public() {
        let (app, shutdown, _t) = ui_setup(crate::sesame::types::ApiRole::ReadOnly);
        // Health needs no cookie even with tokens configured.
        let health = ui_get(&app, "/v1/health", &[]).await;
        assert_eq!(health.status(), StatusCode::OK);
        // The login page itself must be reachable while logged out.
        let login_page = ui_get(&app, "/ui/login", &[("accept", "text/html")]).await;
        assert_eq!(login_page.status(), StatusCode::OK);
        shutdown.cancel();
    }

    // --- GitOps webhook (GIT3) ---------------------------------------------

    /// A router whose GitOps webhook route is wired to a validator with the
    /// given secret. Returns the app plus the receiver, so a test observes a
    /// triggered sync by a real message on the channel rather than a sleep.
    fn webhook_setup(
        secret: &str,
        rate_limit: u32,
    ) -> (Router, mpsc::Receiver<()>, CancellationToken) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());
        tokio::spawn(async move {
            agent.run().await;
        });

        let (webhook_tx, webhook_rx) = mpsc::channel::<()>(4);
        let validator = Arc::new(tokio::sync::Mutex::new(
            crate::lettuce::webhook::WebhookValidator::new(secret, rate_limit),
        ));
        let app = router_with_upgrade(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(webhook_tx),
            Some(validator),
            9117,
            None,
            None,
            None,
            "default".to_string(),
            None,
            900,
            crate::cluster::ClusterHttp::plaintext(),
            5050,
            "http",
            256 * 1024 * 1024,
            false,
            crate::bun::capabilities::StaticCapabilities::default(),
            crate::bun::readiness::ReadinessTracker::new(),
            None,
            None,
        );
        (app, webhook_rx, shutdown)
    }

    fn github_signature(secret: &str, body: &[u8]) -> String {
        use ring::hmac;
        let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
        let tag = hmac::sign(&key, body);
        format!("sha256={}", hex::encode(tag.as_ref()))
    }

    async fn post_webhook(app: &Router, body: &[u8], headers: &[(&str, String)]) -> StatusCode {
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/gitops/webhook");
        for (name, value) in headers {
            req = req.header(*name, value);
        }
        app.clone()
            .oneshot(req.body(Body::from(body.to_vec())).unwrap())
            .await
            .unwrap()
            .status()
    }

    async fn post_authenticated(
        app: Router,
        uri: &str,
        bearer: &str,
        body: &str,
        lease_id: Option<&str>,
    ) -> (StatusCode, Vec<u8>) {
        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", format!("Bearer {bearer}"));
        if uri != "/v1/apply" {
            request = request.header("content-type", "application/json");
        }
        if let Some(lease_id) = lease_id {
            request = request.header("x-reliaburger-test-lease", lease_id);
        }
        let response = app
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, body.to_vec())
    }

    async fn post_capacity_apply(
        app: Router,
        bearer: &str,
        lease_id: &str,
        namespace: &str,
    ) -> (StatusCode, Vec<u8>) {
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/apply")
                    .header("authorization", format!("Bearer {bearer}"))
                    .header("x-reliaburger-test-lease", lease_id)
                    .header("x-reliaburger-capacity-probe", "acknowledged")
                    .body(Body::from(format!(
                        "[app.capacity]\nimage = \"test:v1\"\nnamespace = \"{namespace}\"\n"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, body.to_vec())
    }

    async fn get_authenticated(app: Router, uri: &str, bearer: &str) -> (StatusCode, Vec<u8>) {
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .header("authorization", format!("Bearer {bearer}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, body.to_vec())
    }

    async fn delete_authenticated(app: Router, uri: &str, bearer: &str) -> StatusCode {
        app.oneshot(
            axum::http::Request::builder()
                .method("DELETE")
                .uri(uri)
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    }

    #[tokio::test]
    async fn webhook_rejects_a_bad_signature_without_triggering_a_sync() {
        let secret = "hooksecret";
        let (app, mut rx, shutdown) = webhook_setup(secret, 10);
        let body = br#"{"after":"abc"}"#;

        let status = post_webhook(
            &app,
            body,
            &[("x-hub-signature-256", "sha256=deadbeef".to_string())],
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // No sync was triggered.
        assert!(rx.try_recv().is_err(), "a bad signature must not sync");
        shutdown.cancel();
    }

    #[tokio::test]
    async fn webhook_full_queue_is_bounded_and_delivery_can_be_retried() {
        let (app, mut receiver, shutdown) = webhook_setup("hooksecret", 100);
        let body = br#"{"after":"abc123"}"#;
        let headers = |id: usize| {
            vec![
                ("x-hub-signature-256", github_signature("hooksecret", body)),
                ("x-github-delivery", format!("queue-{id}")),
            ]
        };
        for id in 0..4 {
            assert_eq!(
                post_webhook(&app, body, &headers(id)).await,
                StatusCode::ACCEPTED
            );
        }
        let status = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            post_webhook(&app, body, &headers(4)),
        )
        .await
        .expect("full queue must not stall the request");
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        receiver.recv().await.unwrap();
        assert_eq!(
            post_webhook(&app, body, &headers(4)).await,
            StatusCode::ACCEPTED
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn webhook_refuses_a_closed_sync_loop() {
        let (app, receiver, shutdown) = webhook_setup("hooksecret", 10);
        drop(receiver);
        let body = br#"{"after":"abc123","ref":"refs/heads/main"}"#;
        let status = post_webhook(
            &app,
            body,
            &[
                ("x-hub-signature-256", github_signature("hooksecret", body)),
                ("x-github-delivery", "closed-loop".into()),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn webhook_rejects_a_missing_signature() {
        let (app, mut rx, shutdown) = webhook_setup("hooksecret", 10);
        let status = post_webhook(&app, br#"{}"#, &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err());
        shutdown.cancel();
    }

    #[tokio::test]
    async fn a_valid_github_signed_webhook_triggers_a_sync_without_a_bearer_token() {
        let secret = "hooksecret";
        let (app, mut rx, shutdown) = webhook_setup(secret, 10);
        let body = br#"{"after":"abc123","ref":"refs/heads/main"}"#;
        let sig = github_signature(secret, body);

        // No Authorization header at all — the provider never sends one.
        let status = post_webhook(
            &app,
            body,
            &[
                ("x-hub-signature-256", sig),
                ("x-github-delivery", "delivery-1".to_string()),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        // Observable: the sync loop was nudged.
        assert!(
            rx.recv().await.is_some(),
            "a valid webhook must trigger a sync"
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn webhook_rejects_a_replayed_delivery_id() {
        let secret = "hooksecret";
        let (app, mut rx, shutdown) = webhook_setup(secret, 10);
        let body = br#"{"after":"abc"}"#;
        let sig = github_signature(secret, body);
        let headers = [
            ("x-hub-signature-256", sig),
            ("x-github-delivery", "same-id".to_string()),
        ];

        assert_eq!(
            post_webhook(&app, body, &headers).await,
            StatusCode::ACCEPTED
        );
        assert!(rx.recv().await.is_some());
        // The same delivery id a second time is a replay.
        assert_eq!(
            post_webhook(&app, body, &headers).await,
            StatusCode::UNAUTHORIZED
        );
        assert!(rx.try_recv().is_err(), "a replay must not sync again");
        shutdown.cancel();
    }

    #[tokio::test]
    async fn webhook_rate_limit_trips_on_a_flood() {
        let secret = "hooksecret";
        // One trigger per minute.
        let (app, _rx, shutdown) = webhook_setup(secret, 1);
        let body = br#"{"after":"abc"}"#;
        let sig = github_signature(secret, body);

        // First unique delivery is accepted.
        assert_eq!(
            post_webhook(
                &app,
                body,
                &[
                    ("x-hub-signature-256", sig.clone()),
                    ("x-github-delivery", "d-1".to_string())
                ]
            )
            .await,
            StatusCode::ACCEPTED
        );
        // Second, fresh delivery id but the per-minute slot is used → 429.
        assert_eq!(
            post_webhook(
                &app,
                body,
                &[
                    ("x-hub-signature-256", sig),
                    ("x-github-delivery", "d-2".to_string())
                ]
            )
            .await,
            StatusCode::TOO_MANY_REQUESTS
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn webhook_fails_closed_without_a_configured_secret() {
        // A webhook tx but no validator (no `[gitops] webhook_secret`): the
        // route must refuse rather than trigger unauthenticated syncs.
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());
        tokio::spawn(async move {
            agent.run().await;
        });
        let (webhook_tx, mut rx) = mpsc::channel::<()>(4);
        let app = router_with_upgrade(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(webhook_tx),
            None,
            9117,
            None,
            None,
            None,
            "default".to_string(),
            None,
            900,
            crate::cluster::ClusterHttp::plaintext(),
            5050,
            "http",
            256 * 1024 * 1024,
            false,
            crate::bun::capabilities::StaticCapabilities::default(),
            crate::bun::readiness::ReadinessTracker::new(),
            None,
            None,
        );
        let status = post_webhook(&app, br#"{}"#, &[]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn app_metrics_name_injection_cannot_bypass_predicate() {
        // OBS1-remainder: `metrics_app_handler` interpolated `?name=` and the
        // `namespace/app` path into SQL raw. A crafted name like `x' OR '1'='1`
        // must be matched literally (finding nothing), not executed as SQL that
        // drops the WHERE predicate and leaks another app's rows.
        let (app, shutdown, _dir) = test_setup_with_metrics(&[
            ("cpu", "default/web", 1.0),
            ("secret_metric", "default/web", 99.0),
        ])
        .await;

        // Injection in the metric name.
        let injected = "x%27%20OR%20%271%27%3D%271"; // x' OR '1'='1
        let uri = format!("/v1/metrics/app/web/default?name={injected}");
        let (status, body) = get(app, &uri).await;
        assert_eq!(status, StatusCode::OK);
        let parsed: MetricsQueryResult = serde_json::from_slice(&body).unwrap();
        assert!(
            parsed.data.is_empty(),
            "injection leaked rows: {:?}",
            parsed.data
        );

        // The benign path still returns the one matching row.
        let (app, shutdown2, _dir2) = test_setup_with_metrics(&[
            ("cpu", "default/web", 1.0),
            ("secret_metric", "default/web", 99.0),
        ])
        .await;
        let (status, body) = get(app, "/v1/metrics/app/web/default?name=secret_metric").await;
        assert_eq!(status, StatusCode::OK);
        let parsed: MetricsQueryResult = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.data.len(), 1);
        assert_eq!(parsed.data[0].value, 99.0);

        shutdown.cancel();
        shutdown2.cancel();
    }

    /// With no `start`, the per-app endpoint reads the last fifteen minutes
    /// rather than the whole retention period.
    #[tokio::test]
    async fn app_metrics_default_to_the_recent_window() {
        let now = crate::mayo::types::Sample::now(0.0).timestamp;
        let (app, shutdown, _dir) = test_setup_with_timed_metrics(&[
            ("requests_total", "default/web", "web-0", now - 3600, 1.0),
            ("requests_total", "default/web", "web-0", now - 30, 2.0),
        ])
        .await;
        let (status, body) = get(app.clone(), "/v1/metrics/app/web/default").await;
        assert_eq!(status, StatusCode::OK);
        let parsed: MetricsQueryResult = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.data.len(), 1, "{:?}", parsed.data);
        assert_eq!(parsed.data[0].value, 2.0);

        // An explicit start still reaches back.
        let (_, body) = get(
            app,
            &format!("/v1/metrics/app/web/default?start={}", now - 7200),
        )
        .await;
        let parsed: MetricsQueryResult = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.data.len(), 2);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn app_metrics_per_series_returns_only_the_latest_samples() {
        let now = crate::mayo::types::Sample::now(0.0).timestamp;
        let (app, shutdown, _dir) = test_setup_with_timed_metrics(&[
            ("requests_total", "default/web", "web-0", now - 30, 1.0),
            ("requests_total", "default/web", "web-0", now - 20, 2.0),
            ("requests_total", "default/web", "web-0", now - 10, 3.0),
            ("up", "default/web", "web-0", now - 10, 1.0),
        ])
        .await;
        let (status, body) = get(app, "/v1/metrics/app/web/default?per_series=1").await;
        assert_eq!(status, StatusCode::OK);
        let parsed: MetricsQueryResult = serde_json::from_slice(&body).unwrap();
        let mut latest: Vec<(String, f64)> = parsed
            .data
            .iter()
            .map(|row| (row.metric_name.clone(), row.value))
            .collect();
        latest.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            latest,
            vec![("requests_total".to_string(), 3.0), ("up".to_string(), 1.0)]
        );
        shutdown.cancel();
    }

    /// The dashboard's chart script reads `{timestamps, series: [{label,
    /// values}]}` with one series per instance and `values` aligned to
    /// `timestamps` (brioche.js `toChart`). Counters arrive as rates.
    #[tokio::test]
    async fn the_chart_endpoint_answers_one_rate_line_per_instance() {
        let now = crate::mayo::types::Sample::now(0.0).timestamp;
        let (app, shutdown, _dir) = test_setup_with_timed_metrics(&[
            (
                "http_requests_total",
                "default/web",
                "web-0",
                now - 20,
                100.0,
            ),
            (
                "http_requests_total",
                "default/web",
                "web-0",
                now - 10,
                150.0,
            ),
            (
                "http_requests_total",
                "default/web",
                "web-1",
                now - 18,
                10.0,
            ),
            ("http_requests_total", "default/web", "web-1", now - 8, 30.0),
            (
                "http_requests_total",
                "default/other",
                "other-0",
                now - 8,
                999.0,
            ),
        ])
        .await;
        let (status, body) = get(
            app,
            "/v1/metrics/app/web/default/chart?name=http_requests_total&kind=rate",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let chart: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            chart["timestamps"],
            serde_json::json!([now - 10, now - 8]),
            "{chart}"
        );
        assert_eq!(
            chart["series"],
            serde_json::json!([
                {"label": "web-0", "values": [5.0, null]},
                {"label": "web-1", "values": [null, 2.0]},
            ]),
            "{chart}"
        );
        assert_eq!(chart["warnings"], serde_json::json!([]));
        shutdown.cancel();
    }

    #[tokio::test]
    async fn the_chart_endpoint_draws_a_histogram_as_mean_latency() {
        let now = crate::mayo::types::Sample::now(0.0).timestamp;
        let (app, shutdown, _dir) = test_setup_with_timed_metrics(&[
            ("latency_seconds_sum", "default/web", "web-0", now - 20, 1.0),
            ("latency_seconds_sum", "default/web", "web-0", now - 10, 3.0),
            (
                "latency_seconds_count",
                "default/web",
                "web-0",
                now - 20,
                10.0,
            ),
            (
                "latency_seconds_count",
                "default/web",
                "web-0",
                now - 10,
                50.0,
            ),
        ])
        .await;
        let (status, body) = get(
            app,
            "/v1/metrics/app/web/default/chart?name=latency_seconds&kind=mean",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let chart: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(chart["timestamps"], serde_json::json!([now - 10]));
        assert_eq!(chart["series"][0]["values"], serde_json::json!([0.05]));
        shutdown.cancel();
    }

    #[tokio::test]
    async fn the_app_page_charts_what_the_app_exposes() {
        let (app, shutdown, _dir) = test_setup_with_metrics(&[
            ("http_requests_total", "default/web", 1.0),
            ("http_request_duration_seconds_sum", "default/web", 1.0),
            ("http_request_duration_seconds_count", "default/web", 1.0),
        ])
        .await;
        let (status, body) = get(app, "/ui/app/web/default").await;
        assert_eq!(status, StatusCode::OK);
        let html = String::from_utf8(body.to_vec()).unwrap();
        for endpoint in [
            "chart?name=process_cpu_percent&amp;kind=gauge",
            "chart?name=http_requests_total&amp;kind=rate",
            "chart?name=http_request_duration_seconds&amp;kind=mean",
        ] {
            assert!(html.contains(endpoint), "{endpoint} missing from {html}");
        }
        shutdown.cancel();
    }

    #[tokio::test]
    async fn per_app_process_metric_is_queryable() {
        // OBS3: per-app (app-labelled) process metrics must be collectible and
        // then queryable through the per-app endpoint the dashboard and
        // autoscaler use. The collection loop labels them `namespace/app`; this
        // asserts that shape round-trips through the API's label filter.
        let (app, shutdown, _dir) = test_setup_with_metrics(&[
            ("process_cpu_percent", "default/web", 12.5),
            ("process_cpu_percent", "default/other", 99.0),
        ])
        .await;

        let (status, body) = get(app, "/v1/metrics/app/web/default?name=process_cpu_percent").await;
        assert_eq!(status, StatusCode::OK);
        let parsed: MetricsQueryResult = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.data.len(), 1, "expected exactly web's metric");
        assert_eq!(parsed.data[0].value, 12.5);
        assert_eq!(parsed.data[0].metric_name, "process_cpu_percent");

        shutdown.cancel();
    }
}

/// Multi-node API tests: several real routers on loopback listeners, each
/// with a scripted agent, sharing one membership table. They exercise the
/// cross-node routing paths without starting gossip or Raft.
#[cfg(test)]
mod cluster_routing_tests {
    use super::*;
    use crate::smoker::types::{FaultRequest, FaultSummary, FaultType, ReplicaEvidence};
    use tokio_util::sync::CancellationToken;

    const SERVICE_TOKEN: &str = "cluster-routing-internal";

    type Injected = Arc<tokio::sync::Mutex<Vec<(FaultRequest, Option<ReplicaEvidence>)>>>;

    struct FakeNode {
        url: String,
        injected: Injected,
    }

    struct FakeCluster {
        nodes: Vec<FakeNode>,
        membership: Arc<RwLock<Vec<NodeMembershipInfo>>>,
        operator: String,
        /// A read-only token confined to the `api` app.
        api_reader: String,
        stop: CancellationToken,
    }

    impl FakeCluster {
        /// List a member whose address has nothing listening, like a node
        /// that died before gossip noticed.
        async fn add_unreachable_member(&self, name: &str) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            drop(listener);
            self.membership.write().await.push(NodeMembershipInfo {
                node_id: crate::meat::NodeId::new(name),
                address,
                api_advertised: true,
            });
        }

        async fn get_json<T: serde::de::DeserializeOwned>(&self, entry: usize, path: &str) -> T {
            reqwest::Client::new()
                .get(format!("{}{path}", self.nodes[entry].url))
                .bearer_auth(&self.operator)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap()
        }
    }

    impl Drop for FakeCluster {
        fn drop(&mut self) {
            self.stop.cancel();
        }
    }

    fn instance(id: &str, app: &str, state: &str) -> InstanceStatus {
        InstanceStatus {
            id: id.to_string(),
            app_name: app.to_string(),
            namespace: "default".to_string(),
            state: state.to_string(),
            restart_count: 0,
            host_port: None,
            exit_code: None,
            pid: Some(4242),
        }
    }

    fn summary_of(id: u64, request: &FaultRequest) -> FaultSummary {
        FaultSummary {
            id,
            fault_type: request.fault_type.to_string(),
            target_service: request.target_service.clone(),
            target_instance: request.target_instance.clone(),
            target_node: request.target_node.clone(),
            remaining_secs: 60,
            injected_by: request.injected_by.clone(),
            node: None,
            routed: Vec::new(),
        }
    }

    /// Answer the agent commands the routing paths use from a fixed script.
    fn spawn_fake_agent(
        name: String,
        instances: Vec<InstanceStatus>,
        injected: Injected,
        mut commands: mpsc::Receiver<AgentCommand>,
        stop: CancellationToken,
    ) {
        tokio::spawn(async move {
            loop {
                let command = tokio::select! {
                    () = stop.cancelled() => return,
                    command = commands.recv() => match command {
                        Some(command) => command,
                        None => return,
                    },
                };
                match command {
                    AgentCommand::Status { response } => {
                        let _ = response.send(instances.clone());
                    }
                    AgentCommand::ListFaults { response } => {
                        let faults = injected
                            .lock()
                            .await
                            .iter()
                            .enumerate()
                            .map(|(index, (request, _))| summary_of(index as u64 + 1, request))
                            .collect();
                        let _ = response.send(faults);
                    }
                    AgentCommand::InjectFault {
                        request,
                        replica_evidence,
                        response,
                        ..
                    } => {
                        let mut injected = injected.lock().await;
                        let summary = summary_of(injected.len() as u64 + 1, &request);
                        injected.push((request, replica_evidence));
                        let _ = response.send(Ok(summary));
                    }
                    AgentCommand::ClearAllFaults { response } => {
                        let count = injected.lock().await.drain(..).count();
                        let _ = response.send(Ok(format!("{name} cleared {count}")));
                    }
                    _ => {}
                }
            }
        });
    }

    /// Start one router per `(name, instances)` pair, all sharing a
    /// membership table, a service token and one operator token.
    async fn start_cluster(layout: Vec<(&str, Vec<InstanceStatus>)>) -> FakeCluster {
        let created = crate::sesame::token::create_token(
            "operator",
            crate::sesame::types::ApiRole::Admin,
            crate::sesame::types::TokenScope::default(),
            None,
        )
        .unwrap();
        let api_reader = crate::sesame::token::create_token(
            "api-reader",
            crate::sesame::types::ApiRole::ReadOnly,
            crate::sesame::types::TokenScope {
                apps: Some(vec!["api".to_string()]),
                namespaces: None,
            },
            None,
        )
        .unwrap();
        let stop = CancellationToken::new();
        let mut listeners = Vec::new();
        let mut membership = Vec::new();
        for (name, _) in &layout {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            membership.push(NodeMembershipInfo {
                node_id: crate::meat::NodeId::new(*name),
                address: listener.local_addr().unwrap(),
                api_advertised: true,
            });
            listeners.push(listener);
        }
        let known = KnownMembers(Arc::new(RwLock::new(membership.clone())));
        let membership = Arc::new(RwLock::new(membership));
        let mut nodes = Vec::new();
        for ((name, instances), listener) in layout.into_iter().zip(listeners) {
            let (cmd_tx, cmd_rx) = mpsc::channel(32);
            let injected: Injected = Arc::default();
            spawn_fake_agent(
                name.to_string(),
                instances,
                Arc::clone(&injected),
                cmd_rx,
                stop.clone(),
            );
            let store = crate::sesame::auth::new_token_store();
            *store.write().await = vec![created.token.clone(), api_reader.token.clone()];
            let static_capabilities = crate::bun::capabilities::StaticCapabilities {
                test_policy: crate::testkit::safety::ClusterTestPolicy {
                    safety_class: crate::testkit::safety::ClusterSafetyClass::Development,
                    allowed_operations: std::collections::BTreeSet::from([
                        crate::testkit::safety::OperationPermission::InjectWorkloadFaults,
                    ]),
                    ..crate::testkit::safety::ClusterTestPolicy::default()
                },
                ..crate::bun::capabilities::StaticCapabilities::default()
            };
            let app = router_with_upgrade(
                cmd_tx,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(store),
                Some(SERVICE_TOKEN.to_string()),
                None,
                Some(Arc::clone(&membership)),
                None,
                None,
                listener.local_addr().unwrap().port(),
                None,
                None,
                None,
                "default".to_string(),
                Some(name.to_string()),
                900,
                crate::cluster::ClusterHttp::plaintext(),
                5050,
                "http",
                256 * 1024 * 1024,
                false,
                static_capabilities,
                super::super::readiness::ReadinessTracker::new(),
                None,
                None,
            )
            .layer(axum::Extension(known.clone()));
            let url = format!("http://{}", listener.local_addr().unwrap());
            let cancelled = stop.clone();
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move { cancelled.cancelled().await })
                    .await
                    .ok();
            });
            nodes.push(FakeNode { url, injected });
        }
        FakeCluster {
            nodes,
            membership,
            operator: created.plaintext,
            api_reader: api_reader.plaintext,
            stop,
        }
    }

    fn kill(count: u32) -> FaultRequest {
        FaultRequest {
            fault_type: FaultType::Kill { count },
            target_service: "web".to_string(),
            namespace: None,
            target_instance: None,
            target_node: None,
            duration: std::time::Duration::from_secs(0),
            injected_by: String::new(),
            reason: None,
            include_leader: false,
            override_safety: false,
            acknowledged: true,
        }
    }

    async fn inject(
        cluster: &FakeCluster,
        entry: usize,
        request: &FaultRequest,
    ) -> (StatusCode, serde_json::Value) {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/fault", cluster.nodes[entry].url))
            .bearer_auth(&cluster.operator)
            .json(request)
            .send()
            .await
            .unwrap();
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
        let text = response.text().await.unwrap();
        let body = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
        (status, body)
    }

    async fn injected_count(cluster: &FakeCluster) -> usize {
        let mut total = 0;
        for node in &cluster.nodes {
            total += node.injected.lock().await.len();
        }
        total
    }

    #[tokio::test]
    async fn a_workload_fault_reaches_the_node_that_runs_its_target() {
        let cluster = start_cluster(vec![
            ("node-1", vec![]),
            (
                "node-2",
                vec![
                    instance("default/web-0", "web", "running"),
                    instance("default/web-1", "web", "running"),
                ],
            ),
        ])
        .await;

        let (status, body) = inject(&cluster, 0, &kill(1)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let summary: FaultSummary = serde_json::from_value(body).unwrap();
        assert_eq!(summary.node.as_deref(), Some("node-2"));
        assert_eq!(summary.target_node.as_deref(), Some("node-2"));

        assert!(cluster.nodes[0].injected.lock().await.is_empty());
        let owner = cluster.nodes[1].injected.lock().await;
        assert_eq!(owner.len(), 1);
        let (request, evidence) = &owner[0];
        assert_eq!(request.fault_type, FaultType::Kill { count: 1 });
        // The owner recorded the caller's token, not a node identity.
        assert_eq!(request.injected_by, "operator");
        assert_eq!(
            *evidence,
            Some(ReplicaEvidence {
                replicas: 2,
                faulted_replicas: 0,
            })
        );
    }

    #[tokio::test]
    async fn the_replica_rail_counts_replicas_on_every_node() {
        // One replica on each node: killing both leaves nothing, even though
        // each node alone would think it was only losing its own copy.
        let cluster = start_cluster(vec![
            ("node-1", vec![instance("default/web-0", "web", "running")]),
            ("node-2", vec![instance("default/web-0", "web", "running")]),
        ])
        .await;

        let (status, body) = inject(&cluster, 0, &kill(2)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.to_string().contains("replica"), "{body}");
        assert_eq!(injected_count(&cluster).await, 0);

        let (status, body) = inject(&cluster, 0, &kill(1)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(injected_count(&cluster).await, 1);
    }

    #[tokio::test]
    async fn a_fault_on_several_owners_reports_every_fault_it_created() {
        let cluster = start_cluster(vec![
            ("node-1", vec![instance("default/web-0", "web", "running")]),
            ("node-2", vec![instance("default/web-0", "web", "running")]),
            ("node-3", vec![instance("default/web-0", "web", "running")]),
        ])
        .await;

        let (status, body) = inject(&cluster, 1, &kill(2)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let summary: FaultSummary = serde_json::from_value(body).unwrap();
        let mut nodes: Vec<_> = std::iter::once(&summary)
            .chain(&summary.routed)
            .map(|fault| fault.node.clone().unwrap())
            .collect();
        nodes.sort();
        assert_eq!(nodes, vec!["node-1", "node-2"]);
        assert!(cluster.nodes[2].injected.lock().await.is_empty());
    }

    #[tokio::test]
    async fn a_fault_with_no_running_target_is_refused_before_anything_runs() {
        let cluster = start_cluster(vec![
            ("node-1", vec![]),
            ("node-2", vec![instance("default/web-0", "web", "stopped")]),
        ])
        .await;
        let (status, body) = inject(&cluster, 0, &kill(1)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.to_string().contains("no running instances"), "{body}");
        assert_eq!(injected_count(&cluster).await, 0);
    }

    fn network(fault_type: FaultType) -> FaultRequest {
        FaultRequest {
            fault_type,
            duration: std::time::Duration::from_secs(60),
            ..kill(0)
        }
    }

    async fn nodes_that_got_a_fault(cluster: &FakeCluster) -> Vec<usize> {
        let mut nodes = Vec::new();
        for (index, node) in cluster.nodes.iter().enumerate() {
            if !node.injected.lock().await.is_empty() {
                nodes.push(index);
            }
        }
        nodes
    }

    #[tokio::test]
    async fn a_network_fault_on_every_caller_lands_on_every_node() {
        // The target runs on node-2 only, but its callers could be anywhere:
        // the connect hook and the DNS responder act on the caller's node.
        let cluster = start_cluster(vec![
            ("node-1", vec![]),
            ("node-2", vec![instance("default/web-0", "web", "running")]),
            ("node-3", vec![]),
        ])
        .await;

        let (status, body) = inject(&cluster, 0, &network(FaultType::DnsNxdomain)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let summary: FaultSummary = serde_json::from_value(body).unwrap();
        let mut holders: Vec<_> = std::iter::once(&summary)
            .chain(&summary.routed)
            .map(|fault| fault.node.clone().unwrap())
            .collect();
        holders.sort();
        assert_eq!(holders, vec!["node-1", "node-2", "node-3"]);
        assert_eq!(nodes_that_got_a_fault(&cluster).await, vec![0, 1, 2]);
        for node in &cluster.nodes {
            let injected = node.injected.lock().await;
            // Each node's share names that node, and the owner re-plans it
            // as a network fault rather than a target-owner fault.
            assert_eq!(injected[0].0.fault_type, FaultType::DnsNxdomain);
            assert_eq!(injected[0].1, None, "no replica evidence for traffic");
        }
    }

    #[tokio::test]
    async fn a_network_fault_from_one_source_lands_only_where_that_source_runs() {
        let mut other_tenant = instance("team-b/frontend-0", "frontend", "running");
        other_tenant.namespace = "team-b".to_string();
        let cluster = start_cluster(vec![
            ("node-1", vec![instance("default/web-0", "web", "running")]),
            ("node-2", vec![other_tenant]),
            (
                "node-3",
                vec![instance("default/frontend-0", "frontend", "running")],
            ),
        ])
        .await;

        let partition = network(FaultType::Partition {
            source_app: Some("frontend".to_string()),
        });
        let (status, body) = inject(&cluster, 0, &partition).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(nodes_that_got_a_fault(&cluster).await, vec![2]);

        // A source with no running instance anywhere is refused up front.
        let nowhere = network(FaultType::Delay {
            delay_ns: 300_000_000,
            jitter_ns: 0,
            source_app: Some("worker".to_string()),
        });
        let (status, body) = inject(&cluster, 0, &nowhere).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.to_string().contains("worker"), "{body}");
        assert_eq!(injected_count(&cluster).await, 1);
    }

    #[tokio::test]
    async fn the_cluster_fault_list_and_clear_reach_every_node() {
        let cluster = start_cluster(vec![
            ("node-1", vec![]),
            (
                "node-2",
                vec![
                    instance("default/web-0", "web", "running"),
                    instance("default/web-1", "web", "running"),
                ],
            ),
        ])
        .await;
        assert_eq!(inject(&cluster, 0, &kill(1)).await.0, StatusCode::OK);

        let client = reqwest::Client::new();
        let listing: ClusterFaultList = client
            .get(format!("{}/v1/fault?cluster=true", cluster.nodes[0].url))
            .bearer_auth(&cluster.operator)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(listing.warnings.is_empty(), "{:?}", listing.warnings);
        assert_eq!(listing.faults.len(), 1);
        assert_eq!(listing.faults[0].node.as_deref(), Some("node-2"));

        let response = client
            .delete(format!("{}/v1/fault", cluster.nodes[0].url))
            .bearer_auth(&cluster.operator)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let message = response.text().await.unwrap();
        assert!(message.contains("node-2: node-2 cleared 1"), "{message}");
        assert!(cluster.nodes[1].injected.lock().await.is_empty());
    }

    #[tokio::test]
    async fn top_merges_every_node_and_warns_about_the_missing_one() {
        let cluster = start_cluster(vec![
            ("node-1", vec![instance("default/web-0", "web", "running")]),
            (
                "node-2",
                vec![
                    instance("default/web-0", "web", "running"),
                    instance("default/api-0", "api", "running"),
                ],
            ),
        ])
        .await;
        cluster.add_unreachable_member("node-3").await;

        let top: crate::bun::top::ClusterTop = cluster.get_json(0, "/v1/top?cluster=true").await;
        let rows: Vec<_> = top
            .rows
            .iter()
            .map(|row| (row.node.as_str(), row.instance.app_name.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![("node-1", "web"), ("node-2", "api"), ("node-2", "web")]
        );
        assert_eq!(top.warnings.len(), 1, "{:?}", top.warnings);
        assert!(
            top.warnings[0].starts_with("node node-3"),
            "{:?}",
            top.warnings
        );

        // Without `cluster`, a node answers for itself only.
        let local: Vec<crate::bun::top::TopRow> = cluster.get_json(1, "/v1/top").await;
        assert!(local.iter().all(|row| row.node == "node-2"));
        assert_eq!(local.len(), 2);
    }

    async fn relay(
        cluster: &FakeCluster,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
    ) -> (StatusCode, String) {
        let mut request =
            reqwest::Client::new().request(method, format!("{}{path}", cluster.nodes[0].url));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
        (status, response.text().await.unwrap())
    }

    #[tokio::test]
    async fn the_relay_reaches_a_peer_with_the_callers_own_credential() {
        let cluster = start_cluster(vec![
            ("node-1", vec![]),
            (
                "node-2",
                vec![
                    instance("default/web-0", "web", "running"),
                    instance("default/api-0", "api", "running"),
                ],
            ),
        ])
        .await;
        let operator = Some(cluster.operator.as_str());

        let (status, body) = relay(
            &cluster,
            reqwest::Method::GET,
            "/v1/nodes/node-2/relay/v1/status",
            operator,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let statuses: Vec<InstanceStatus> = serde_json::from_str(&body).unwrap();
        assert_eq!(statuses.len(), 2);

        // A scoped caller stays scoped on the far side: the peer filtered with
        // the caller's token, not a node identity that sees everything.
        let (status, body) = relay(
            &cluster,
            reqwest::Method::GET,
            "/v1/nodes/node-2/relay/v1/status",
            Some(&cluster.api_reader),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let statuses: Vec<InstanceStatus> = serde_json::from_str(&body).unwrap();
        let apps: Vec<_> = statuses.iter().map(|s| s.app_name.as_str()).collect();
        assert_eq!(apps, vec!["api"]);

        // No credential, no relay.
        let (status, _) = relay(
            &cluster,
            reqwest::Method::GET,
            "/v1/nodes/node-2/relay/v1/status",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// A node-kill fault leaves the target's API open while gossip calls it
    /// dead. The relay must still reach it, or nobody outside the cluster
    /// network can watch it or clear the fault.
    #[tokio::test]
    async fn the_relay_reaches_a_member_gossip_no_longer_counts_as_alive() {
        let cluster = start_cluster(vec![
            ("node-1", vec![]),
            ("node-2", vec![instance("default/web-0", "web", "running")]),
        ])
        .await;
        cluster
            .membership
            .write()
            .await
            .retain(|member| member.node_id != crate::meat::NodeId::new("node-2"));

        let (status, body) = relay(
            &cluster,
            reqwest::Method::GET,
            "/v1/nodes/node-2/relay/v1/status",
            Some(cluster.operator.as_str()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let statuses: Vec<InstanceStatus> = serde_json::from_str(&body).unwrap();
        assert_eq!(statuses.len(), 1);
        cluster.stop.cancel();
    }

    #[test]
    fn the_relay_forwards_one_apps_deploy_history_and_nothing_nested() {
        let get = axum::http::Method::GET;
        assert!(relay_allows(&get, "v1/deploys/history/web"));
        assert!(!relay_allows(&get, "v1/deploys/history/"));
        assert!(!relay_allows(&get, "v1/deploys/history/web/extra"));
        assert!(!relay_allows(&get, "v1/deploys/history"));
        assert!(!relay_allows(
            &axum::http::Method::POST,
            "v1/deploys/history/web"
        ));
    }

    #[test]
    fn the_relay_forwards_exec_to_one_app_and_nothing_nested() {
        let post = axum::http::Method::POST;
        assert!(relay_allows(&post, "v1/exec/web/default"));
        assert!(!relay_allows(&post, "v1/exec/web/default/extra"));
        assert!(!relay_allows(&post, "v1/exec/web"));
        assert!(!relay_allows(&post, "v1/exec//default"));
        assert!(!relay_allows(
            &axum::http::Method::GET,
            "v1/exec/web/default"
        ));
    }

    #[tokio::test]
    async fn the_relay_forwards_only_the_diagnostic_reads() {
        let cluster = start_cluster(vec![
            ("node-1", vec![]),
            ("node-2", vec![instance("default/web-0", "web", "running")]),
        ])
        .await;
        let operator = Some(cluster.operator.as_str());
        for (method, path) in [
            (reqwest::Method::POST, "/v1/nodes/node-2/relay/v1/fault"),
            (reqwest::Method::GET, "/v1/nodes/node-2/relay/v1/token/list"),
            (reqwest::Method::DELETE, "/v1/nodes/node-2/relay/v1/fault"),
            (
                reqwest::Method::GET,
                "/v1/nodes/node-2/relay/v1/nodes/node-1/relay/v1/status",
            ),
        ] {
            let (status, body) = relay(&cluster, method.clone(), path, operator).await;
            assert!(
                status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path} was relayed: {status} {body}"
            );
        }
        assert_eq!(injected_count(&cluster).await, 0);

        let (status, body) = relay(
            &cluster,
            reqwest::Method::GET,
            "/v1/nodes/node-9/relay/v1/status",
            operator,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(body.contains("node-9"), "{body}");
    }

    #[tokio::test]
    async fn the_relay_keeps_the_query_string() {
        let cluster = start_cluster(vec![
            ("node-1", vec![]),
            (
                "node-2",
                vec![
                    instance("default/web-0", "web", "running"),
                    instance("default/web-1", "web", "running"),
                ],
            ),
        ])
        .await;
        assert_eq!(inject(&cluster, 1, &kill(1)).await.0, StatusCode::OK);
        let (status, body) = relay(
            &cluster,
            reqwest::Method::GET,
            "/v1/nodes/node-2/relay/v1/fault?cluster=true",
            Some(&cluster.operator),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let listing: ClusterFaultList = serde_json::from_str(&body).unwrap();
        assert_eq!(listing.faults.len(), 1);
    }
}

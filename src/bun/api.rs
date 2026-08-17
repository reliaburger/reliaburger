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
        .route("/v1/readiness", get(readiness_handler))
        .route("/v1/jobs", get(jobs_handler))
        .route("/v1/events", get(events_handler))
        .route("/v1/ws/events", get(ws_events_handler))
        .route("/v1/ws/logs/{app}/{namespace}", get(ws_logs_handler))
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
        .route("/v1/capabilities", get(capabilities_handler))
        .route(
            "/v1/capabilities/cluster",
            get(cluster_capabilities_handler),
        )
        .route("/v1/diagnostics", get(diagnostics_handler))
        .route("/v1/diagnostics/apps", get(desired_apps_handler))
        .route("/v1/trace", post(trace_handler))
        .route("/v1/test/leases", post(test_lease_create_handler))
        .route("/v1/test/leases/{id}", get(test_lease_get_handler))
        .route("/v1/test/leases/{id}/renew", post(test_lease_renew_handler))
        .route(
            "/v1/test/leases/{id}",
            axum::routing::delete(test_lease_release_handler),
        )
        .route("/v1/cluster/nodes", get(nodes_handler))
        .route("/v1/cluster/council", get(council_handler))
        .route("/v1/upgrade/apply", post(upgrade_apply_handler))
        .route("/v1/upgrade/status", get(upgrade_status_handler))
        .route("/v1/upgrade/rollback", post(upgrade_rollback_handler))
        .route("/v1/upgrade/start", post(upgrade_start_handler))
        .route("/v1/upgrade/cluster", get(upgrade_cluster_handler))
        .route("/v1/upgrade/resume", post(upgrade_resume_handler))
        .route(
            "/v1/upgrade/cluster-rollback",
            post(upgrade_cluster_rollback_handler),
        )
        .route("/v1/cluster/elect", post(cluster_elect_handler))
        .route("/v1/chaos/partition", post(chaos_partition_handler))
        .route("/v1/chaos/heal", post(chaos_heal_handler))
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
        .route("/v1/metrics/cluster", get(metrics_cluster_handler))
        .route(
            "/v1/metrics/app/{app}/{namespace}",
            get(metrics_app_handler),
        )
        .route("/v1/alerts", get(alerts_handler))
        .route("/v1/logs/sql", get(logs_sql_handler))
        .route("/v1/deploys/active", get(deploys_active_handler))
        .route("/v1/deploys/operations", get(deploys_operations_handler))
        .route("/v1/deploys/history/{app}", get(deploys_history_handler))
        .route("/v1/rollback/{app}/{namespace}", post(rollback_handler))
        .route("/v1/placements/{node_id}", get(placements_handler))
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
        .route("/v1/secret/rotate", post(secret_rotate_handler))
        .route_layer(axum::middleware::from_fn_with_state(
            auth_state,
            crate::sesame::auth::auth_middleware,
        ))
        .with_state(state);

    public.merge(auth_routes).merge(protected)
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
    let certificates = match &state.static_capabilities.diagnostics.node_certificate {
        // The node leaf metadata is genuinely available and diagnosable (expiry
        // is checked from it). That the broader workload-certificate inventory
        // and hot-reload state aren't exposed yet is a static product
        // limitation, not a per-request collection failure — reporting it
        // `Degraded` forced every mTLS cluster's `relish wtf` to exit 2, which
        // the "0 = healthy" contract forbids. Serve it as `Available`.
        Some(certificate) => DiagnosticSource::Available {
            observed_at,
            value: vec![certificate.clone()],
        },
        None if !state.static_capabilities.identity => DiagnosticSource::Unsupported {
            reason: "workload identity issuance is disabled and no node mTLS leaf is loaded"
                .to_string(),
        },
        None => DiagnosticSource::Unavailable {
            reason: "identity issuance is enabled but no safe certificate inventory is available"
                .to_string(),
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
                    service_port: spec.port,
                },
            )
            .collect::<Vec<_>>();
        apps.sort_by(|left, right| {
            (&left.namespace, &left.app).cmp(&(&right.namespace, &right.app))
        });
        apps
    } else {
        let (response, receiver) = oneshot::channel();
        if state
            .cmd_tx
            .send(AgentCommand::DesiredApps { response })
            .await
            .is_err()
        {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "agent unavailable"})),
            )
                .into_response();
        }
        match receiver.await {
            Ok(apps) => apps,
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": "agent dropped response"})),
                )
                    .into_response();
            }
        }
    };
    Json(filter_desired_apps_for_scope(apps, auth.as_deref())).into_response()
}

/// `POST /v1/trace` — fixed DNS and TCP probes from a local source workload.
async fn trace_handler(
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
            Json(
                serde_json::json!({"error": "trace destination port must be between 1 and 65535"}),
            ),
        )
            .into_response();
    }
    if !valid_trace_label(&request.source) || !valid_trace_label(&request.source_namespace) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "trace source and namespace must be DNS labels"})),
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

    let internal_destination = valid_trace_label(&request.destination);
    if internal_destination {
        if !valid_trace_label(&request.destination_namespace) {
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
                Json(serde_json::json!({"error": "external trace destination requires --port"})),
            )
                .into_response();
        };
        let Some(auth) = auth.as_deref() else {
            return (
                StatusCode::FORBIDDEN,
                "external trace requires an authenticated Admin credential",
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
                "external trace destination is not exactly allowlisted as host:port",
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
    match tokio::time::timeout(std::time::Duration::from_secs(20), receiver).await {
        Ok(Ok(Ok(result))) => Json(result).into_response(),
        Ok(Ok(Err(crate::bun::BunError::AppNotFound { .. }))) => (
            StatusCode::NOT_FOUND,
            "source app has no running instance on this node",
        )
            .into_response(),
        Ok(Ok(Err(crate::bun::BunError::TraceBusy))) => (
            StatusCode::TOO_MANY_REQUESTS,
            "too many connectivity traces are already running on this node",
        )
            .into_response(),
        Ok(Ok(Err(error))) => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
        Ok(Err(_)) => (StatusCode::SERVICE_UNAVAILABLE, "agent dropped response").into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "trace timed out after 20 seconds",
        )
            .into_response(),
    }
}

fn valid_trace_label(value: &str) -> bool {
    let bytes = value.as_bytes();
    let edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    !bytes.is_empty()
        && bytes.len() <= 63
        && edge(bytes[0])
        && edge(bytes[bytes.len() - 1])
        && bytes.iter().all(|byte| edge(*byte) || *byte == b'-')
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
    if let Some(council) = &state.council
        && !council.is_leader().await
    {
        return forward_test_lease_request(
            &state,
            council,
            reqwest::Method::POST,
            "/v1/test/leases",
            &headers,
            Some(&request),
        )
        .await;
    }
    let mut random = [0u8; 16];
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to generate lease id",
        )
            .into_response();
    }
    let lease_id = hex::encode(random);
    let namespace = request
        .namespace
        .unwrap_or_else(|| format!("rbtest-{}", &lease_id[..12]));
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
    let lease = match crate::testkit::lease::TestLease::new(
        lease_id,
        auth.principal_id.clone(),
        auth.token_name.clone(),
        namespace,
        now,
        now.saturating_add(ttl_millis),
    ) {
        Ok(lease) => lease,
        Err(error) => return lease_error_response(error),
    };

    if let Some(council) = &state.council {
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

async fn test_lease_get_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(lease_id): Path<String>,
) -> Response {
    let auth =
        match authenticated_test_user(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly) {
            Ok(auth) => auth,
            Err(response) => return response,
        };
    let Some(lease) = find_test_lease(&state, &lease_id).await else {
        return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
    };
    if lease.owner_id != auth.principal_id && auth.role != crate::sesame::types::ApiRole::Admin {
        return lease_error_response(crate::testkit::lease::LeaseError::WrongOwner);
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

    if let Some(council) = &state.council {
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
        None
    } else {
        return lease_error_response(crate::testkit::lease::LeaseError::WrongOwner);
    };
    let result = if let Some(council) = &state.council {
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
        .request(method, format!("{leader_url}{path}"));
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
        crate::testkit::lease::LeaseError::WrongOwner => StatusCode::FORBIDDEN,
        crate::testkit::lease::LeaseError::NotActive
        | crate::testkit::lease::LeaseError::AlreadyExists
        | crate::testkit::lease::LeaseError::NamespaceOwned
        | crate::testkit::lease::LeaseError::NamespaceMismatch
        | crate::testkit::lease::LeaseError::ResourceLimit => StatusCode::CONFLICT,
        crate::testkit::lease::LeaseError::TooManyLeases => StatusCode::TOO_MANY_REQUESTS,
        crate::testkit::lease::LeaseError::InvalidId
        | crate::testkit::lease::LeaseError::InvalidOwner
        | crate::testkit::lease::LeaseError::InvalidNamespace
        | crate::testkit::lease::LeaseError::InvalidExpiry
        | crate::testkit::lease::LeaseError::UnsupportedSchema { .. } => StatusCode::BAD_REQUEST,
        crate::testkit::lease::LeaseError::Persistence(_)
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
            "upgrade_in_flight": manager.upgrade_in_flight(),
            // Ids this node attempted and reverted — the orchestrator
            // reads these to detect node-side reverts.
            "failed_upgrade_ids": manager.reverted_upgrade_ids(),
        })),
        None => Json(serde_json::json!({
            "version": crate::upgrade::version::compiled_version().to_string(),
            "upgrade_in_flight": false,
            "failed_upgrade_ids": [],
        })),
    }
}

/// Apply a node-level upgrade directive (admin). Responds 202 once the
/// binary is verified and staged; the process execs moments later.
async fn upgrade_apply_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
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

    let (tx, rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::UpgradeApply {
            directive,
            response: tx,
        })
        .await
        .is_err()
    {
        return agent_unavailable();
    }
    match rx.await {
        Ok(Ok(())) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "upgrading" })),
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
    let (tx, rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::UpgradeStatus { response: tx })
        .await
        .is_err()
    {
        return agent_unavailable();
    }
    match rx.await {
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
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
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

    let (tx, rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::UpgradeRollback {
            version: request.version,
            response: tx,
        })
        .await
        .is_err()
    {
        return agent_unavailable();
    }
    match rx.await {
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
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
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
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    if council.desired_state().await.active_upgrade.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "an upgrade is already in progress" })),
        )
            .into_response();
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
        direction: request
            .direction
            .unwrap_or(crate::upgrade::types::UpgradeDirection::Upgrade),
        phase: crate::upgrade::types::ClusterUpgradePhase::Preparing,
        registry_address: request.registry_address,
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

/// Build the leader's authoritative view of every node for upgrade
/// planning (UPG2): node id → its API address (from gossip membership) and
/// role (from the Raft voter set + current leader). This is the source of
/// truth the client's start request is validated against.
///
/// The role comes from Raft: the current leader is `Leader`, other voters
/// are `Council`, and everything else `Worker`. Gossip identifies nodes by
/// name; the Raft voter set by `raft_id_from_name(name)`, so we bridge them
/// with that same stable hash.
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
                address: member.address.to_string(),
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
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
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

/// Start a cluster-wide rolling rollback (admin, leader only). The
/// binaries are already on every node's disk, so there is no registry or
/// signature material — just a target version and the node list.
async fn upgrade_cluster_rollback_handler(
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
    if council.desired_state().await.active_upgrade.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "an upgrade is already in progress" })),
        )
            .into_response();
    }

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
        nodes: request
            .nodes
            .into_iter()
            .map(|node| crate::upgrade::types::NodeUpgradeRecord {
                node_id: node.node_id,
                address: node.address,
                role: node.role,
                from_version: None,
                phase: crate::upgrade::types::NodeUpgradePhase::Pending,
                since: None,
            })
            .collect(),
    };

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
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
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
    if let Some(lease_id) = &lease_id {
        if !config.job.is_empty() || !config.permission.is_empty() || !config.build.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                "leased apply currently accepts apps and their namespace declaration only",
            )
                .into_response();
        }
        let Some(auth) = auth.as_deref() else {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        };
        let Some(lease) = find_test_lease(&state, lease_id).await else {
            return lease_error_response(crate::testkit::lease::LeaseError::NotFound);
        };
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
        lease_owner_id = Some(lease.owner_id);
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
        for spec in config.app.values() {
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            if crate::testkit::lease::valid_test_namespace(namespace) {
                return (
                    StatusCode::CONFLICT,
                    "test lease namespace requires x-reliaburger-test-lease",
                )
                    .into_response();
            }
        }
    }

    // AUTH1: a scoped token may only deploy apps its scope covers. Check
    // every app in the manifest, so one out-of-scope app rejects the whole
    // apply rather than being silently dropped. A principal's `[permission]`
    // spec (when it has one) must also grant `deploy` on each app — and
    // `host-exec` for any app that runs an inline `script` (a host process),
    // which is the capability the whitepaper ties to script workloads.
    let permissions = match &state.council {
        Some(council) => council.desired_state().await.permissions,
        None => std::collections::BTreeMap::new(),
    };
    for (app_name, spec) in &config.app {
        let namespace = spec.namespace.as_deref().unwrap_or("default");
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
        if spec.script.is_some()
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
        )
        .await;
    }

    let lease_operation = if let (Some(lease_id), Some(owner_id)) =
        (&lease_id, lease_owner_id.as_deref())
    {
        let now = crate::testkit::lease::now_unix_millis();
        let app_ids = config
            .app
            .iter()
            .map(|(app_name, spec)| {
                crate::meat::AppId::new(app_name, spec.namespace.as_deref().unwrap_or("default"))
            })
            .collect();
        match state
            .local_test_leases
            .begin_app_operation(lease_id, owner_id, app_ids, now)
            .await
        {
            Ok(operation) => Some(operation),
            Err(error) => return lease_error_response(error),
        }
    } else {
        None
    };

    let (agent_event_tx, mut agent_event_rx) = mpsc::channel::<ApplyEvent>(32);
    if state
        .cmd_tx
        .send(AgentCommand::Deploy {
            config,
            events: agent_event_tx,
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
            for name in [
                axum::http::header::AUTHORIZATION,
                axum::http::header::COOKIE,
            ] {
                if let Some(value) = caller_headers.get(&name) {
                    request = request.header(name.as_str(), value.as_bytes());
                }
            }
        } else if let Some(token) = &state.service_token {
            request = request.bearer_auth(token);
        }
        return match request.send().await {
            Ok(response) => {
                let stream = response.bytes_stream();
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from_stream(stream))
                    .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
            }
            Err(e) => (
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

/// `GET /v1/placements/{node_id}` — the apps (and per-node replica
/// counts) the leader has assigned to a node. Served from the Raft
/// state machine; reconcilers poll this every couple of seconds.
async fn placements_handler(
    State(state): State<ApiState>,
    Path(node_id): Path<String>,
) -> Response {
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "not running in cluster mode" })),
        )
            .into_response();
    };

    let desired = council.desired_state().await;
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
        // Piggyback the replicated endpoint catalogue (12b.4) so the polling
        // node can resolve services on other nodes.
        endpoint_catalog: desired.endpoint_catalog.clone(),
    })
    .into_response()
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

/// List all run-to-completion workload instances.
async fn jobs_handler(State(state): State<ApiState>) -> Response {
    let (response_tx, response_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::JobStatus {
            response: response_tx,
        })
        .await
        .is_err()
    {
        return agent_unavailable();
    }
    match response_rx.await {
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
        return cluster_stop(state, council, app, namespace).await;
    }

    stop_local(&state, app, namespace).await
}

/// Stop an app in cluster mode: clear its desired state through Raft, then
/// best-effort stop the local instances.
async fn cluster_stop(
    state: ApiState,
    council: Arc<crate::council::CouncilNode>,
    app: String,
    namespace: String,
) -> Response {
    // Followers can't write to Raft (openraft does not forward client
    // writes), so forward the whole stop to the leader's API.
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
        let url = format!("{leader_url}/v1/stop/{app}/{namespace}");
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
                    "error": format!("failed to forward stop to the leader: {e}")
                })),
            )
                .into_response(),
        };
    }

    let app_id = crate::meat::AppId::new(&app, &namespace);
    if let Err(e) = council
        .write(crate::council::types::RaftRequest::AppDelete { app_id })
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("failed to clear desired state: {e}")
            })),
        )
            .into_response();
    }

    // The desired state is gone; stop the local replica if we have one.
    // A missing local instance is expected on a leader that holds no
    // replica, so it is not an error here.
    let (resp_tx, resp_rx) = oneshot::channel();
    let _ = state
        .cmd_tx
        .send(AgentCommand::Stop {
            app_name: app,
            namespace,
            response: resp_tx,
        })
        .await;
    let _ = resp_rx.await;

    Json(serde_json::json!({ "status": "stopped" })).into_response()
}

/// Stop an app on this node only (standalone mode).
async fn stop_local(state: &ApiState, app: String, namespace: String) -> Response {
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
/// Cross-node log query. Looks up which nodes run the app from the
/// council placement state, fans out the query to those nodes, and
/// merge-sorts results by timestamp.
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
        let mut nodes: Vec<(String, String)> = Vec::new();
        let mut warnings = Vec::new();

        for node_id in &node_ids {
            if let Some(info) = members.iter().find(|m| m.node_id == *node_id) {
                nodes.push((
                    node_id.0.clone(),
                    state.cluster_http.url(&info.address.to_string(), ""),
                ));
            } else {
                // No membership entry — can't even reach it. A partial failure.
                warnings.push(LogQueryWarning::NodeUnresponsive {
                    node_id: node_id.0.clone(),
                });
            }
        }
        drop(members);

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
    let tokens = auth.tokens.read().await;
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
        crate::sesame::auth::authenticate(&form.token, &tokens)
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
    };
    drop(tokens);

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

/// Request body for cluster join: a one-time token, the joiner's node id, and
/// the joiner's CSR (PKI4 — the joiner keeps its private key).
#[derive(Deserialize)]
struct JoinRequest {
    token: String,
    node_id: String,
    csr_b64: String,
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
        "node_ca_b64": encoder.encode(&node_ca.certificate_der),
        "root_ca_b64": encoder.encode(&root_ca.certificate_der),
    }))
    .into_response()
}

async fn join_handler(State(state): State<ApiState>, Json(body): Json<JoinRequest>) -> Response {
    use base64::Engine as _;
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
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::JoinIssue {
            token: body.token,
            node_id: body.node_id,
            csr_der,
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
        Ok(Ok(bundle)) => Json(bundle).into_response(),
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
    #[serde(default)]
    acknowledged: bool,
}

/// Inject a network partition.
async fn chaos_partition_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(body): Json<ChaosPartitionRequest>,
) -> Response {
    // AUTH4: fault injection is an operator action, not a node-to-node one.
    if let Err(resp) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    let (principal, role) = auth
        .as_deref()
        .map(|auth| (auth.principal_id.as_str(), auth.role))
        .unwrap_or(("local-bootstrap", crate::sesame::types::ApiRole::Admin));
    if let Err(error) = state.static_capabilities.test_policy.authorise(
        crate::testkit::safety::OperationPermission::AlterNodeState,
        &crate::testkit::safety::OperationAuthorisation {
            principal,
            role,
            acknowledged: body.acknowledged,
        },
    ) {
        return (StatusCode::FORBIDDEN, error.to_string()).into_response();
    }
    let audit_peers = body.peers.clone();
    let audit_duration_seconds = body.duration_secs;
    let injected_by = auth
        .as_deref()
        .map(|auth| auth.token_name.clone())
        .unwrap_or_else(|| "local-bootstrap".to_string());
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::InjectPartition {
            peers: body.peers,
            duration_secs: body.duration_secs,
            injected_by,
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
        Ok(Ok((msg, summary))) => {
            record_fault_audit(
                &state,
                FaultAudit {
                    action: "fault.injected",
                    principal,
                    severity: crate::bun::events::EventSeverity::Warning,
                    app: None,
                    node: None,
                    details: std::collections::BTreeMap::from([
                        ("fault_type".to_string(), "CouncilPartition".to_string()),
                        (
                            "duration_seconds".to_string(),
                            audit_duration_seconds.to_string(),
                        ),
                        ("peers".to_string(), audit_peers.join(",")),
                        ("fault_id".to_string(), summary.id.to_string()),
                    ]),
                    message: format!("{msg} by principal {principal}"),
                },
            )
            .await;
            Json(serde_json::json!({ "message": msg, "fault": summary })).into_response()
        }
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
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return resp;
    }
    let (principal, role) = auth
        .as_deref()
        .map(|auth| (auth.principal_id.as_str(), auth.role))
        .unwrap_or(("local-bootstrap", crate::sesame::types::ApiRole::Admin));
    if let Err(error) = state.static_capabilities.test_policy.authorise_reversal(
        crate::testkit::safety::OperationPermission::AlterNodeState,
        &crate::testkit::safety::OperationAuthorisation {
            principal,
            role,
            acknowledged: false,
        },
    ) {
        return (StatusCode::FORBIDDEN, error.to_string()).into_response();
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
        Ok(Ok(msg)) => {
            record_fault_audit(
                &state,
                FaultAudit {
                    action: "fault.cleared-council-partition",
                    principal,
                    severity: crate::bun::events::EventSeverity::Info,
                    app: None,
                    node: None,
                    details: std::collections::BTreeMap::new(),
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
        crate::bun::BunError::Snapshot(SnapshotError::UnsupportedFilesystem { .. }) => {
            StatusCode::BAD_REQUEST
        }
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
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::SnapshotCreate {
            namespace,
            app_name: app,
            volume: body.volume,
            name: body.name,
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
        Ok(Ok(metas)) => (StatusCode::CREATED, Json(serde_json::json!(metas))).into_response(),
        Ok(Err(e)) => snapshot_error_response(&e),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
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
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::SnapshotList {
            namespace,
            app_name: app,
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
        Ok(Ok(metas)) => Json(serde_json::json!(metas)).into_response(),
        Ok(Err(e)) => snapshot_error_response(&e),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
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
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::SnapshotRestore {
            namespace,
            app_name: app,
            name: body.name,
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
        Ok(Ok(())) => Json(serde_json::json!({ "restored": true })).into_response(),
        Ok(Err(e)) => snapshot_error_response(&e),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent dropped response" })),
        )
            .into_response(),
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
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::SnapshotDelete {
            namespace,
            app_name: app,
            name,
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
        Ok(Ok(())) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Ok(Err(e)) => snapshot_error_response(&e),
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
    }

    // The caller controls the JSON body, so it cannot be the audit identity.
    // Token names are already authenticated by the middleware.
    request.injected_by = auth
        .as_deref()
        .map(|auth| auth.token_name.clone())
        .unwrap_or_else(|| "local-bootstrap".to_string());
    let audit_principal = auth
        .as_deref()
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
            Json(serde_json::json!(summary)).into_response()
        }
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

/// Resolve a live cluster member to one of its API URLs.
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
    let allow_node_fault = if let Some(target_node) =
        query.node.as_deref().filter(|target| !target.is_empty())
    {
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
            return forward_node_fault_clear(&state, target_node, &headers, id, query.acknowledged)
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
    let (resp_tx, resp_rx) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::ClearFault {
            fault_id: id,
            allow_workload_fault,
            allow_node_fault,
            allow_node_pressure,
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
        Ok(Ok(msg)) => {
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
            Json(serde_json::json!({ "message": msg })).into_response()
        }
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

/// Route manual reversal to the node which owns the local fault id.
async fn forward_node_fault_clear(
    state: &ApiState,
    target_node: &str,
    headers: &HeaderMap,
    fault_id: u64,
    acknowledged: bool,
) -> Response {
    let path = format!("/v1/fault/{fault_id}");
    let url = match target_node_api_url(state, target_node, &path).await {
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
    let (resp_tx, resp_rx) = oneshot::channel();
    // `?service=NAME` clears only that service's faults; no query clears all
    // workload faults. An *empty* `?service=` is neither: every node-class
    // fault carries an empty `target_service`, so it would match them all —
    // reject it rather than let this Deployer-authorised path reverse Admin
    // faults by omission.
    let command = match params.get("service") {
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
                AgentCommand::ClearFaultsByService {
                    service: service.clone(),
                    namespace: Some(namespace.clone()),
                    response: resp_tx,
                }
            }
            // Cross-namespace clear: reversing a service's faults in every
            // namespace is a cluster-wide action, so a scoped token is refused
            // and told to name a namespace it may touch (C3, as for reads).
            None => {
                if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
                    return response;
                }
                AgentCommand::ClearFaultsByService {
                    service: service.clone(),
                    namespace: None,
                    response: resp_tx,
                }
            }
        },
        None => AgentCommand::ClearAllFaults { response: resp_tx },
    };
    if state.cmd_tx.send(command).await.is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "agent unavailable" })),
        )
            .into_response();
    }

    match resp_rx.await {
        Ok(Ok(msg)) => {
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
    /// Restrict to one app's samples, matched against the `app` label
    /// (`namespace/app`). Set by the single-app cross-node fan-out so each
    /// node answers with only that app's local data; absent for node-wide
    /// dashboard queries.
    app: Option<String>,
}

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
        let app_filter = crate::mayo::store::escape_sql_literal(app);
        let sql = if name == "*" {
            format!(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE labels LIKE '%\"{app_filter}\"%' \
                 AND timestamp >= {start} AND timestamp <= {end} \
                 ORDER BY timestamp LIMIT 10000"
            )
        } else {
            let name = crate::mayo::store::escape_sql_literal(name);
            format!(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE metric_name = '{name}' \
                 AND labels LIKE '%\"{app_filter}\"%' \
                 AND timestamp >= {start} AND timestamp <= {end} \
                 ORDER BY timestamp LIMIT 10000"
            )
        };
        return match store.query_sql(&sql).await {
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

    let nodes = gather_dashboard_nodes(state).await;
    // The node count follows the real membership when we have it. A
    // standalone node with no gossip table still shows itself as one node.
    let node_count = if nodes.is_empty() { 1 } else { nodes.len() };

    DashboardData {
        cluster_name: String::new(),
        node_count,
        app_count: apps.len(),
        alert_count,
        apps,
        nodes,
        alerts,
    }
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
    let data = gather_dashboard_data(&state).await;
    html_response(render_dashboard(&data))
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
    let statuses = gather_statuses(&state).await;
    let apps = statuses_to_dashboard_apps(&statuses);
    html_response(fragments::render_apps_table_fragment(&apps))
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
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
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
/// Fans out to all council aggregators' `/v1/metrics/rollup` endpoints,
/// merges results (summing partial aggregates), and returns the combined data
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

    // Cluster path: fan out to every council aggregator's local rollup store
    // and sum. Reading only this member's store (as this endpoint used to)
    // undercounts, because each aggregator holds a different slice of workers.
    if let Some(urls) = resolve_council_urls(&state).await {
        let query = MetricsQuery {
            metric_name: params.name.clone(),
            start,
            end,
            app: None,
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
/// When the placement map is visible (council + membership), fans out to the
/// nodes running the app, hitting each one's app-filtered `/v1/metrics` leaf
/// and merge-sorting the per-instance rows. Falls back to the local metrics
/// store otherwise (single-node, or no placement info) — which is the same as
/// fanning out to just this node.
async fn metrics_app_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    let start = params.start.unwrap_or(0);
    // Clamped below u64::MAX: DataFusion 45's interval analysis
    // overflows (debug-build panic) computing the cardinality of a
    // full-domain unsigned range like `timestamp <= u64::MAX`.
    let end = params.end.unwrap_or(i64::MAX as u64).min(i64::MAX as u64);

    // Cross-node fan-out: each node keeps only its own instances' samples, so
    // reading just this node's store misses instances scheduled elsewhere.
    if let (Some(council), Some(membership)) = (&state.council, &state.membership) {
        use crate::meat::types::AppId;
        let desired = council.desired_state().await;
        let app_id = AppId::new(&app, &namespace);
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
                    metric_name: params.name.clone(),
                    start,
                    end,
                    // The leaf filters on the `app` label, stored as `namespace/app`.
                    app: Some(format!("{namespace}/{app}")),
                };
                let timeout = std::time::Duration::from_secs(10);
                let result = crate::mayo::query_fanout::fan_out_app_query(
                    &query,
                    &urls,
                    state.cluster_http.client(),
                    timeout,
                    state.service_token.as_deref(),
                )
                .await;
                return Json(result).into_response();
            }
        }
    }

    let Some(mayo) = &state.mayo else {
        return Json(MetricsQueryResult {
            data: vec![],
            warnings: vec![],
        })
        .into_response();
    };

    let store = mayo.read().await;

    // Filter by app label in the local store. Both the app/namespace path
    // segments and the caller-supplied `name` reach the SQL literal, so escape
    // every one (OBS1): without this a crafted `?name=x' OR '1'='1` or an app
    // name carrying a quote would break out of the literal and drop the
    // tenant/time predicate, leaking other apps' metrics.
    let app_filter = crate::mayo::store::escape_sql_literal(&format!("{namespace}/{app}"));
    let sql = match &params.name {
        Some(name) => {
            let name = crate::mayo::store::escape_sql_literal(name);
            format!(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE metric_name = '{name}' \
                 AND labels LIKE '%\"{app_filter}\"%' \
                 AND timestamp >= {start} AND timestamp <= {end} \
                 ORDER BY timestamp LIMIT 10000"
            )
        }
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
            let _ = tx.send(()).await;
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "message": "sync triggered" })),
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

/// Sign an image manifest digest and attach the signature via Raft.
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
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
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
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
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
    body: String,
) -> Response {
    // AUTH4: the highest-value lateral-movement target. A stolen service
    // token must not be able to mint fresh user tokens.
    if let Err(resp) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
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
    fn internal_trace_names_are_single_dns_labels() {
        for valid in ["api", "api-v2", "a1"] {
            assert!(valid_trace_label(valid), "rejected {valid:?}");
        }
        for invalid in ["", "API", "-api", "api-", "api.default", "api;id"] {
            assert!(!valid_trace_label(invalid), "accepted {invalid:?}");
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

    /// Build a router whose local `MayoStore` already holds `samples`
    /// (`(metric_name, app_filter, value)` where `app_filter` is the
    /// `namespace/app` label written under the `app` key). Used to drive the
    /// per-app metrics endpoint through the real HTTP route.
    async fn test_setup_with_metrics(
        samples: &[(&str, &str, f64)],
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
        for (name, app_filter, value) in samples {
            let mut labels = std::collections::BTreeMap::new();
            labels.insert("app".to_string(), app_filter.to_string());
            let key = MetricKey::with_labels(*name, labels);
            store.insert(&key, Sample::at(1000, *value));
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
            None,
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
    async fn external_trace_refuses_the_open_bootstrap_window() {
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
                    .uri("/v1/trace")
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
    async fn external_trace_needs_admin_policy_and_exact_destination() {
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
            post_status(app.clone(), "/v1/trace", &plaintext, &body(80)).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            post_status(app.clone(), "/v1/trace", &plaintext, &body(0)).await,
            StatusCode::BAD_REQUEST
        );
        // The exact allowlisted destination passes the policy boundary and
        // reaches the local-source check. No workload was seeded, hence 404.
        assert_eq!(
            post_status(app, "/v1/trace", &plaintext, &body(443)).await,
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
        serde_json::json!({
            "peers": ["node-b"],
            "duration_secs": 30,
            "acknowledged": acknowledged,
        })
        .to_string()
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
    async fn legacy_partition_route_does_not_bypass_the_admin_role() {
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
            post_status(
                app,
                "/v1/chaos/partition",
                &plaintext,
                &council_partition_body(true),
            )
            .await,
            StatusCode::FORBIDDEN
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn legacy_partition_route_requires_explicit_acknowledgement() {
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
            "/v1/chaos/partition",
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
    async fn legacy_partition_response_exposes_the_exact_owned_fault_id() {
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
            "/v1/chaos/partition",
            &plaintext,
            &council_partition_body(true),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let response: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(response["fault"]["id"].as_u64().is_some(), "{response}");
        assert_eq!(
            response["fault"]["fault_type"].as_str(),
            Some("council-partition")
        );
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
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
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
        let created = crate::sesame::token::create_token(
            "scoped",
            crate::sesame::types::ApiRole::Deployer,
            scope,
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
            },
            NodeMembershipInfo {
                node_id: crate::meat::NodeId::new("node-beta"),
                address: "127.0.0.1:9102".parse().unwrap(),
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
                        r#"{"token":"abc123","node_id":"node-02","csr_b64":""}"#,
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
                        r#"{"token":"whatever","node_id":"node-09","csr_b64":""}"#,
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

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
use crate::ketchup::follow::LogFrame;
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
mod secrets;
use secrets::{secret_public_key_handler, secret_rotate_handler};
mod identity;
use identity::{
    identity_jwks_handler, identity_sign_handler, join_token_create_handler, token_create_handler,
    token_list_handler, token_revoke_handler,
};
mod gitops;
use gitops::gitops_webhook_handler;
mod registry;
use registry::{
    images_handler, registry_proposal_deadline, registry_proposal_handler, registry_query_handler,
};
mod deploys;
use deploys::{
    cluster_deploy_history, deploy_cancel_handler, deploys_active_handler, deploys_history_handler,
    deploys_operations_handler, rollback_handler,
};
mod metrics;
use metrics::{
    alerts_handler, app_metric_rows, metrics_app_chart_handler, metrics_app_handler,
    metrics_cluster_handler, metrics_keys_handler, metrics_owned_rollup_handler,
    metrics_query_handler, metrics_rollup_handler, metrics_summary_handler,
};
mod ui;
use ui::{
    app_detail_handler, app_env_handler, dashboard_handler, fragment_alerts_handler,
    fragment_apps_handler, fragment_instances_handler, fragment_nodes_handler, gitops_handler,
    login_handler, node_detail_handler, ui_logout_handler, ui_session_handler,
};
mod logs;
use logs::{
    logs_cross_node_handler, logs_entries_handler, logs_export_handler, logs_handler,
    logs_sql_handler, ws_logs_handler,
};
mod discovery;
use discovery::{resolve_all_handler, resolve_handler, routes_handler};
mod faults;
pub use faults::ClusterFaultList;
use faults::{
    chaos_status_handler, fault_clear_all_handler, fault_clear_handler, fault_inject_handler,
    fault_list_handler, node_fault_fence_handler, node_fault_reserve_handler,
    spawn_node_fault_reaper,
};
mod snapshots;
use snapshots::{
    snapshot_create_handler, snapshot_delete_handler, snapshot_list_handler,
    snapshot_restore_handler,
};
mod join;
use join::{cluster_ca_handler, join_handler, node_renewal_handler};
mod nodes;
use nodes::{
    MAX_RELAY_REQUEST_BYTES, cluster_elect_handler, council_handler, node_relay_handler,
    nodes_handler,
};
mod apps;
use apps::{delete_handler, exec_handler, stop_handler};
mod status;
use status::{
    cluster_statuses, collect_cluster_statuses, current_apps_handler, events_handler, jobs_handler,
    status_app_handler, status_handler, top_handler, ws_events_handler,
};
mod internal;
use internal::{
    endpoint_withdrawal_receipt_handler, node_decommission_handler, placements_handler,
    producer_retirement_handler, refuse_retired_tls_peer, workload_csr_handler,
};

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

/// Every member this node has seen through gossip, with its last API address:
/// alive, suspect, and dead ones gossip no longer publishes.
///
/// [`ApiState::membership`] holds only live members, which is right for
/// fan-out and for injecting faults. A node-kill fault, though, closes a
/// node's cluster transports and leaves its management API open: gossip calls
/// it dead while it can still answer. The node relay and node-fault reversal
/// reach it through this table, so a caller outside the cluster network can
/// still inspect it and heal it, and the nodes listing reports it as dead
/// rather than dropping it. `bun` attaches it as a layer; without it the relay
/// reaches live members only.
///
/// Remembering is bounded. A member that left on purpose, or whose identity
/// the operator retired, is forgotten at once: nothing on it needs healing.
/// A member unheard of for [`KNOWN_MEMBER_RETENTION`] is forgotten too.
#[derive(Clone, Default)]
pub struct KnownMembers(Arc<RwLock<Vec<KnownMember>>>);

/// How long a down member's last address outlives the last time gossip heard
/// from it.
///
/// A dead node's address is kept so a fault injected into it can be cleared.
/// Injection needs a live target, and every fault expires within
/// [`crate::smoker::types::MAX_FAULT_DURATION_NS`] (24 hours) of injection. So
/// a node silent for longer holds no fault anyone could still clear, and its
/// entry only feeds a stale row to `relish nodes`. A day also spans an
/// overnight outage, so a node that died at 18:00 is still listed as dead the
/// next morning.
pub const KNOWN_MEMBER_RETENTION: std::time::Duration =
    std::time::Duration::from_nanos(crate::smoker::types::MAX_FAULT_DURATION_NS);

/// One member as gossip's roster reports it, with its API address resolved.
#[derive(Debug, Clone)]
pub struct RosterMember {
    /// The member's identity and API endpoint.
    pub info: NodeMembershipInfo,
    /// The member's gossip endpoint.
    pub gossip_address: std::net::SocketAddr,
    /// The member's SWIM state as this node last saw it.
    pub state: crate::mustard::state::NodeState,
    /// The member's SWIM incarnation.
    pub incarnation: u64,
    /// The member's placement labels.
    pub labels: std::collections::BTreeMap<String, String>,
}

/// A remembered member and when gossip last called it alive or suspect.
#[derive(Debug, Clone)]
struct KnownMember {
    member: RosterMember,
    last_heard: std::time::Instant,
}

impl KnownMembers {
    /// Take gossip's latest roster, remembering members it has reaped.
    ///
    /// Gossip reaps a dead member a minute after declaring it dead, while a
    /// node-kill fault on it may still need clearing. So a member missing from
    /// `roster` keeps its last address, as dead, rather than vanishing; a
    /// fresh entry for the same node replaces it. A remembered node that
    /// really has gone just fails to connect, which is the honest answer for
    /// a reversal aimed at it.
    ///
    /// Members gossip reports as Left, and those named in `retired`, are
    /// dropped, as is any down member not heard from since
    /// [`KNOWN_MEMBER_RETENTION`] before `now`.
    pub async fn refresh(
        &self,
        roster: Vec<RosterMember>,
        retired: &std::collections::BTreeSet<String>,
        now: std::time::Instant,
    ) {
        use crate::mustard::state::NodeState;
        let mut table = self.0.write().await;
        let previous: Vec<KnownMember> = table.drain(..).collect();
        let heard_before = |node_id: &crate::meat::NodeId| {
            previous
                .iter()
                .find(|known| &known.member.info.node_id == node_id)
                .map(|known| known.last_heard)
        };
        let mut next = Vec::new();
        for member in &roster {
            if member.state == NodeState::Left || retired.contains(&member.info.node_id.0) {
                continue;
            }
            let last_heard = if member.state.is_down() {
                heard_before(&member.info.node_id).unwrap_or(now)
            } else {
                now
            };
            next.push(KnownMember {
                member: member.clone(),
                last_heard,
            });
        }
        for known in &previous {
            let node_id = &known.member.info.node_id;
            if roster.iter().any(|member| &member.info.node_id == node_id)
                || retired.contains(&node_id.0)
            {
                continue;
            }
            // Gossip reaps only down members, and a Left one was forgotten
            // the moment the roster showed it, so a reaped member was dead.
            let mut known = known.clone();
            known.member.state = NodeState::Dead;
            next.push(known);
        }
        next.retain(|known| {
            !known.member.state.is_down()
                || now.saturating_duration_since(known.last_heard) <= KNOWN_MEMBER_RETENTION
        });
        *table = next;
    }

    /// The last API address of a member, live or down.
    pub async fn api_address(&self, node_id: &crate::meat::NodeId) -> Option<std::net::SocketAddr> {
        self.0
            .read()
            .await
            .iter()
            .find(|known| &known.member.info.node_id == node_id)
            .map(|known| known.member.info.address)
    }

    /// Remembered members that are down: dead to gossip, or reaped by it.
    pub async fn down(&self) -> Vec<RosterMember> {
        self.0
            .read()
            .await
            .iter()
            .filter(|known| known.member.state.is_down())
            .map(|known| known.member.clone())
            .collect()
    }
}

/// The gossip control-plane directory, which names the leader and its API
/// endpoint to every node, including workers outside Raft that have no
/// leader in their own metrics. `bun` attaches it as a layer; without it a
/// follower finds the leader through Raft alone.
#[derive(Clone)]
pub struct LeaderDirectory(
    pub tokio::sync::watch::Receiver<crate::mustard::directory::NodeDirectory>,
);

/// Shared state for API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub cmd_tx: mpsc::Sender<AgentCommand>,
    /// Answers status requests from the snapshot the agent loop publishes,
    /// without queueing for it. Production Bun always supplies one; routers
    /// built without an agent loop (small embedded and test routers) ask
    /// the loop through `cmd_tx` instead.
    pub status: Option<super::agent::StatusReader>,
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
    /// How this node runs image builds: the per-stage timeout, its own
    /// Buildah storage and the cache cap.
    pub build: super::build_runner::BuildSettings,
    /// Held for the whole Buildah part of a build (build, export, prune).
    /// The runner prunes Buildah storage after every build, which would race
    /// another build using the same storage, so builds on one node queue.
    pub build_lock: Arc<tokio::sync::Mutex<()>>,
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
        crate::bun::build_runner::BuildSettings::with_timeout(900),
        crate::cluster::ClusterHttp::plaintext(),
        5050,
        "http",
        256 * 1024 * 1024,
        false,
        crate::bun::capabilities::StaticCapabilities::default(),
        super::readiness::ReadinessTracker::new(),
        None,
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
    build: super::build_runner::BuildSettings,
    cluster_http: crate::cluster::ClusterHttp,
    registry_port: u16,
    registry_scheme: &'static str,
    max_context_bytes: u64,
    require_signatures: bool,
    static_capabilities: crate::bun::capabilities::StaticCapabilities,
    readiness: super::readiness::ReadinessTracker,
    local_test_leases: Option<crate::testkit::lease::LocalLeaseStore>,
    jwt_verifier: Option<crate::sesame::auth::WorkloadJwtVerifier>,
    status: Option<super::agent::StatusReader>,
) -> Router {
    let state = ApiState {
        cmd_tx,
        status,
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
        build,
        build_lock: Arc::new(tokio::sync::Mutex::new(())),
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
            "/v1/build/sign",
            post(super::build_runner::build_sign_handler),
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

/// Desired replicas, placements and any quota block for every app the
/// council knows, sorted by namespace and name.
fn council_app_evidence(
    desired: &crate::council::types::DesiredState,
    live_nodes: usize,
) -> Vec<crate::bun::diagnostics::DesiredAppEvidence> {
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
                blocked: desired.quota_blocked.get(app_id).cloned(),
            },
        )
        .collect::<Vec<_>>();
    apps.sort_by(|left, right| (&left.namespace, &left.app).cmp(&(&right.namespace, &right.app)));
    apps
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
        council_app_evidence(&desired, live_nodes)
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
        if let Err(response) =
            enforce_cluster_permission(&state, Some(auth), crate::config::PermissionAction::Admin)
                .await
        {
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
        if let Err(response) =
            enforce_cluster_permission(&state, Some(auth), crate::config::PermissionAction::Admin)
                .await
        {
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
            // The commit the running bytes were built from, so two builds
            // with the same version are told apart at a glance.
            "commit": crate::upgrade::version::build_commit(),
            "compatibility": crate::compatibility::CURRENT,
            "upgrade_in_flight": manager.upgrade_in_flight(),
            // Ids this node attempted and reverted — the orchestrator
            // reads these to detect node-side reverts.
            "failed_upgrade_ids": manager.reverted_upgrade_ids(),
            // The leader refuses a cluster upgrade up front when a node
            // reports false, rather than recording a run the node will
            // refuse and leaving it paused.
            "accepts_network_upgrades": manager.accepts_network_upgrades(),
            // What a rollback could return to. The leader refuses a
            // cluster rollback up front when a node lacks the target.
            "installed_versions": manager.installed_versions().await,
        })),
        None => Json(serde_json::json!({
            "version": crate::upgrade::version::compiled_version().to_string(),
            "commit": crate::upgrade::version::build_commit(),
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
///
/// A principal with a `[permission]` spec also needs `admin` granted across
/// the whole cluster (B18), so a spec can take cluster administration away
/// from an Admin token without revoking it.
// `Response` is large but it IS the HTTP reply to send on failure.
#[allow(clippy::result_large_err)]
async fn authorize_cluster_admin(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
) -> Result<(), Response> {
    crate::sesame::auth::authorize(auth, crate::sesame::types::ApiRole::Admin)?;
    crate::sesame::auth::require_unscoped(auth)?;
    enforce_cluster_permission(state, auth, crate::config::PermissionAction::Admin).await
}

/// Apply a node-level upgrade directive (admin). Responds 202 once the
/// binary is verified and staged; the process execs moments later.
async fn upgrade_apply_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
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
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
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

/// Marks an upgrade control call a follower has already forwarded, so two
/// nodes that disagree about the leader can't pass it back and forth.
const UPGRADE_FORWARDED_HEADER: &str = "x-reliaburger-upgrade-forwarded";

/// How long a follower waits for the leader to answer a forwarded upgrade
/// call. A start probes every node (five seconds each, concurrently) before
/// its Raft write, so the budget is well above that.
const UPGRADE_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Send an upgrade control call on to the leader when this node isn't it.
///
/// Only the leader can record a run, and openraft doesn't forward client
/// writes, so `relish upgrade start` against a follower used to fail with
/// "not leader". `None` means handle the call here: this node leads (and if
/// it has just lost that, its Raft write says so). The caller's own
/// credential travels with the request, so the leader repeats every
/// authorisation check; the follower never adds its service identity.
async fn forward_upgrade_to_leader(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    directory: Option<&LeaderDirectory>,
    path: &str,
    headers: &HeaderMap,
    body: &str,
) -> Option<Response> {
    let leads = {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        metrics.current_leader == Some(metrics.id)
    };
    if leads {
        return None;
    }
    let unavailable = |error: &str| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": error })),
        )
            .into_response()
    };
    if headers.contains_key(UPGRADE_FORWARDED_HEADER) {
        return Some(unavailable(
            "this node was named the leader but isn't; retry once the election settles",
        ));
    }
    let advertised = directory.and_then(|LeaderDirectory(directory)| {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        crate::cluster::directory::leader_api_address(&metrics, &directory.borrow())
    });
    let leader_url = match advertised {
        Some(address) => state.cluster_http.url(&address.to_string(), ""),
        None => match leader_api_url(state, council).await {
            Some(url) => url,
            None => return Some(unavailable("no cluster leader known yet; retry shortly")),
        },
    };
    let request = state
        .cluster_http
        .client()
        .post(format!("{leader_url}{path}"))
        .header(UPGRADE_FORWARDED_HEADER, "1")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(body.to_string());
    let request = copy_forwarded_auth(request, headers);
    let exchange = async {
        let response = request.send().await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .cloned();
        let body = response.bytes().await?;
        Ok::<_, reqwest::Error>((status, content_type, body))
    };
    let response = match tokio::time::timeout(UPGRADE_FORWARD_TIMEOUT, exchange).await {
        Ok(Ok((status, content_type, body))) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut response = (status, body).into_response();
            if let Some(content_type) = content_type
                && let Ok(value) = axum::http::HeaderValue::from_bytes(content_type.as_bytes())
            {
                response
                    .headers_mut()
                    .insert(axum::http::header::CONTENT_TYPE, value);
            }
            response
        }
        Ok(Err(error)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": format!("failed to forward the upgrade call to the leader: {error}")
            })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({ "error": "the leader did not answer the upgrade call in time" })),
        )
            .into_response(),
    };
    Some(response)
}

/// A node in a cluster upgrade start request.
#[derive(serde::Deserialize)]
struct StartUpgradeNode {
    node_id: String,
    /// The node's bun API address (`host:port`).
    address: String,
    role: crate::upgrade::types::NodeRole,
}

/// Refuse a run a two-voter council would hold in the council phase for
/// good: the orchestrator never takes a voter down without quorum to spare,
/// and a holding run isn't paused, so it couldn't be aborted either.
// `Response` is large but it IS the HTTP reply to send on failure.
#[allow(clippy::result_large_err)]
fn check_council_can_roll(council: &crate::council::CouncilNode) -> Result<(), Response> {
    let configured_voters = council
        .metrics()
        .borrow()
        .membership_config
        .membership()
        .voter_ids()
        .count();
    crate::upgrade::plan::check_council_can_roll(configured_voters).map_err(|e| {
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response()
    })
}

/// Start a cluster-wide rolling upgrade (admin, leader only).
///
/// The caller (relish) has already pushed the binary blob to the leader's
/// Pickle registry; this handler records the plan in Raft and the
/// orchestrator loop takes it from there.
async fn upgrade_start_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
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
    if let Some(forwarded) = forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        "/v1/upgrade/start",
        &headers,
        &body,
    )
    .await
    {
        return forwarded;
    }
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

    if let Err(resp) = check_council_can_roll(council) {
        return resp;
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
    // Check the directive every node will get here, before anything is
    // recorded: a candidate with other formats would otherwise pause the
    // run on the first node it reached.
    let directive = crate::upgrade::orchestrator::build_directive(&upgrade);
    if let Err(resp) = check_candidate_on_leader(&state, &directive).await {
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
            Json(serde_json::json!({ "status": "starting", "upgrade_id": upgrade_id })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("could not record the upgrade: {e}")
            })),
        )
            .into_response(),
    }
}

/// Ask every planned node what it runs and whether it can verify a
/// network upgrade, for the start-time gates.
async fn probe_running_binaries(
    state: &ApiState,
    nodes: &[crate::upgrade::types::NodeUpgradeRecord],
) -> (
    Vec<crate::upgrade::plan::RunningBinary>,
    Vec<crate::upgrade::plan::NetworkReadiness>,
) {
    probe_planned_nodes(state, nodes)
        .await
        .into_iter()
        .map(|(node, probe)| {
            (
                crate::upgrade::plan::RunningBinary {
                    node: node.clone(),
                    version: probe.version,
                    sha256: probe.binary_sha256,
                },
                crate::upgrade::plan::NetworkReadiness {
                    node,
                    accepts_network_upgrades: probe.accepts_network_upgrades,
                },
            )
        })
        .unzip()
}

/// Probe every planned node, named as an error names it (`node n1`).
///
/// Probes run concurrently, each bounded. An unreachable node is left out:
/// the orchestrator re-checks every node as the walk reaches it.
async fn probe_planned_nodes(
    state: &ApiState,
    nodes: &[crate::upgrade::types::NodeUpgradeRecord],
) -> Vec<(String, crate::upgrade::orchestrator::NodeProbe)> {
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
            Some((format!("node {}", record.node_id), probe))
        }
    });
    futures_util::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// How long the leader spends fetching, verifying and querying a
/// candidate before recording a run. It stays under the time a follower
/// waits for a forwarded start.
const CANDIDATE_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Ask the candidate for its formats on the leader, the way every node
/// will, so a release the cluster can't run is refused before it's
/// recorded rather than paused on the first node (#339).
///
/// Only a definite answer refuses: other formats, a bad signature, a blob
/// the registry doesn't hold. A registry that's down right now, or a check
/// that runs out of time, proves nothing about the candidate, so the run is
/// recorded and the nodes check it themselves, riding out the outage as
/// they always have. A leader without an upgrade manager can't check
/// either; the nodes still do.
// `Response` is large but it IS the HTTP reply to send on failure.
#[allow(clippy::result_large_err)]
async fn check_candidate_on_leader(
    state: &ApiState,
    directive: &crate::upgrade::types::UpgradeDirective,
) -> Result<(), Response> {
    let Some(manager) = &state.upgrade else {
        return Ok(());
    };
    let unchecked = |reason: String| {
        eprintln!(
            "bun: could not check the candidate {} before recording the upgrade ({reason}); \
             each node checks it when directed",
            directive.target_version
        );
        Ok(())
    };
    match tokio::time::timeout(CANDIDATE_CHECK_TIMEOUT, manager.check_candidate(directive)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) if e.is_transient() => unchecked(e.to_string()),
        Ok(Err(e)) => Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("refusing to upgrade to {}: {e}", directive.target_version)
            })),
        )
            .into_response()),
        Err(_) => unchecked(format!(
            "no answer within {}s",
            CANDIDATE_CHECK_TIMEOUT.as_secs()
        )),
    }
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
                    "error": format!("could not end the paused upgrade: {e}")
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
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council on this node" })),
        )
            .into_response();
    };
    if let Some(forwarded) = forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        "/v1/upgrade/resume",
        &headers,
        "",
    )
    .await
    {
        return forwarded;
    }
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
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council on this node" })),
        )
            .into_response();
    };
    if let Some(forwarded) = forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        "/v1/upgrade/abort",
        &headers,
        "",
    )
    .await
    {
        return forwarded;
    }
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
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
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
    if let Some(forwarded) = forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        "/v1/upgrade/cluster-rollback",
        &headers,
        &body,
    )
    .await
    {
        return forwarded;
    }
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

    // A rollback execs a binary each node already holds; nothing is
    // downloaded. Ask every node what its store holds and refuse here,
    // naming each node without the target, rather than record a run the
    // first such node refuses (#339).
    let stored: Vec<crate::upgrade::plan::StoredBinaries> =
        probe_planned_nodes(&state, &derived_nodes)
            .await
            .into_iter()
            .map(|(node, probe)| crate::upgrade::plan::StoredBinaries {
                node,
                running: probe.version,
                installed: probe.installed_versions,
            })
            .collect();
    if let Err(e) = crate::upgrade::plan::check_rollback_target(&request.target_version, &stored) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    if let Err(resp) = check_council_can_roll(council) {
        return resp;
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
    let permissions = permission_map(state).await;
    crate::sesame::auth::authorize_permission(auth, action, app, namespace, &permissions)
}

/// Enforce a principal's `[permission]` spec for a cluster-wide action: one
/// that names no single app, so only a grant for every app in every
/// namespace covers it. See [`crate::sesame::auth::authorize_cluster_permission`].
#[allow(clippy::result_large_err)]
async fn enforce_cluster_permission(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    action: crate::config::PermissionAction,
) -> Result<(), Response> {
    let permissions = permission_map(state).await;
    crate::sesame::auth::authorize_cluster_permission(auth, action, &permissions)
}

/// The replicated `[permission]` map, keyed by token name. Empty without a
/// council (single-node mode, where permissions can't be configured).
async fn permission_map(
    state: &ApiState,
) -> std::collections::BTreeMap<String, crate::config::PermissionSpec> {
    match &state.council {
        Some(council) => council.desired_state().await.permissions,
        None => std::collections::BTreeMap::new(),
    }
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
        if let Err(response) = enforce_cluster_permission(
            &state,
            auth.as_deref(),
            crate::config::PermissionAction::Admin,
        )
        .await
        {
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
    let permissions = permission_map(&state).await;
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

async fn local_statuses(state: &ApiState) -> Result<Vec<InstanceStatus>, String> {
    if let Some(reader) = &state.status {
        return reader.read().await.map_err(|error| error.to_string());
    }
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

/// Ask one member for `path` and decode its JSON answer, within `timeout`.
///
/// The request carries this node's service token, so the peer answers as it
/// would to the system principal; the caller trims the result to its own
/// caller's scope. The error names the member, ready to show as a warning.
async fn fetch_from_peer<T: serde::de::DeserializeOwned>(
    state: &ApiState,
    member: &NodeMembershipInfo,
    path: &str,
    timeout: std::time::Duration,
) -> Result<T, String> {
    let name = &member.node_id.0;
    let result = tokio::time::timeout(timeout, async {
        let url = state.cluster_http.url(&member.address.to_string(), path);
        let mut request = state.cluster_http.client().get(url);
        if let Some(token) = &state.service_token {
            request = request.bearer_auth(token);
        }
        request.send().await?.error_for_status()?.json::<T>().await
    })
    .await;
    match result {
        Ok(Ok(answer)) => Ok(answer),
        Ok(Err(error)) => Err(format!("node {name}: {error}")),
        Err(_) => Err(format!("node {name} timed out")),
    }
}

/// Ask every live member except this node for `path`.
///
/// Returns each member's answer beside its name, and one sorted warning per
/// member that failed or timed out. A cluster-wide view shows what it has
/// and names what's missing, rather than failing whole or going quiet.
async fn fan_out_to_peers<T: serde::de::DeserializeOwned>(
    state: &ApiState,
    path: &str,
    timeout: std::time::Duration,
) -> (Vec<(String, T)>, Vec<String>) {
    let local_name = local_node_name(state);
    let members = match &state.membership {
        Some(membership) => membership.read().await.clone(),
        None => Vec::new(),
    };
    let requests = futures_util::stream::iter(
        members
            .into_iter()
            .filter(|member| member.node_id.0 != local_name)
            .map(|member| async move {
                let answer = fetch_from_peer(state, &member, path, timeout).await;
                (member.node_id.0, answer)
            }),
    )
    .buffer_unordered(8);
    tokio::pin!(requests);
    let mut answers = Vec::new();
    let mut failures = Vec::new();
    while let Some((name, answer)) = requests.next().await {
        match answer {
            Ok(answer) => answers.push((name, answer)),
            Err(failure) => failures.push(failure),
        }
    }
    failures.sort();
    (answers, failures)
}

// ---------------------------------------------------------------------------
// Chaos testing endpoints
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Volume snapshots (Phase 12 E2)
// ---------------------------------------------------------------------------

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
        .api_address(&crate::meat::NodeId::new(target_node))
        .await;
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

// ---------------------------------------------------------------------------
// Metrics endpoints (Mayo)
// ---------------------------------------------------------------------------

/// Gather instance statuses from the agent.
async fn gather_statuses(state: &ApiState) -> Vec<InstanceStatus> {
    local_statuses(state).await.unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Deploy endpoints
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Identity endpoints
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Secret rotation endpoint
// ---------------------------------------------------------------------------

#[cfg(test)]
mod permission_tests;

#[cfg(test)]
mod cluster_view_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cluster_routing_tests;

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

// One module per route group. `router_with_upgrade` wires their handlers;
// the helpers at the bottom of this file are the ones several groups share.
mod apply;
mod apps;
mod ca;
mod deploys;
mod discovery;
mod faults;
mod gitops;
mod identity;
mod internal;
mod join;
mod logs;
mod metrics;
mod node_info;
mod nodes;
mod registry;
mod secrets;
mod snapshots;
mod status;
mod test_leases;
mod ui;
mod upgrade;

pub(crate) use apply::leader_api_url;
use apply::{apply_handler, cluster_apply};
use apps::{delete_handler, exec_handler, stop_handler};
use ca::{ca_rotation_begin_handler, ca_rotation_finalize_handler, ca_rotation_prepare_handler};
use deploys::{
    cluster_deploy_history, deploy_cancel_handler, deploys_active_handler, deploys_history_handler,
    deploys_operations_handler, rollback_handler,
};
use discovery::{resolve_all_handler, resolve_handler, routes_handler};
pub use faults::ClusterFaultList;
use faults::{
    chaos_status_handler, fault_clear_all_handler, fault_clear_handler, fault_inject_handler,
    fault_list_handler, node_fault_fence_handler, node_fault_reserve_handler,
    spawn_node_fault_reaper,
};
use gitops::gitops_webhook_handler;
use identity::{
    identity_jwks_handler, identity_sign_handler, join_token_create_handler, token_create_handler,
    token_list_handler, token_revoke_handler, token_rotate_handler,
};
use internal::{
    endpoint_withdrawal_receipt_handler, node_decommission_handler, placements_handler,
    producer_retirement_handler, refuse_retired_tls_peer, workload_csr_handler,
};
use join::{cluster_ca_handler, join_handler, node_renewal_handler, node_trust_ack_handler};
use logs::{
    logs_cross_node_handler, logs_entries_handler, logs_export_handler, logs_handler,
    logs_sql_handler, ws_logs_handler,
};
use metrics::{
    alerts_handler, app_metric_rows, metrics_app_chart_handler, metrics_app_handler,
    metrics_cluster_handler, metrics_keys_handler, metrics_owned_rollup_handler,
    metrics_query_handler, metrics_rollup_handler, metrics_summary_handler,
};
use node_info::{
    DesiredAppsSource, capabilities_handler, cluster_capabilities_handler, desired_apps_handler,
    diagnostics_handler, gather_desired_apps, health_handler, path_handler, readiness_handler,
    version_handler,
};
use nodes::{
    MAX_RELAY_REQUEST_BYTES, cluster_elect_handler, council_handler, node_relay_handler,
    nodes_handler,
};
use registry::{
    images_handler, registry_proposal_deadline, registry_proposal_handler, registry_query_handler,
};
use secrets::{secret_public_key_handler, secret_rotate_handler};
use snapshots::{
    snapshot_create_handler, snapshot_delete_handler, snapshot_list_handler,
    snapshot_restore_handler,
};
use status::{
    cluster_statuses, collect_cluster_statuses, current_apps_handler, events_handler, jobs_handler,
    status_app_handler, status_handler, top_handler, ws_events_handler,
};
use test_leases::{
    authenticated_test_user, find_test_lease, forward_test_lease_request, lease_error_response,
    test_lease_create_handler, test_lease_get_handler, test_lease_release_handler,
    test_lease_renew_handler, test_lease_retired_handler, test_operation_authorisation,
    write_lease_request,
};
use ui::{
    app_detail_handler, app_env_handler, dashboard_handler, fragment_alerts_handler,
    fragment_apps_handler, fragment_batches_handler, fragment_instances_handler,
    fragment_nodes_handler, gitops_handler, login_handler, node_detail_handler, ui_logout_handler,
    ui_session_handler,
};
use upgrade::{
    upgrade_abort_handler, upgrade_apply_handler, upgrade_cluster_handler,
    upgrade_cluster_rollback_handler, upgrade_resume_handler, upgrade_rollback_handler,
    upgrade_start_handler, upgrade_status_handler,
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
    /// When each API token last authenticated a request on this node, shared
    /// with the auth middleware that records it.
    pub token_last_used: crate::sesame::auth::TokenLastUsed,
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
    /// Task arrays: this node's executor, the standalone store and the
    /// leader's latest view of every node (0.2.0, million jobs).
    pub task_arrays: Arc<super::task_array_leader::TaskArrayService>,
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
    task_arrays: Option<Arc<super::task_array_leader::TaskArrayService>>,
) -> Router {
    let token_last_used = crate::sesame::auth::new_token_last_used();
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
        token_last_used: token_last_used.clone(),
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
        task_arrays: task_arrays
            .unwrap_or_else(|| Arc::new(super::task_array_leader::TaskArrayService::new(None))),
    };

    spawn_node_fault_reaper(state.clone());
    super::task_array_leader::spawn_leader_loop(state.clone());

    let mut auth_state = crate::sesame::auth::AuthState::new(
        token_store.unwrap_or_else(crate::sesame::auth::new_token_store),
        service_token,
    )
    .with_last_used(token_last_used);
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
        .route("/ui/fragment/batches", get(fragment_batches_handler))
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
            "/v1/cluster/trust-ack",
            post(node_trust_ack_handler).layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
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
        .route(
            "/v1/batch/summaries",
            get(super::task_array_api::summaries_handler),
        )
        .route("/v1/batch/run", post(super::batch::batch_run_handler))
        .route(
            "/v1/batch/{id}/report",
            post(super::batch::batch_report_handler),
        )
        .route("/v1/batch/{id}", get(super::batch::batch_status_handler))
        .route(
            "/v1/batch/manifest",
            post(super::task_array_api::manifest_handler),
        )
        .route(
            "/v1/batch/array",
            post(super::task_array_api::submit_handler),
        )
        .route(
            "/v1/batch/array/sync",
            post(super::task_array_api::sync_handler),
        )
        .route(
            "/v1/batch/array/{id}/local/results",
            get(super::task_array_api::local_results_handler),
        )
        .route(
            "/v1/batch/array/{id}/local/tasks/{index}/logs",
            get(super::task_array_api::local_logs_handler),
        )
        .route(
            "/v1/batch/{id}/cancel",
            post(super::task_array_api::cancel_handler),
        )
        .route(
            "/v1/batch/{id}/results",
            get(super::task_array_api::results_handler),
        )
        .route(
            "/v1/batch/{id}/tasks/{index}/logs",
            get(super::task_array_api::logs_handler),
        )
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
        .route("/v1/token/rotate", post(token_rotate_handler))
        .route("/v1/join-token/create", post(join_token_create_handler))
        .route("/v1/secret/public-key", get(secret_public_key_handler))
        .route("/v1/secret/rotate", post(secret_rotate_handler))
        .route("/v1/ca/rotation/prepare", post(ca_rotation_prepare_handler))
        .route("/v1/ca/rotation/begin", post(ca_rotation_begin_handler))
        .route(
            "/v1/ca/rotation/finalize",
            post(ca_rotation_finalize_handler),
        )
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

fn unavailable_response(error: String) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": error})),
    )
        .into_response()
}

fn system_time_millis(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Require a current quorum before a lease read or retirement instruction.
async fn confirmed_lease_leader(council: &crate::council::CouncilNode) -> bool {
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(3), council.is_leader()).await,
        Ok(true)
    )
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

/// New execution-ownership metadata must bound both queueing and the reply.
/// Keep the legacy helper's semantics for its existing consumers.
// The HTTP response crosses this helper directly to the calling route.
#[allow(clippy::result_large_err)]
pub(super) async fn ask_agent_bounded<T>(
    cmd_tx: &mpsc::Sender<AgentCommand>,
    build: impl FnOnce(oneshot::Sender<T>) -> AgentCommand,
) -> Result<T, Response> {
    match tokio::time::timeout(std::time::Duration::from_secs(5), ask_agent(cmd_tx, build)).await {
        Ok(Ok(value)) => Ok(value),
        _ => Err(agent_unavailable()),
    }
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
pub(crate) async fn permission_map(
    state: &ApiState,
) -> std::collections::BTreeMap<String, crate::config::PermissionSpec> {
    match &state.council {
        Some(council) => council.desired_state().await.permissions,
        None => std::collections::BTreeMap::new(),
    }
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

/// Record an audited action, attributed to the credential that asked for it
/// (F05 I1). The principal is the caller's stable `principal_id`, and the
/// token's name goes in the details for people; a node with no token store
/// yet (the bootstrap window) records `local-bootstrap`. Never pass a secret
/// in `details` or `message`.
pub(super) async fn record_caller_audit(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    kind: crate::bun::events::EventKind,
    action: &str,
    mut details: std::collections::BTreeMap<String, String>,
    message: String,
) {
    let Some(events) = &state.events else {
        return;
    };
    let (principal, token_name) = match auth {
        Some(auth) => (auth.principal_id.clone(), auth.token_name.clone()),
        None => ("local-bootstrap".to_string(), "local-bootstrap".to_string()),
    };
    details.insert("token_name".to_string(), token_name);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    events
        .write()
        .await
        .record_audit(crate::bun::events::AuditEvent {
            timestamp,
            kind,
            severity: crate::bun::events::EventSeverity::Info,
            action: action.to_string(),
            principal,
            app: None,
            namespace: None,
            node: Some(local_node_name(state)),
            details,
            message,
        });
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
pub(crate) fn copy_forwarded_auth(
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

/// Gather instance statuses from the agent.
async fn gather_statuses(state: &ApiState) -> Vec<InstanceStatus> {
    local_statuses(state).await.unwrap_or_default()
}

#[cfg(test)]
mod permission_tests;

#[cfg(test)]
mod cluster_view_tests;

#[cfg(test)]
mod token_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cluster_routing_tests;

#[cfg(test)]
mod binding_tests;

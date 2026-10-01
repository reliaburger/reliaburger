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

/// Ask this node to call a Raft election on itself (admin; manual
/// recovery tool — e.g. to move leadership off a node before maintenance).
async fn cluster_elect_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
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
    if let Err(response) =
        enforce_cluster_permission(&state, Some(auth), crate::config::PermissionAction::Admin).await
    {
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

/// Every node's workload statuses, plus one message per peer that didn't
/// answer. Only this node's own status failing is an error: callers decide
/// whether a partial cluster view is good enough.
async fn collect_cluster_statuses(
    state: &ApiState,
    peer_timeout: std::time::Duration,
) -> Result<(Vec<super::agent::ClusterInstanceStatus>, Vec<String>), String> {
    let local_name = local_node_name(state);
    let local = local_statuses(state).await?;
    let (peers, failures) =
        fan_out_to_peers::<Vec<InstanceStatus>>(state, "/v1/status", peer_timeout).await;
    let mut statuses: Vec<_> = std::iter::once((local_name, local))
        .chain(peers)
        .flat_map(|(node, instances)| {
            instances
                .into_iter()
                .map(move |instance| super::agent::ClusterInstanceStatus {
                    node: node.clone(),
                    instance,
                })
        })
        .collect();
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
    // CPU and memory are metrics, so a `[permission]` spec must grant
    // `metrics` on a row's app for the caller to see it (B18).
    let permissions = permission_map(&state).await;
    let visible = |row: &crate::bun::top::TopRow| {
        let (app, namespace) = (&row.instance.app_name, &row.instance.namespace);
        crate::sesame::auth::authorize_scoped(auth, app, namespace).is_ok()
            && crate::sesame::auth::authorize_permission(
                auth,
                crate::config::PermissionAction::Metrics,
                app,
                namespace,
                &permissions,
            )
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
    let (peers, warnings) =
        fan_out_to_peers::<Vec<crate::bun::top::TopRow>>(&state, "/v1/top", CLUSTER_STATUS_TIMEOUT)
            .await;
    rows.extend(peers.into_iter().flat_map(|(_, peer_rows)| peer_rows));
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
            // Missing samples leave the columns empty; they don't hide the
            // workloads themselves.
            match mayo
                .read()
                .await
                .query_names_since(&[CPU_METRIC, MEMORY_METRIC], since)
                .await
            {
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
///
/// `?cluster=true` merges every live member's jobs, each tagged with its
/// node, and names any member that didn't answer. Either way the rows are
/// trimmed to the caller's token scope.
async fn jobs_handler(
    State(state): State<ApiState>,
    Query(query): Query<StatusQuery>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    let auth = auth.as_deref();
    let visible = |job: &crate::bun::agent::JobStatus| {
        crate::sesame::auth::authorize_scoped(auth, &job.name, &job.namespace).is_ok()
    };
    let Ok(mut local) = ask_agent(&state.cmd_tx, |response| AgentCommand::JobStatus {
        response,
    })
    .await
    else {
        return agent_unavailable();
    };
    if !query.cluster {
        local.retain(visible);
        return Json(local).into_response();
    }
    let (peers, warnings) = fan_out_to_peers::<Vec<crate::bun::agent::JobStatus>>(
        &state,
        "/v1/jobs",
        CLUSTER_STATUS_TIMEOUT,
    )
    .await;
    let mut jobs = crate::bun::cluster_view::merge_jobs(
        std::iter::once((local_node_name(&state), local))
            .chain(peers)
            .collect(),
    );
    // Peers answered with the service token, which sees every namespace.
    jobs.retain(|job| visible(&job.row));
    Json(crate::bun::cluster_view::ClusterJobs { jobs, warnings }).into_response()
}

#[derive(Deserialize)]
struct EventsQuery {
    limit: Option<usize>,
    app: Option<String>,
    severity: Option<crate::bun::events::EventSeverity>,
    /// Answer from this node's store only. Set on the fan-out's own requests
    /// so a peer never fans out again.
    #[serde(default)]
    local: bool,
}

/// The request a peer gets for its share of `/v1/events`: the same filters,
/// answered from its own store.
//
// A plain function rather than inline in the handler because the URL
// serializer holds a non-`Send` reference; kept out of the async body it
// can't make the handler's future un-`Send`.
fn peer_events_path(query: &EventsQuery, limit: usize) -> String {
    let mut params = url::form_urlencoded::Serializer::new(String::new());
    params.append_pair("limit", &limit.to_string());
    params.append_pair("local", "true");
    if let Some(app) = &query.app {
        params.append_pair("app", app);
    }
    if let Some(severity) = query.severity
        && let Ok(serde_json::Value::String(severity)) = serde_json::to_value(severity)
    {
        params.append_pair("severity", &severity);
    }
    format!("/v1/events?{}", params.finish())
}

/// Return the newest events across the cluster, oldest first.
///
/// Each node keeps its own bounded store, so the node asked merges its own
/// with every live member's and names any member that didn't answer.
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
    let limit = query.limit.unwrap_or(100);
    let local = match &state.events {
        Some(events) => events
            .read()
            .await
            .recent(limit, query.app.as_deref(), query.severity),
        None => Vec::new(),
    };
    let mut answers = vec![(local_node_name(&state), local)];
    let mut warnings = Vec::new();
    if !query.local {
        let path = peer_events_path(&query, limit);
        let (peers, failures) = fan_out_to_peers::<crate::bun::cluster_view::ClusterEvents>(
            &state,
            &path,
            CLUSTER_STATUS_TIMEOUT,
        )
        .await;
        answers.extend(peers.into_iter().map(|(node, view)| (node, view.events)));
        warnings = failures;
    }
    Json(crate::bun::cluster_view::ClusterEvents {
        events: crate::bun::cluster_view::merge_events(answers, limit),
        warnings,
    })
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
    match local_statuses(&state).await.map_err(unavailable_response) {
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
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Logs,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }
    let follow = query.follow.unwrap_or(false);

    if follow {
        // A cluster member follows every node that runs the app; the
        // per-node streams it opens come back here with `local=true`.
        if !query.local.unwrap_or(false)
            && let Some(frames) = spawn_cluster_log_follow(&state, &app, &namespace, query.tail)
        {
            let stream = ReceiverStream::new(frames)
                .map(|frame| Ok::<_, std::convert::Infallible>(log_frame_event(frame)));
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

/// Start following an app on every node that runs it, when this node is a
/// cluster member; `None` on a standalone node, which follows itself.
///
/// The SSE and WebSocket endpoints both read the returned frames, so a
/// browser, `relish logs -f` and the TUI see the same merged stream.
fn spawn_cluster_log_follow(
    state: &ApiState,
    app: &str,
    namespace: &str,
    tail: Option<usize>,
) -> Option<mpsc::Receiver<LogFrame>> {
    let (Some(council), Some(membership), Some(self_name)) =
        (&state.council, &state.membership, &state.node_name)
    else {
        return None;
    };
    let (frames_tx, frames_rx) = mpsc::channel::<LogFrame>(256);
    tokio::spawn(follow_cluster_logs(
        state.clone(),
        Arc::clone(council),
        Arc::clone(membership),
        self_name.clone(),
        app.to_string(),
        namespace.to_string(),
        tail,
        frames_tx,
    ));
    Some(frames_rx)
}

/// One followed frame as an SSE event: a warning carries `event: warning`.
fn log_frame_event(frame: LogFrame) -> Event {
    match frame {
        LogFrame::Line(line) => Event::default().data(line),
        LogFrame::Warning(warning) => Event::default()
            .event(crate::ketchup::sse::WARNING_EVENT)
            .data(warning),
    }
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
/// [`LogFrame::Warning`] and the follow carries on with the rest. It returns
/// when the client disconnects.
#[allow(clippy::too_many_arguments)]
async fn follow_cluster_logs(
    state: ApiState,
    council: Arc<crate::council::CouncilNode>,
    membership: Arc<RwLock<Vec<NodeMembershipInfo>>>,
    self_name: String,
    app: String,
    namespace: String,
    tail: Option<usize>,
    events: mpsc::Sender<LogFrame>,
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

async fn send_log_warning(events: &mpsc::Sender<LogFrame>, warning: String) -> bool {
    events.send(LogFrame::Warning(warning)).await.is_ok()
}

/// Follow this node's own instances as one source of a cluster-wide follow.
async fn spawn_local_log_source(
    state: &ApiState,
    app: &str,
    namespace: &str,
    tail: Option<usize>,
    self_name: &str,
    events: mpsc::Sender<LogFrame>,
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
                if events.send(LogFrame::Line(line)).await.is_err() {
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
    events: mpsc::Sender<LogFrame>,
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
    events: &mpsc::Sender<LogFrame>,
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
            let forwarded = match event.event.as_deref() {
                Some(crate::ketchup::sse::WARNING_EVENT) => LogFrame::Warning(event.data),
                _ => LogFrame::Line(event.data),
            };
            if events.send(forwarded).await.is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Upgrade an authenticated request to a live log stream.
///
/// A cluster member follows every node that runs the app, exactly as the SSE
/// follow does; each text frame is one [`LogFrame`] as JSON.
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
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Logs,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }
    let frames = match spawn_cluster_log_follow(&state, &app, &namespace, query.tail) {
        Some(frames) => frames,
        None => match follow_local_logs(&state, app, namespace, query.tail, None).await {
            Ok(lines) => frame_local_lines(lines),
            Err(response) => return response,
        },
    };
    upgrade
        .on_upgrade(move |socket| ws_logs_session(socket, frames))
        .into_response()
}

/// Wrap a standalone node's own followed lines as [`LogFrame::Line`]s.
fn frame_local_lines(mut lines: mpsc::Receiver<String>) -> mpsc::Receiver<LogFrame> {
    let (frames_tx, frames_rx) = mpsc::channel(64);
    tokio::spawn(async move {
        while let Some(line) = lines.recv().await {
            if frames_tx.send(LogFrame::Line(line)).await.is_err() {
                return;
            }
        }
    });
    frames_rx
}

async fn ws_logs_session(mut socket: WebSocket, mut frames: mpsc::Receiver<LogFrame>) {
    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else { return };
                let Ok(json) = serde_json::to_string(&frame) else { continue };
                if socket.send(Message::Text(json.into())).await.is_err() {
                    return;
                }
            }
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
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Logs,
        &app,
        &namespace,
    )
    .await
    {
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
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Logs,
        &app,
        &namespace,
    )
    .await
    {
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
            .map(LogQueryWarning::from)
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
                // Each node that failed the fan-out becomes a warning that
                // keeps its cause, so the caller sees "wolf4 timed out after
                // 10s", not a silent empty or a bare "did not respond" (#282).
                warnings.extend(result.failures.into_iter().map(LogQueryWarning::from));
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

/// List cluster nodes: gossip's live members, then the ones this node
/// remembers as dead.
///
/// Gossip's live view drops a member the moment it is declared dead, and the
/// scheduler, council and Pickle rely on that. A listing is for people,
/// though, and a node that vanished is harder to act on than one marked
/// dead, so down members come from [`KnownMembers`] instead.
async fn nodes_handler(
    State(state): State<ApiState>,
    known: Option<axum::Extension<KnownMembers>>,
) -> Response {
    let down = match known {
        Some(known) => known
            .down()
            .await
            .into_iter()
            .map(|member| crate::bun::agent::NodeStatus {
                node_id: member.info.node_id.0.clone(),
                address: member.gossip_address.to_string(),
                api_address: member.info.api_advertised.then_some(member.info.address),
                state: member.state.to_string(),
                incarnation: member.incarnation,
                is_council: false,
                is_leader: false,
                labels: member.labels,
            })
            .collect(),
        None => Vec::new(),
    };
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Nodes {
        down,
        response,
    })
    .await
    {
        Ok(mut nodes) => {
            if let Some(membership) = &state.membership {
                let members = membership.read().await;
                // A down row already carries its last advertised address,
                // and the live table has none for it.
                for node in nodes.iter_mut().filter(|n| n.api_address.is_none()) {
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

/// The per-node reads `relish wtf`, `relish inspect`, `relish path` and
/// `relish test` make, and nothing else. The relay is a reachability aid, not a general proxy.
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
        "v1/version",
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
    // cannot widen to cluster-wide reads by exchanging itself for a cookie. It
    // also records which exact token it came from and that token's expiry
    // (B11), so the auth middleware can end it when the token is revoked or
    // lapses.
    let identity = if auth
        .service_token
        .as_deref()
        .is_some_and(|s| crate::sesame::auth::tokens_equal(&form.token, s))
    {
        // The operator presented the real service token; the session is
        // unconfined (but still read-only), matching the service principal.
        Some((
            crate::sesame::session::SessionIdentity {
                token_name: crate::sesame::auth::SYSTEM_PRINCIPAL.to_string(),
                principal_id: crate::sesame::auth::SYSTEM_PRINCIPAL.to_string(),
                scope: crate::sesame::types::TokenScope::default(),
            },
            None,
        ))
    } else {
        // Snapshot the tokens under the lock, then verify through the same
        // bounded path as a bearer (B12): a malformed token is refused by a
        // string check before any hashing, and a well-shaped one waits for a
        // permit from the process-wide Argon2 semaphore. This route is
        // unauthenticated, so calling Argon2 directly here would let anyone on
        // the network pin every core and exhaust memory with junk logins.
        let tokens = auth.tokens.read().await.clone();
        crate::sesame::auth::authenticate_off_lock(&form.token, tokens.clone())
            .await
            .ok()
            .map(|ctx| {
                let expires_at =
                    crate::sesame::auth::find_token_by_principal(&ctx.principal_id, &tokens)
                        .and_then(|token| token.expires_at);
                (
                    crate::sesame::session::SessionIdentity {
                        token_name: ctx.token_name,
                        principal_id: ctx.principal_id,
                        scope: crate::sesame::types::TokenScope {
                            apps: ctx.scoped_apps,
                            namespaces: ctx.scoped_namespaces,
                        },
                    },
                    expires_at,
                )
            })
    };

    let Some((identity, expires_at)) = identity else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::response::Html(crate::brioche::login::render_login(Some(
                "invalid or expired token",
            ))),
        )
            .into_response();
    };

    let session = auth.sessions.create(identity, expires_at).await;
    // The cookie lives no longer than the session, which lives no longer
    // than the token.
    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        crate::sesame::session::SESSION_COOKIE,
        session.id,
        session.lifetime.as_secs()
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
    /// Container mount path; required when several volumes share the name.
    volume: Option<String>,
}

#[derive(serde::Deserialize)]
struct SnapshotDeleteQuery {
    /// Container mount path; required when several volumes share the name.
    volume: Option<String>,
}

/// Map snapshot failures to honest status codes: a running app, an
/// ambiguous name or volumes another operation owns is a conflict, missing
/// things are 404, a non-btrfs volume or an out-of-scope input is the
/// client's problem, anything else is ours.
fn snapshot_error_response(error: &crate::bun::BunError) -> Response {
    use crate::grill::snapshot::SnapshotError;
    let status = match error {
        crate::bun::BunError::Snapshot(
            SnapshotError::AppRunning { .. }
            | SnapshotError::Ambiguous { .. }
            | SnapshotError::Busy { .. }
            | SnapshotError::RestoreInProgress { .. },
        ) => StatusCode::CONFLICT,
        crate::bun::BunError::Snapshot(
            SnapshotError::NotFound { .. } | SnapshotError::NoVolumes { .. },
        ) => StatusCode::NOT_FOUND,
        crate::bun::BunError::Snapshot(
            SnapshotError::UnsupportedFilesystem { .. }
            | SnapshotError::TestStorage
            | SnapshotError::InvalidInput(_),
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
        volume: body.volume,
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
    Query(query): Query<SnapshotDeleteQuery>,
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
        volume: query.volume,
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

/// Complete a forwarded node request with one deadline and a bounded body.
async fn send_node_request(
    target_node: &str,
    request: reqwest::RequestBuilder,
    operation: &str,
) -> Response {
    let deadline = tokio::time::Instant::now() + NODE_REQUEST_TIMEOUT;
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
    // One budget covers the agent's answer and the release wait, so a node
    // that forwarded this clear hears this node's own verdict, not its own
    // deadline passing.
    let deadline = tokio::time::Instant::now() + NODE_FAULT_CLEAR_BUDGET;
    let cleared = ask_agent(&state.cmd_tx, |response| AgentCommand::ClearFault {
        fault_id: id,
        allow_workload_fault,
        allow_node_fault,
        allow_node_pressure,
        response,
    });
    let Ok(cleared) = tokio::time::timeout_at(deadline, cleared).await else {
        // A clear already queued still runs; asking again is idempotent.
        return (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({
                "error": format!(
                    "the agent has not answered the clear of fault {id} yet; retry the clear"
                )
            })),
        )
            .into_response();
    };
    match cleared {
        Ok(Ok(clearance)) => {
            if let Some(sequence) = clearance.reservation
                && !wait_for_node_fault_release(&state, sequence, deadline).await
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

/// How long a node that forwards a node-level request waits for the owning
/// node's answer.
const NODE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a clear may spend on the owning node: the agent's answer plus the
/// wait for the council to release a node fault's reservation. It stays a
/// second under [`NODE_REQUEST_TIMEOUT`], so a forwarded clear reports this
/// node's own verdict rather than the forwarder's timeout.
const NODE_FAULT_CLEAR_BUDGET: std::time::Duration =
    NODE_REQUEST_TIMEOUT.saturating_sub(std::time::Duration::from_secs(1));

/// Wait until the council no longer holds the reservation a cleared node fault
/// owned, or `deadline` passes. Returns whether it was released.
///
/// The leader's reaper releases a reservation only after it has fenced the
/// target node through its own live membership view. So once this returns
/// `true`, the leader that will judge the next node fault has already seen
/// this node back, and the single experiment slot is free again.
async fn wait_for_node_fault_release(
    state: &ApiState,
    sequence: u64,
    deadline: tokio::time::Instant,
) -> bool {
    let Some(council) = &state.council else {
        return true;
    };
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
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
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
        match store.query_all(start, end).await {
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
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
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
    local_statuses(state).await.unwrap_or_default()
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
    for app in desired.iter().filter(|app| app.blocked.is_some()) {
        if let Some(row) = rows.get_mut(&(app.namespace.clone(), app.app.clone()))
            && row.state == "pending"
        {
            row.state = "blocked".into();
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

    let alerts = firing_dashboard_alerts(state).await;
    let alert_count = alerts.len();

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
///
/// The alert panel follows `/v1/alerts`: a principal whose `[permission]`
/// spec doesn't grant `metrics` across the cluster sees the page without it.
async fn dashboard_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    let mut data = match gather_dashboard_data(&state).await {
        Ok(data) => data,
        Err(error) => return unavailable_response(error),
    };
    if enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    .is_err()
    {
        data.alerts.clear();
        data.alert_count = 0;
    }
    html_response(render_dashboard(&data))
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
    let blocked = desired
        .iter()
        .find(|evidence| evidence.app == app && evidence.namespace == namespace)
        .and_then(|evidence| evidence.blocked.as_ref())
        .map(ToString::to_string);

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

    let deploy_history = cluster_deploy_history(&state, &app, &namespace, false).await;

    // Each chart polls the app's metric endpoint, which refuses a principal
    // without `metrics` on this app; leave them out rather than draw errors.
    let charts = match enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
        &app,
        &namespace,
    )
    .await
    {
        Ok(()) => crate::brioche::app_detail::app_charts(
            &app,
            &namespace,
            &scraped_metric_names(&state, &app, &namespace).await,
        ),
        Err(_) => Vec::new(),
    };

    let data = AppDetailData {
        app_name: app,
        namespace,
        state: overall_state,
        blocked,
        desired_instances,
        instances,
        env,
        deploy_history: deploy_history.history,
        history_warnings: deploy_history.warnings,
        charts,
    };

    html_response(render_app_detail(&data))
}

/// `GET /ui/node/{name}` — node detail page.
///
/// Any node's page lists that node's workloads: this node answers from its
/// own status, another member is asked for its status directly, and a
/// member that doesn't answer is named on the page rather than shown empty.
async fn node_detail_handler(State(state): State<ApiState>, Path(name): Path<String>) -> Response {
    let is_self = local_node_name(&state) == name;
    let member = match &state.membership {
        Some(m) => m
            .read()
            .await
            .iter()
            .find(|info| info.node_id.0 == name)
            .cloned(),
        None => None,
    };
    let node_state = if is_self || member.is_some() {
        "alive"
    } else {
        "unknown"
    }
    .to_string();
    let (statuses, warning) = match (is_self, &member) {
        (true, _) => (gather_statuses(&state).await, None),
        (false, Some(member)) => {
            match fetch_from_peer::<Vec<InstanceStatus>>(
                &state,
                member,
                "/v1/status",
                CLUSTER_STATUS_TIMEOUT,
            )
            .await
            {
                Ok(statuses) => (statuses, None),
                Err(error) => (Vec::new(), Some(format!("did not answer: {error}"))),
            }
        }
        (false, None) => (Vec::new(), None),
    };
    // The charts read this node's metrics store, so drawing them on another
    // node's page would label this node's CPU and memory as that node's.
    let charts = if is_self { node_charts() } else { Vec::new() };

    let data = NodeDetailData {
        name,
        state: node_state,
        app_count: statuses.len(),
        apps: statuses,
        warning,
        charts,
    };

    html_response(render_node_detail(&data))
}

/// CPU and memory charts for this node's own page.
fn node_charts() -> Vec<ChartConfig> {
    vec![
        ChartConfig {
            endpoint: "/v1/metrics?name=node_cpu_usage_percent".to_string(),
            title: "CPU Usage".to_string(),
            unit: crate::brioche::units::ChartUnit::Percent,
            refresh_secs: 10,
            range_secs: 3600,
        },
        ChartConfig {
            endpoint: "/v1/metrics?name=node_memory_used_bytes".to_string(),
            title: "Memory Usage".to_string(),
            unit: crate::brioche::units::ChartUnit::Bytes,
            refresh_secs: 10,
            range_secs: 3600,
        },
    ]
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
async fn fragment_alerts_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
        return resp;
    }
    let alerts = firing_dashboard_alerts(&state).await;
    html_response(fragments::render_alerts_table_fragment(&alerts))
}

/// The firing alerts, shaped for the dashboard's alert table.
async fn firing_dashboard_alerts(
    state: &ApiState,
) -> Vec<crate::brioche::dashboard::DashboardAlert> {
    let Some(evaluator) = &state.alerts else {
        return Vec::new();
    };
    evaluator
        .read()
        .await
        .firing_alerts()
        .iter()
        .map(|a| crate::brioche::dashboard::DashboardAlert {
            labels: a.labels.clone(),
            name: a.rule_name.clone(),
            severity: format!("{:?}", a.severity),
            description: a.description.clone(),
        })
        .collect()
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
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Logs,
    )
    .await
    {
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
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Admin,
    )
    .await
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
///
/// Alerts are rules evaluated over the whole metric store, so a principal
/// with a `[permission]` spec needs `metrics` across the cluster (B18).
async fn alerts_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
        return resp;
    }
    let Some(alerts) = &state.alerts else {
        return Json(crate::mayo::alert::AlertsResponse { alerts: Vec::new() }).into_response();
    };
    let evaluator = alerts.read().await;
    Json(crate::mayo::alert::AlertsResponse {
        alerts: evaluator.all_statuses(),
    })
    .into_response()
}

/// `GET /v1/metrics/keys` — list all distinct metric names.
async fn metrics_keys_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
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
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
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
        None => store.query_all(start, end).await,
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
    if let Err(response) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
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
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
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
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
        &app,
        &namespace,
    )
    .await
    {
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
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
        &app,
        &namespace,
    )
    .await
    {
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

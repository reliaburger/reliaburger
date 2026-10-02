#[tokio::test]
async fn registry_query_response_refuses_an_oversized_catalogue() {
    use crate::pickle::authority::{MAX_REGISTRY_PROPOSAL_BYTES, RegistryQueryResponse};
    let mut catalog = crate::pickle::types::ManifestCatalog::default();
    catalog
        .repository_owners
        .insert("repository".into(), "x".repeat(MAX_REGISTRY_PROPOSAL_BYTES));
    assert_eq!(
        super::registry::bounded_registry_query_response(RegistryQueryResponse::Repository(
            Box::new(catalog)
        ))
        .await
        .status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        super::registry::bounded_registry_query_response(RegistryQueryResponse::Repository(
            Default::default()
        ))
        .await
        .status(),
        axum::http::StatusCode::OK
    );
}

use super::faults::NODE_REQUEST_TIMEOUT;
use super::node_info::{council_app_evidence, filter_desired_apps_for_scope, valid_path_label};
use super::ui::statuses_to_dashboard_apps;
use super::upgrade::{UPGRADE_FORWARDED_HEADER, check_council_can_roll};
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
        blocked: None,
        volume_home_away: None,
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
pub(super) async fn seeded_council(tag: &str) -> Arc<crate::council::CouncilNode> {
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
        crate::bun::build_runner::BuildSettings::with_timeout(900),
        crate::cluster::ClusterHttp::plaintext(),
        5050,
        "http",
        256 * 1024 * 1024,
        false,
        static_capabilities,
        readiness,
        local_test_leases,
        None,
        None,
    );
    (app, shutdown)
}

fn a_user_token(role: crate::sesame::types::ApiRole) -> (crate::sesame::types::ApiToken, String) {
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
        delete_authenticated(app, "/v1/fault/1?node=node-a&acknowledged=true", &plaintext,).await,
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
        delete_authenticated(app, "/v1/fault/1?node=node-a&acknowledged=true", &plaintext,).await,
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
    let faults: Vec<crate::smoker::types::FaultSummary> = serde_json::from_slice(&body).unwrap();
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

/// A node forwarding a clear gives the owning node [`NODE_REQUEST_TIMEOUT`].
/// When the owning agent is busy, the owning node must still answer inside
/// that, with its own retryable 504, instead of letting the forwarder's
/// deadline pass first.
#[tokio::test(start_paused = true)]
async fn a_clear_answers_within_its_budget_when_the_agent_is_busy() {
    // An agent that never gets round to the command.
    let (cmd_tx, _cmd_rx) = mpsc::channel(4);
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
        None,
        None,
        9117,
        None,
        None,
        None,
        "default".to_string(),
        Some("node-2".to_string()),
        crate::bun::build_runner::BuildSettings::with_timeout(900),
        crate::cluster::ClusterHttp::plaintext(),
        5050,
        "http",
        256 * 1024 * 1024,
        false,
        workload_fault_static_capabilities(),
        crate::bun::readiness::ReadinessTracker::new(),
        None,
        None,
        None,
    );
    let started = tokio::time::Instant::now();
    let response = tokio::time::timeout(
        NODE_REQUEST_TIMEOUT * 2,
        app.oneshot(
            axum::http::Request::builder()
                .method("DELETE")
                .uri("/v1/fault/7")
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .expect("the clear never answered")
    .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(started.elapsed() < NODE_REQUEST_TIMEOUT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("retry the clear"),
        "{}",
        String::from_utf8_lossy(&body)
    );
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
    let (other, other_text) = named_user_token("other", crate::sesame::types::ApiRole::Deployer);
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
    use crate::council::{CouncilNode, log_store::MemLogStore, state_machine::CouncilStateMachine};
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

/// One request the fake leader received: path, bearer, loop marker, body.
type SeenAtLeader = (String, Option<String>, bool, String);

/// Serve a fake leader API on `listener` that accepts every request and
/// records what it saw.
fn serve_recording_leader(
    listener: tokio::net::TcpListener,
) -> Arc<tokio::sync::Mutex<Vec<SeenAtLeader>>> {
    let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let leader = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let recorder = recorder.clone();
        async move {
            let path = request.uri().path().to_string();
            let bearer = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(String::from);
            let looped = request.headers().contains_key(UPGRADE_FORWARDED_HEADER);
            let body = request.into_body().collect().await.unwrap().to_bytes();
            let body = String::from_utf8_lossy(&body).to_string();
            recorder.lock().await.push((path, bearer, looped, body));
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "status": "recorded by the leader" })),
            )
        }
    });
    tokio::spawn(async move { axum::serve(listener, leader).await.unwrap() });
    seen
}

/// An in-memory council with `voters` voters, led by node 1.
async fn council_of(voters: u64) -> Vec<Arc<crate::council::CouncilNode>> {
    use crate::council::CouncilNode;
    use crate::council::log_store::MemLogStore;
    use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
    use crate::council::state_machine::CouncilStateMachine;
    use crate::council::types::{CouncilConfig, CouncilNodeInfo};

    let network = InMemoryRaftRouter::new();
    let mut nodes = Vec::new();
    let mut members = std::collections::BTreeMap::new();
    for id in 1..=voters {
        members.insert(
            id,
            CouncilNodeInfo {
                addr: std::net::SocketAddr::from(([127, 0, 0, 1], 7000 + id as u16)),
                name: format!("node-{id}"),
            },
        );
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
        while nodes[0].current_leader().await.is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    nodes
}

/// The appliance lab's two-node cluster (#259): start and a cluster
/// rollback read the council's configured voters from Raft, and two of
/// them can never roll, so both handlers refuse with a 409.
#[tokio::test]
async fn a_two_voter_council_refuses_to_roll_and_three_voters_can() {
    let pair = council_of(2).await;
    let refusal = check_council_can_roll(&pair[0]).expect_err("two voters must be refused");
    assert_eq!(refusal.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(refusal.into_body(), 4096)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("council of 2 voters") && body.contains("at least three voters"),
        "{body}"
    );
    for node in pair {
        node.shutdown().await.unwrap();
    }

    let trio = council_of(3).await;
    assert!(check_council_can_roll(&trio[0]).is_ok());
    for node in trio {
        node.shutdown().await.unwrap();
    }
}

/// A three-node council led by node 1, whose API is a fake that records
/// every request and accepts it. Returns node 2's real router (a
/// follower) with `token` in its store, what the leader has seen, and
/// the council nodes to shut down.
async fn follower_of_a_recording_leader(
    token: crate::sesame::types::ApiToken,
) -> (
    Router,
    Arc<tokio::sync::Mutex<Vec<SeenAtLeader>>>,
    Vec<Arc<crate::council::CouncilNode>>,
) {
    use crate::council::CouncilNode;
    use crate::council::log_store::MemLogStore;
    use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
    use crate::council::state_machine::CouncilStateMachine;
    use crate::council::types::{CouncilConfig, CouncilNodeInfo};

    let leader_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let leader_port = leader_listener.local_addr().unwrap().port();
    let network = InMemoryRaftRouter::new();
    let mut nodes = Vec::new();
    let mut members = std::collections::BTreeMap::new();
    for id in 1..=3 {
        // Without gossip, a follower finds the leader's API at its Raft
        // IP and the cluster's API port.
        members.insert(
            id,
            CouncilNodeInfo {
                addr: std::net::SocketAddr::from(([127, 0, 0, 1], 7000 + id as u16)),
                name: format!("node-{id}"),
            },
        );
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
        while nodes[1].current_leader().await != Some(1) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let seen = serve_recording_leader(leader_listener);

    let (commands, _receiver) = mpsc::channel(4);
    let store = crate::sesame::auth::new_token_store();
    *store.write().await = vec![token];
    let follower = router(
        commands,
        None,
        None,
        None,
        None,
        None,
        Some(nodes[1].clone()),
        Some(store),
        Some("internal".into()),
        None,
        None,
        None,
        leader_port,
        None,
    );
    (follower, seen, nodes)
}

#[tokio::test]
async fn a_follower_forwards_upgrade_control_calls_to_the_leader_with_the_callers_token() {
    let (admin, admin_key) = a_user_token(crate::sesame::types::ApiRole::Admin);
    let (follower, seen, nodes) = follower_of_a_recording_leader(admin).await;

    let calls = [
        ("/v1/upgrade/start", r#"{"target_version":"v0.2.0"}"#),
        ("/v1/upgrade/resume", ""),
        ("/v1/upgrade/abort", ""),
        (
            "/v1/upgrade/cluster-rollback",
            r#"{"target_version":"v0.1.0"}"#,
        ),
    ];
    for (path, body) in calls {
        let (status, reply) =
            post_authenticated(follower.clone(), path, &admin_key, body, None).await;
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "{path}: {}",
            String::from_utf8_lossy(&reply)
        );
        assert!(
            String::from_utf8_lossy(&reply).contains("recorded by the leader"),
            "{path} was answered by the follower"
        );
    }
    let seen = seen.lock().await.clone();
    let expected: Vec<SeenAtLeader> = calls
        .iter()
        .map(|(path, body)| {
            (
                path.to_string(),
                Some(format!("Bearer {admin_key}")),
                true,
                body.to_string(),
            )
        })
        .collect();
    assert_eq!(seen, expected);
    for node in nodes {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn a_follower_checks_upgrade_authority_and_never_forwards_twice() {
    let (reader, reader_key) = a_user_token(crate::sesame::types::ApiRole::ReadOnly);
    let (follower, seen, nodes) = follower_of_a_recording_leader(reader).await;

    let (status, _) =
        post_authenticated(follower.clone(), "/v1/upgrade/abort", &reader_key, "", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A request another node already forwarded, arriving at a node that
    // isn't the leader either: the two disagree about who leads.
    let looped = follower
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/upgrade/abort")
                .header("authorization", "Bearer internal")
                .header(UPGRADE_FORWARDED_HEADER, "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(looped.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(seen.lock().await.is_empty(), "nothing reaches the leader");
    for node in nodes {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn a_worker_outside_raft_forwards_upgrade_calls_to_the_leader_gossip_names() {
    use crate::council::CouncilNode;
    use crate::council::log_store::MemLogStore;
    use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
    use crate::council::state_machine::CouncilStateMachine;
    use crate::council::types::CouncilConfig;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let leader_api = listener.local_addr().unwrap();
    let seen = serve_recording_leader(listener);
    // Never initialised and never added: its own Raft knows no leader.
    let worker = Arc::new(
        CouncilNode::new(
            9,
            CouncilConfig::default(),
            InMemoryRaftNetworkFactory::new(9, InMemoryRaftRouter::new()),
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap(),
    );
    let (_directory_tx, directory_rx) =
        tokio::sync::watch::channel(crate::mustard::directory::NodeDirectory {
            leader: Some(crate::mustard::message::LeaderHint {
                node_id: crate::meat::NodeId::new("node-1"),
                term: 1,
                api_address: leader_api,
                reporting_address: leader_api,
            }),
            ..Default::default()
        });
    let (admin, admin_key) = a_user_token(crate::sesame::types::ApiRole::Admin);
    let (commands, _receiver) = mpsc::channel(4);
    let store = crate::sesame::auth::new_token_store();
    *store.write().await = vec![admin];
    let router = |directory: Option<LeaderDirectory>| {
        let app = router(
            commands.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(worker.clone()),
            Some(store.clone()),
            Some("internal".into()),
            None,
            None,
            None,
            // No Raft leader to take an address from: only the
            // directory knows where the leader's API is.
            1,
            None,
        );
        match directory {
            Some(directory) => app.layer(axum::Extension(directory)),
            None => app,
        }
    };

    let (status, _) =
        post_authenticated(router(None), "/v1/upgrade/abort", &admin_key, "", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, reply) = post_authenticated(
        router(Some(LeaderDirectory(directory_rx))),
        "/v1/upgrade/abort",
        &admin_key,
        "",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "{}",
        String::from_utf8_lossy(&reply)
    );
    let seen = seen.lock().await.clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "/v1/upgrade/abort");
    assert_eq!(seen[0].1, Some(format!("Bearer {admin_key}")));
    worker.shutdown().await.unwrap();
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
            crate::bun::build_runner::BuildSettings::with_timeout(900),
            crate::cluster::ClusterHttp::plaintext(),
            5050,
            "http",
            256 * 1024 * 1024,
            false,
            lease_static_capabilities(),
            crate::bun::readiness::ReadinessTracker::new(),
            None,
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
    let (mut scoped, scoped_key) = named_user_token("scoped", crate::sesame::types::ApiRole::Admin);
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
async fn credential_free_placements_serve_discovery_but_a_service_token_requires_authentication() {
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
    let (status, bytes) = get_authenticated(app, "/v1/placements/late-worker", "internal").await;
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

    let (ungranted_token, ungranted_plaintext) = a_user_token(crate::sesame::types::ApiRole::Admin);
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
    let (app, shutdown) = setup_with_auth_and_readiness(vec![token], None, readiness.clone()).await;
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
    let status = post_status(app, "/v1/join-token/create", &tok, r#"{"ttl_seconds":900}"#).await;
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
    let status = post_status(app, "/v1/join-token/create", &tok, r#"{"ttl_seconds":900}"#).await;
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
        let encrypted =
            crate::sesame::secret::encrypt_secret("probe", json["public_key"].as_str().unwrap())
                .unwrap();
        let identity =
            crate::sesame::secret::unwrap_age_identity(keypair, council.wrapping_ikm().unwrap())
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
        let status = post_status(app, "/v1/deploys/operations/unknown/cancel", &token, "").await;
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
    let (app, council, mut commands) = workload_admission_fixture("leased-image-admission").await;
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
        crate::bun::build_runner::BuildSettings::with_timeout(900),
        crate::cluster::ClusterHttp::plaintext(),
        5050,
        "http",
        256 * 1024 * 1024,
        false,
        statics,
        crate::bun::readiness::ReadinessTracker::new(),
        None,
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

/// F07 part 2: `grep` is a regular expression. One that won't compile is the
/// caller's mistake, so both query routes answer 400 with the reason, not a
/// 500 that reads like a broken store.
#[tokio::test]
async fn an_invalid_grep_pattern_is_a_bad_request() {
    let (cmd_tx, _cmd_rx) = mpsc::channel(32);
    let store_dir = tempfile::tempdir().unwrap();
    let store = crate::ketchup::log_store::LogStore::new(store_dir.path().to_path_buf());
    let app = router(
        cmd_tx,
        None,
        Some(Arc::new(RwLock::new(store))),
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
    for uri in [
        "/v1/logs/entries/web/default?grep=%28unclosed",
        "/v1/logs/query/web/default?grep=%28unclosed",
    ] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("regular expression"), "{uri}: {body}");
    }
}

/// `stream` is `stdout` or `stderr`; anything else is refused on both query
/// routes, and following with a stream filter is refused too, since the
/// raw tail can't honour it.
#[tokio::test]
async fn an_unknown_stream_or_a_followed_stream_filter_is_a_bad_request() {
    let (cmd_tx, _cmd_rx) = mpsc::channel(32);
    let store_dir = tempfile::tempdir().unwrap();
    let store = crate::ketchup::log_store::LogStore::new(store_dir.path().to_path_buf());
    let app = router(
        cmd_tx,
        None,
        Some(Arc::new(RwLock::new(store))),
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
    for uri in [
        "/v1/logs/entries/web/default?stream=stdin",
        "/v1/logs/query/web/default?stream=stdin",
        "/v1/logs/web/default?follow=true&stream=stderr",
    ] {
        let status = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
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
        cmd_tx, None, log_store, None, None, None, None, None, None, None, None, None, 9117, None,
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

/// B01: a Deployer scoped to `a/web` passes the route's scope check,
/// so the snapshot inputs themselves must not reach outside `a/web`.
/// A traversal volume used to snapshot `b/db`'s volume, and an
/// absolute or `..` name placed a root-owned subvolume anywhere.
#[tokio::test]
async fn scoped_deployer_cannot_escape_its_app_through_snapshot_inputs() {
    let volumes_dir = tempfile::tempdir().unwrap();
    let volumes = crate::grill::volume::VolumeManager::new(volumes_dir.path());
    for (namespace, app) in [("a", "web"), ("b", "db")] {
        volumes
            .create_managed_volume(namespace, app, std::path::Path::new("/data"), None)
            .unwrap();
    }
    let tree = |root: &std::path::Path| {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                }
                out.push(entry.path());
            }
        }
        out.sort();
        out
    };
    let before = tree(volumes_dir.path());

    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        MockGrill::new(),
        PortAllocator::new(30000, 31000),
        cmd_rx,
        shutdown.clone(),
    );
    agent.set_volumes_dir(volumes_dir.path().to_path_buf());
    tokio::spawn(async move {
        agent.run().await;
    });
    let scope = crate::sesame::types::TokenScope {
        apps: Some(vec!["web".to_string()]),
        namespaces: Some(vec!["a".to_string()]),
    };
    let created = crate::sesame::token::create_token(
        "a-web",
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
        None,
        Some(store),
        None,
        None,
        None,
        None,
        9117,
        None,
    );
    let tok = created.plaintext;

    // Refused as invalid input: on a non-btrfs tempdir a merely
    // "unsupported filesystem" 400 would hide that validation never ran.
    let send = |method: &'static str, uri: &'static str, body: String| {
        let request = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {tok}"))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let app = app.clone();
        async move {
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };
    let refused = |(status, body): (StatusCode, String), what: &str| {
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {body}");
        assert!(
            body.contains("invalid snapshot request"),
            "{what} was not refused as invalid input: {body}"
        );
    };

    for body in [
        serde_json::json!({ "volume": "/../../b/db/data" }),
        serde_json::json!({ "volume": "../../b/db/data" }),
        serde_json::json!({ "name": "/abs/path" }),
        serde_json::json!({ "name": "../../../../tmp/owned" }),
        serde_json::json!({ "name": "a/b" }),
        serde_json::json!({ "volume": "/data", "name": ".." }),
    ] {
        let what = format!("create {body}");
        refused(
            send("POST", "/v1/snapshots/a/web", body.to_string()).await,
            &what,
        );
    }
    for body in [
        serde_json::json!({ "name": "../../../b/db/data/x" }),
        serde_json::json!({ "name": "1", "volume": "/../../b/db/data" }),
    ] {
        let what = format!("restore {body}");
        refused(
            send("POST", "/v1/snapshots/a/web/restore", body.to_string()).await,
            &what,
        );
    }
    refused(
        send(
            "DELETE",
            "/v1/snapshots/a/web/1?volume=/../../b/db/data",
            String::new(),
        )
        .await,
        "delete",
    );

    assert_eq!(
        tree(volumes_dir.path()),
        before,
        "a refused request touched the volumes directory"
    );
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
                        serde_json::json!({ "ttl_seconds": 900, "node_id": node_id }).to_string(),
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
                .any(|token| crate::sesame::ca::verify_join_token(plaintext, &token.token_hash)),
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
    let created = create_token("ci-bot", ApiRole::Deployer, TokenScope::default(), None).unwrap();
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

    let (code, body) = get_authenticated(app, "/v1/status?cluster=true", &created.plaintext).await;
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
                    platform: None,
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

/// #326: the council's record of why an app isn't placed reaches every
/// reader of desired-app evidence (`relish status`, `inspect`, `wtf`
/// and the dashboard).
#[test]
fn council_app_evidence_carries_the_quota_block() {
    let mut desired = crate::council::types::DesiredState::default();
    let greedy = crate::meat::types::AppId::new("greedy", "prod");
    let fine = crate::meat::types::AppId::new("fine", "prod");
    let spec: crate::config::app::AppSpec = toml::from_str(r#"image = "x:1""#).unwrap();
    desired.apps.insert(greedy.clone(), spec.clone());
    desired.apps.insert(fine.clone(), spec);
    let reason = crate::meat::quota::QuotaError::MaxAppsExceeded {
        namespace: "prod".into(),
        current: 1,
        limit: 1,
    };
    desired.quota_blocked.insert(greedy, reason.clone());

    let evidence = council_app_evidence(&desired, None);
    assert_eq!(evidence[0].app, "fine");
    assert_eq!(evidence[0].blocked, None);
    assert_eq!(evidence[1].app, "greedy");
    assert_eq!(evidence[1].blocked, Some(reason));
}

/// #423: a volume app whose home node is out of the cluster carries that
/// node, so status, inspect, the dashboard and wtf can say what it waits
/// for. A decommissioned home, a live one, or a stopped app carries none.
#[test]
fn council_app_evidence_names_the_volume_home_an_app_waits_for() {
    let mut desired = crate::council::types::DesiredState::default();
    let db = crate::meat::types::AppId::new("db", "prod");
    let spec: crate::config::app::AppSpec =
        toml::from_str("image = \"x:1\"\n[[volumes]]\npath = \"/data\"\n").unwrap();
    desired.apps.insert(db.clone(), spec);
    desired.scheduling.insert(
        db.clone(),
        vec![crate::meat::types::Placement {
            node_id: crate::meat::NodeId::new("node-2"),
            resources: crate::meat::Resources::new(100, 0, 0),
        }],
    );
    let live: std::collections::HashSet<String> = ["node-1".to_string()].into();

    let away = |desired: &crate::council::types::DesiredState| {
        council_app_evidence(desired, Some(&live))[0]
            .volume_home_away
            .clone()
    };
    assert_eq!(away(&desired), Some("node-2".to_string()));
    assert_eq!(
        council_app_evidence(&desired, None)[0].volume_home_away,
        None
    );

    let mut back = live.clone();
    back.insert("node-2".to_string());
    assert_eq!(
        council_app_evidence(&desired, Some(&back))[0].volume_home_away,
        None
    );

    let mut stopped = desired.clone();
    stopped.stopped_apps.insert(db.clone());
    assert_eq!(away(&stopped), None);

    let mut retired = desired.clone();
    retired.security_state.crl.retired_nodes.insert(
        "node-2".into(),
        crate::cluster::retirement::NodeRetirement {
            node_id: "node-2".into(),
            retired_by: "operator".into(),
            reason: "disk died".into(),
            retired_at_unix_ms: 30,
            released_placements: Default::default(),
            released_registry_writers: Default::default(),
            released_node_fault: None,
            released_endpoint_consumer: false,
        },
    );
    assert_eq!(away(&retired), None);
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
            blocked: None,
            volume_home_away: None,
        },
        crate::bun::diagnostics::DesiredAppEvidence {
            app: "pending".into(),
            namespace: "default".into(),
            desired_replicas: 2,
            scheduled_replicas: 0,
            placements: Default::default(),
            service_port: None,
            blocked: None,
            volume_home_away: None,
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
    assert_eq!(pending.state, "pending");

    // #326: an app the namespace quota keeps off every node reads as
    // blocked, not as waiting for room.
    let mut quota_blocked = desired.clone();
    quota_blocked[1].blocked = Some(crate::meat::quota::QuotaError::MaxAppsExceeded {
        namespace: "default".into(),
        current: 1,
        limit: 1,
    });
    let rows = statuses_to_dashboard_apps(&[running.clone()], &quota_blocked);
    let pending = rows.iter().find(|row| row.name == "pending").unwrap();
    assert_eq!(pending.state, "blocked");

    // #423: so does a volume app waiting for the node that holds its data.
    let mut waiting = desired.clone();
    waiting[1].volume_home_away = Some("node-2".into());
    let rows = statuses_to_dashboard_apps(&[running.clone()], &waiting);
    let pending = rows.iter().find(|row| row.name == "pending").unwrap();
    assert_eq!(pending.state, "blocked");

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
        let Some(AgentCommand::Nodes { response, .. }) = rx.recv().await else {
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

/// Gossip's live view drops a dead member; the listing hands the agent
/// the ones this node remembers, so `relish nodes` shows them as dead.
#[tokio::test]
async fn nodes_endpoint_lists_remembered_dead_members_as_dead() {
    let (tx, mut rx) = mpsc::channel(1);
    let worker = tokio::spawn(async move {
        let Some(AgentCommand::Nodes { down, response }) = rx.recv().await else {
            panic!("expected membership request");
        };
        // The agent lists them after its live members; echo them back.
        response.send(down).unwrap();
    });
    let known = KnownMembers::default();
    let member = |name: &str, port: u16, state| RosterMember {
        info: NodeMembershipInfo {
            node_id: crate::meat::NodeId::new(name),
            address: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            api_advertised: true,
        },
        gossip_address: std::net::SocketAddr::from(([127, 0, 0, 1], port - 3)),
        state,
        incarnation: 4,
        labels: std::collections::BTreeMap::from([("zone".to_string(), "a".to_string())]),
    };
    use crate::mustard::state::NodeState::{Alive, Dead, Suspect};
    known
        .refresh(
            vec![
                member("one", 19117, Alive),
                member("doubtful", 19217, Suspect),
                member("dead", 19317, Dead),
            ],
            &Default::default(),
            std::time::Instant::now(),
        )
        .await;
    let app = router(
        tx, None, None, None, None, None, None, None, None, None, None, None, 9117, None,
    )
    .layer(axum::Extension(known));
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
    assert_eq!(nodes.len(), 1, "{nodes:?}");
    assert_eq!(nodes[0].node_id, "dead");
    assert_eq!(nodes[0].state, "dead");
    assert_eq!(nodes[0].address, "127.0.0.1:19314");
    assert_eq!(
        nodes[0].api_address,
        Some("127.0.0.1:19317".parse().unwrap())
    );
    assert_eq!(nodes[0].incarnation, 4);
    assert_eq!(nodes[0].labels["zone"], "a");
    worker.await.unwrap();
}

#[tokio::test]
async fn version_endpoint_reports_the_build_commit() {
    let (app, shutdown) = test_setup();
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/version")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["commit"].as_str(),
        crate::upgrade::version::build_commit()
    );
    assert!(json.get("commit").is_some(), "the field is always present");
    assert_eq!(
        json["version"],
        crate::upgrade::version::compiled_version().to_string()
    );
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
    let (app, shutdown, plaintext, _store) = ui_setup_with_store(role, None);
    (app, shutdown, plaintext)
}

/// Like `ui_setup`, but the token can expire and the caller keeps the
/// token store, so a test can revoke or reissue tokens after logging in.
fn ui_setup_with_store(
    role: crate::sesame::types::ApiRole,
    expires_at: Option<std::time::SystemTime>,
) -> (
    Router,
    CancellationToken,
    String,
    crate::sesame::auth::TokenStore,
) {
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
        expires_at,
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
        Some(store.clone()),
        None,
        None,
        None,
        None,
        9117,
        None,
    );
    (app, shutdown, plaintext, store)
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

/// Build a POST /ui/session request carrying `token`.
fn login_request(token: &str) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri("/ui/session")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(format!("token={token}")))
        .unwrap()
}

#[tokio::test]
async fn a_malformed_login_is_refused_without_touching_argon2() {
    // Every verification permit is held, so any login that reached the
    // Argon2 path would block. A junk token must be refused by the shape
    // check alone, promptly (B12).
    let (app, shutdown, _t) = ui_setup(crate::sesame::types::ApiRole::ReadOnly);
    let _permits = crate::sesame::auth::hold_all_verify_permits().await;
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        app.clone().oneshot(login_request("nope")),
    )
    .await
    .expect("a malformed login must not wait for an Argon2 permit")
    .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    shutdown.cancel();
}

#[tokio::test]
async fn a_well_shaped_login_shares_the_argon2_concurrency_bound() {
    // With every permit held, a login that looks like a real token has to
    // queue for the same semaphore the bearer path uses (B12) rather than
    // hashing on the blocking pool unbounded.
    let (app, shutdown, token) = ui_setup(crate::sesame::types::ApiRole::ReadOnly);
    let permits = crate::sesame::auth::hold_all_verify_permits().await;
    let pending = tokio::spawn(app.clone().oneshot(login_request(&token)));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !pending.is_finished(),
        "login verified without waiting for a permit"
    );

    // Releasing the permits lets the queued login finish normally.
    drop(permits);
    let resp = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
        .await
        .expect("login should proceed once a permit frees up")
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    shutdown.cancel();
}

#[tokio::test]
async fn a_dashboard_session_ends_when_its_token_is_revoked() {
    // B11: the cookie is only as good as the token it was exchanged for.
    // A second token keeps the store non-empty, so auth stays enforced.
    let (app, shutdown, token, store) =
        ui_setup_with_store(crate::sesame::types::ApiRole::ReadOnly, None);
    let spare = crate::sesame::token::create_token(
        "spare",
        crate::sesame::types::ApiRole::Admin,
        crate::sesame::types::TokenScope::default(),
        None,
    )
    .unwrap();
    store.write().await.push(spare.token);
    let id = login(&app, &token).await;
    let cookie = format!("rb_session={id}");
    let before = ui_get(&app, "/ui/fragment/apps", &[("cookie", &cookie)]).await;
    assert_eq!(before.status(), StatusCode::OK);

    store.write().await.retain(|t| t.name != "dash");
    let after = ui_get(&app, "/ui/fragment/apps", &[("cookie", &cookie)]).await;
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
    shutdown.cancel();
}

#[tokio::test]
async fn the_session_cookie_expires_no_later_than_its_token() {
    let expiry = std::time::SystemTime::now() + std::time::Duration::from_secs(600);
    let (app, shutdown, token, _store) =
        ui_setup_with_store(crate::sesame::types::ApiRole::ReadOnly, Some(expiry));
    let resp = app.clone().oneshot(login_request(&token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
    let max_age: u64 = cookie
        .split("; ")
        .find_map(|part| part.strip_prefix("Max-Age="))
        .unwrap()
        .parse()
        .unwrap();
    assert!(max_age <= 600, "cookie outlives its token: {cookie}");
    assert!(max_age > 500, "cookie cut unexpectedly short: {cookie}");
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
fn webhook_setup(secret: &str, rate_limit: u32) -> (Router, mpsc::Receiver<()>, CancellationToken) {
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
        crate::bun::build_runner::BuildSettings::with_timeout(900),
        crate::cluster::ClusterHttp::plaintext(),
        5050,
        "http",
        256 * 1024 * 1024,
        false,
        crate::bun::capabilities::StaticCapabilities::default(),
        crate::bun::readiness::ReadinessTracker::new(),
        None,
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
        crate::bun::build_runner::BuildSettings::with_timeout(900),
        crate::cluster::ClusterHttp::plaintext(),
        5050,
        "http",
        256 * 1024 * 1024,
        false,
        crate::bun::capabilities::StaticCapabilities::default(),
        crate::bun::readiness::ReadinessTracker::new(),
        None,
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

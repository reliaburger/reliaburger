//! Token issuance must reject invalid expiry before committing credentials.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use reliaburger::council::{
    CouncilConfig, CouncilNode, CouncilNodeInfo, DesiredState,
    log_store::MemLogStore,
    network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter},
    state_machine::CouncilStateMachine,
};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tower::ServiceExt;

async fn api() -> (Arc<CouncilNode>, Router) {
    api_with_capacity(false).await
}

async fn api_with_capacity(capacity: bool) -> (Arc<CouncilNode>, Router) {
    let network = InMemoryRaftRouter::new();
    let council = Arc::new(
        CouncilNode::new(
            1,
            CouncilConfig::default(),
            InMemoryRaftNetworkFactory::new(1, network.clone()),
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap(),
    );
    network.register(1, council.raft().clone()).await;
    council
        .initialize(BTreeMap::from([(1, CouncilNodeInfo::default())]))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !council.is_leader().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let router = router_for_council(council.clone(), None, None, capacity);
    (council, router)
}

fn router_for_council(
    council: Arc<CouncilNode>,
    membership: Option<Arc<tokio::sync::RwLock<Vec<reliaburger::bun::api::NodeMembershipInfo>>>>,
    service_token: Option<String>,
    capacity: bool,
) -> Router {
    let (capacity_publisher, aggregated_rx) = if capacity {
        let node = reliaburger::meat::NodeId::new("local");
        let mut state = reliaburger::reporting::aggregator::AggregatedState {
            leadership_epoch: Some(council.current_term()),
            ..Default::default()
        };
        state.receive_deadlines.insert(
            node.clone(),
            tokio::time::Instant::now() + Duration::from_secs(30),
        );
        state.reports.insert(
            node.clone(),
            reliaburger::reporting::types::StateReport {
                node_id: node,
                timestamp: SystemTime::UNIX_EPOCH,
                running_apps: vec![],
                cached_specs: vec![],
                rollouts: Vec::new(),
                resource_usage: reliaburger::reporting::types::ResourceUsage {
                    cpu_total_millicores: 8000,
                    memory_total_mb: 16384,
                    ..Default::default()
                },
                event_log: vec![],
                has_buildah: false,
                job_reservations: Default::default(),
                capacity_refusals: Vec::new(),
            },
        );
        let (publisher, receiver) = tokio::sync::watch::channel(state);
        (Some(publisher), Some(receiver))
    } else {
        (None, None)
    };
    let membership = if capacity {
        Some(Arc::new(tokio::sync::RwLock::new(vec![
            reliaburger::bun::api::NodeMembershipInfo {
                node_id: reliaburger::meat::NodeId::new("local"),
                address: "127.0.0.1:1".parse().unwrap(),
                api_advertised: true,
                jobs: None,
            },
        ])))
    } else {
        membership
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        let _capacity_publisher = capacity_publisher;
        while let Some(command) = rx.recv().await {
            if let reliaburger::bun::agent::AgentCommand::Status { response } = command {
                let _ = response.send(Vec::new());
            }
        }
    });
    reliaburger::bun::api::router_with_upgrade(
        tx,
        None,
        None,
        None,
        None,
        None,
        Some(council.clone()),
        None,
        service_token,
        None,
        membership,
        None,
        None,
        0,
        None,
        None,
        aggregated_rx,
        "test".into(),
        capacity.then(|| "local".into()),
        reliaburger::bun::build_runner::BuildSettings::with_timeout(900),
        reliaburger::cluster::ClusterHttp::plaintext(),
        5050,
        "http",
        256 * 1024 * 1024,
        false,
        reliaburger::bun::capabilities::StaticCapabilities {
            test_policy: reliaburger::testkit::safety::ClusterTestPolicy {
                safety_class: reliaburger::testkit::safety::ClusterSafetyClass::Development,
                allowed_operations: [
                    reliaburger::testkit::safety::OperationPermission::ProvisionIsolatedWorkloads,
                ]
                .into(),
                ..Default::default()
            },
            ..Default::default()
        },
        reliaburger::bun::readiness::ReadinessTracker::new(),
        None,
        None,
        None,
        None,
    )
}

async fn create(router: Router, name: &str, ttl: Option<u64>) -> StatusCode {
    router
        .oneshot(
            Request::post("/v1/token/create")
                .header("content-type", "application/json")
                .body(Body::from(
                    // No TTL means an explicit `--no-expiry`, not the default lifetime.
                    serde_json::json!({"name": name, "role": "deployer", "ttl_days": ttl, "no_expiry": ttl.is_none()})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn token_creation_rejects_overflowing_expiry_without_panicking_or_committing() {
    let (council, router) = api().await;
    for days in [u64::MAX, u64::MAX / 86_400] {
        assert_eq!(
            create(router.clone(), "overflow", Some(days)).await,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(council.security_state().await.api_tokens.is_empty());
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn token_creation_refuses_zero_days_and_preserves_explicit_non_expiring_tokens() {
    let (council, router) = api().await;
    assert_eq!(
        create(router.clone(), "zero", Some(0)).await,
        StatusCode::BAD_REQUEST
    );
    let before = SystemTime::now();
    assert_eq!(
        create(router.clone(), "one-day", Some(1)).await,
        StatusCode::OK
    );
    let after = SystemTime::now();
    assert_eq!(create(router, "unlimited", None).await, StatusCode::OK);
    let state = council.security_state().await;
    assert_eq!(state.api_tokens.len(), 2);
    let expiry = state
        .api_tokens
        .iter()
        .find(|token| token.name == "one-day")
        .unwrap()
        .expires_at
        .unwrap();
    assert!(expiry >= before + Duration::from_secs(86_400));
    assert!(expiry <= after + Duration::from_secs(86_400));
    assert!(
        state
            .api_tokens
            .iter()
            .find(|token| token.name == "unlimited")
            .unwrap()
            .expires_at
            .is_none()
    );
    council.shutdown().await.unwrap();
}

fn owner() -> reliaburger::sesame::auth::AuthContext {
    reliaburger::sesame::auth::AuthContext {
        token_name: "ci".into(),
        principal_id: "exact-ci-credential".into(),
        role: reliaburger::sesame::types::ApiRole::Admin,
        scoped_apps: None,
        scoped_namespaces: None,
    }
}

#[tokio::test]
async fn batch_submission_checks_every_job_against_token_scope_before_dispatch() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let router = reliaburger::bun::api::router(
        tx, None, None, None, None, None, None, None, None, None, None, None, 0, None,
    );
    let mut auth = owner();
    auth.role = reliaburger::sesame::types::ApiRole::Deployer;
    auth.scoped_apps = Some(vec!["allowed".into(), "valid".into()]);
    auth.scoped_namespaces = Some(vec!["team".into()]);
    for (name, namespace) in [("forbidden", "team"), ("allowed", "other")] {
        let mut request = Request::post("/v1/batch").header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"jobs":[{"name":"valid","namespace":"team","spec":{"image":"busybox","command":["true"]}},{"name":name,"namespace":namespace,"spec":{"image":"busybox","command":["true"]}}]}).to_string())).unwrap();
        request.extensions_mut().insert(auth.clone());
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }
}

#[tokio::test]
async fn batch_submission_enforces_deploy_and_host_execution_permissions() {
    let (council, router) = api_with_capacity(true).await;
    for (actions, spec) in [
        (
            vec!["logs"],
            serde_json::json!({"image":"busybox","command":["true"]}),
        ),
        (
            vec!["deploy"],
            serde_json::json!({"runtime":"process","script":"echo denied"}),
        ),
        (
            vec!["deploy"],
            serde_json::json!({"runtime":"process","exec":"/bin/true"}),
        ),
    ] {
        council
            .write(reliaburger::council::types::RaftRequest::PermissionSpec {
                name: "ci".into(),
                spec: Box::new(reliaburger::config::PermissionSpec {
                    actions: actions.into_iter().map(str::to_string).collect(),
                    apps: vec!["*".into()],
                    namespaces: None,
                }),
            })
            .await
            .unwrap();
        let mut request = Request::post("/v1/batch")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"jobs":[{"name":"migration","spec":spec}]}).to_string(),
            ))
            .unwrap();
        request.extensions_mut().insert(owner());
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(council.desired_state().await.task_arrays.ids().is_empty());
    }
    council
        .write(reliaburger::council::types::RaftRequest::PermissionSpec {
            name: "ci".into(),
            spec: Box::new(reliaburger::config::PermissionSpec {
                actions: vec!["deploy".into(), "host-exec".into()],
                apps: vec!["migration".into()],
                namespaces: Some(vec!["default".into()]),
            }),
        })
        .await
        .unwrap();
    let mut request = Request::post("/v1/batch")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"jobs":[{"name":"migration","spec":{"runtime":"process","exec":"/bin/true"}}]})
                .to_string(),
        ))
        .unwrap();
    request.extensions_mut().insert(owner());
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(council.desired_state().await.task_arrays.get(1).is_some());
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn batch_submission_refuses_unowned_test_namespaces_and_images() {
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let router = reliaburger::bun::api::router(
        tx, None, None, None, None, None, None, None, None, None, None, None, 0, None,
    );
    for (namespace, image) in [
        ("rbtest-batch", "busybox"),
        ("default", "localhost:5050/rbtest-image/work:test"),
    ] {
        let request = Request::post("/v1/batch").header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"jobs":[{"name":"migration","namespace":namespace,"spec":{"image":image,"command":["true"]}}]}).to_string())).unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
}

#[tokio::test]
async fn batch_submission_validates_every_spec_before_registering_or_dispatching() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let router = reliaburger::bun::api::router(
        tx, None, None, None, None, None, None, None, None, None, None, None, 0, None,
    );
    for spec in [
        serde_json::json!({}),
        serde_json::json!({"image":"busybox","exec":"/bin/true"}),
        serde_json::json!({"script":"echo invalid","exec":"/bin/true"}),
    ] {
        let request = Request::post("/v1/batch")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"jobs":[
                    {"name":"valid","spec":{"image":"busybox","command":["true"]}},
                    {"name":"invalid","spec":spec}
                ]})
                .to_string(),
            ))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        let response = router
            .clone()
            .oneshot(Request::get("/v1/batch/1").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn follower_batch_submission_preserves_the_callers_credential_for_leader_admission() {
    let (received_tx, mut received_rx) = tokio::sync::mpsc::channel(1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let leader_api = Router::new().route(
        "/v1/batch",
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let received_tx = received_tx.clone();
            async move {
                let credential = headers
                    .get("authorization")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();
                let status = if credential == "Bearer caller-credential" {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::ACCEPTED
                };
                received_tx.send(credential).await.unwrap();
                status
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, leader_api).await.unwrap() });
    let network = InMemoryRaftRouter::new();
    let mut nodes = Vec::new();
    for id in [1, 2] {
        let council = Arc::new(
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
        network.register(id, council.raft().clone()).await;
        nodes.push(council);
    }
    nodes[0]
        .initialize(BTreeMap::from([(
            1,
            CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), "leader"),
        )]))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !nodes[0].is_leader().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    nodes[0]
        .add_learner(
            2,
            CouncilNodeInfo::new("127.0.0.1:9002".parse().unwrap(), "follower"),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while nodes[1].current_leader().await != Some(1) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let membership = Arc::new(tokio::sync::RwLock::new(vec![
        reliaburger::bun::api::NodeMembershipInfo {
            node_id: reliaburger::meat::NodeId::new("leader"),
            address,
            api_advertised: true,
            jobs: None,
        },
    ]));
    let router = router_for_council(
        nodes[1].clone(),
        Some(membership),
        Some("cluster-service-credential".into()),
        false,
    );
    let mut request = Request::post("/v1/batch")
        .header("content-type", "application/json")
        .header("authorization", "Bearer caller-credential")
        .body(Body::from(serde_json::json!({"jobs":[{"name":"migration","spec":{"image":"busybox","command":["true"]}}]}).to_string())).unwrap();
    request.extensions_mut().insert(owner());
    let response = router.oneshot(request).await.unwrap();
    let received = tokio::time::timeout(Duration::from_secs(5), received_rx.recv())
        .await
        .unwrap()
        .unwrap();
    server.abort();
    for node in nodes {
        node.shutdown().await.unwrap();
    }
    assert_eq!(received, "Bearer caller-credential");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

async fn create_leased(
    router: Router,
    auth: Option<reliaburger::sesame::auth::AuthContext>,
    lease_id: &str,
    name: &str,
) -> axum::response::Response {
    let mut request = Request::post("/v1/token/create")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::json!({
            "name": name, "role": "deployer", "namespaces": ["rbtest-token"], "lease_id": lease_id,
        }).to_string())).unwrap();
    if let Some(auth) = auth {
        request.extensions_mut().insert(auth);
    }
    router.oneshot(request).await.unwrap()
}

async fn lease(
    council: &CouncilNode,
    lifetime: Duration,
) -> reliaburger::testkit::lease::TestLease {
    use reliaburger::testkit::lease::{TestLease, now_unix_millis};
    let now = now_unix_millis();
    let lease = TestLease::new(
        "token-lease".into(),
        owner().principal_id,
        "ci".into(),
        "rbtest-token".into(),
        now,
        now + lifetime.as_millis() as u64,
    )
    .unwrap();
    council
        .write(reliaburger::council::RaftRequest::TestLeaseCreate(
            lease.clone(),
        ))
        .await
        .unwrap();
    lease
}

#[tokio::test]
async fn leased_token_creation_requires_the_exact_authenticated_owner() {
    let (council, router) = api().await;
    let lease = lease(&council, Duration::from_secs(60)).await;
    assert_eq!(
        create_leased(
            router.clone(),
            None,
            &lease.lease_id,
            "rbtest-token-anonymous"
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let mut stranger = owner();
    stranger.principal_id = "different-credential".into();
    assert_eq!(
        create_leased(
            router,
            Some(stranger),
            &lease.lease_id,
            "rbtest-token-stranger"
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert!(council.security_state().await.api_tokens.is_empty());
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn leased_token_is_atomically_owned_expiry_bounded_and_reaped_without_a_client() {
    use reliaburger::testkit::lease::spawn_cluster_lease_reaper;
    let (council, router) = api().await;
    let lease = lease(&council, Duration::from_secs(2)).await;
    let response = create_leased(
        router,
        Some(owner()),
        &lease.lease_id,
        "rbtest-token-scoped",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let tokens = council.security_state().await.api_tokens;
    assert_eq!(tokens.len(), 1);
    let expiry = tokens[0].expires_at.expect("leased token must expire");
    assert!(expiry <= SystemTime::UNIX_EPOCH + Duration::from_millis(lease.expires_at_unix_ms));
    assert_eq!(
        council.desired_state().await.test_leases[&lease.lease_id]
            .resources
            .len(),
        1
    );
    let shutdown = tokio_util::sync::CancellationToken::new();
    let reaper = spawn_cluster_lease_reaper(council.clone(), shutdown.clone());
    tokio::time::timeout(Duration::from_secs(6), async {
        while council
            .desired_state()
            .await
            .test_leases
            .contains_key(&lease.lease_id)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(council.security_state().await.api_tokens.is_empty());
    shutdown.cancel();
    reaper.await.unwrap();
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn token_creation_refuses_duplicate_and_unleased_test_names_without_returning_credentials() {
    let (council, router) = api().await;
    assert_eq!(
        create(router.clone(), "operator", None).await,
        StatusCode::OK
    );
    assert_eq!(
        create(router.clone(), "operator", None).await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        create(router.clone(), "rbtest-unowned", None).await,
        StatusCode::CONFLICT
    );
    let lease = lease(&council, Duration::from_secs(60)).await;
    assert_eq!(
        create_leased(
            router.clone(),
            Some(owner()),
            &lease.lease_id,
            "rbtest-token-owned"
        )
        .await
        .status(),
        StatusCode::OK
    );
    let refused = create_leased(router, Some(owner()), &lease.lease_id, "rbtest-token-owned").await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(refused.into_body(), 4096)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("rbrg_"));
    assert_eq!(council.security_state().await.api_tokens.len(), 2);
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn scoped_admin_cannot_mint_unscoped_credentials_or_use_global_management() {
    let (council, router) = api().await;
    for (apps, namespaces) in [
        (Some(vec!["web".into()]), None),
        (None, Some(vec!["team".into()])),
    ] {
        let mut auth = owner();
        auth.scoped_apps = apps;
        auth.scoped_namespaces = namespaces;
        for (method, path, body) in [
            (
                "POST",
                "/v1/token/create",
                r#"{"name":"escape","role":"admin"}"#,
            ),
            ("GET", "/v1/token/list", ""),
            ("POST", "/v1/token/revoke", r#"{"name":"operator"}"#),
            ("POST", "/v1/join-token/create", "{}"),
            ("POST", "/v1/secret/rotate", "{}"),
            (
                "POST",
                "/v1/identity/sign",
                r#"{"digest":"sha256:example","public_key":"","signature":""}"#,
            ),
        ] {
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            request.extensions_mut().insert(auth.clone());
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
            assert!(council.security_state().await.api_tokens.is_empty());
        }
    }
    // An unrestricted operator retains ordinary token administration.
    for (method, path, body) in [
        (
            "POST",
            "/v1/token/create",
            r#"{"name":"operator","role":"deployer"}"#,
        ),
        ("GET", "/v1/token/list", ""),
        ("POST", "/v1/token/revoke", r#"{"name":"operator"}"#),
    ] {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        request.extensions_mut().insert(owner());
        assert_eq!(
            router.clone().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
    }
    assert!(council.security_state().await.api_tokens.is_empty());
    council.shutdown().await.unwrap();
}

async fn access_lease(
    router: Router,
    method: &str,
    auth: reliaburger::sesame::auth::AuthContext,
) -> StatusCode {
    let mut request = Request::builder()
        .method(method)
        .uri("/v1/test/leases/token-lease")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(auth);
    router.oneshot(request).await.unwrap().status()
}

async fn check_lease_admin_override(method: &str, success: StatusCode) {
    let (council, router) = api().await;
    lease(&council, Duration::from_secs(60)).await;
    for (apps, namespaces) in [
        (Some(vec!["web".into()]), None),
        (None, Some(vec!["another-tenant".into()])),
    ] {
        let mut outsider = owner();
        outsider.principal_id = "other-credential".into();
        outsider.token_name = "other-admin".into();
        outsider.scoped_apps = apps;
        outsider.scoped_namespaces = namespaces;
        assert_eq!(
            access_lease(router.clone(), method, outsider).await,
            StatusCode::FORBIDDEN
        );
        assert!(
            council
                .desired_state()
                .await
                .test_leases
                .contains_key("token-lease")
        );
    }
    let mut operator = owner();
    operator.principal_id = "global-operator".into();
    assert_eq!(
        access_lease(router.clone(), method, operator).await,
        success
    );
    if method == "DELETE" {
        lease(&council, Duration::from_secs(60)).await;
    }
    // Exact owners do not need the administrator override.
    let mut scoped_owner = owner();
    scoped_owner.role = reliaburger::sesame::types::ApiRole::Deployer;
    scoped_owner.scoped_namespaces = Some(vec!["rbtest-token".into()]);
    assert_eq!(access_lease(router, method, scoped_owner).await, success);
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn scoped_admin_cannot_inspect_another_credentials_lease() {
    check_lease_admin_override("GET", StatusCode::OK).await;
}

#[tokio::test]
async fn scoped_admin_cannot_release_another_credentials_lease() {
    check_lease_admin_override("DELETE", StatusCode::NO_CONTENT).await;
}

/// The top-level desired-state fields that differ between two reads,
/// ignoring what the leader writes on its own (#849). The router's
/// task-array leader loop records each new wall-clock minute it observes
/// (`CronObserve`): its first tick lands late under load, and later ones
/// whenever a request straddles a minute boundary. That entry moves the log
/// position and `cron_observed_minute`, so neither can show whether a
/// request wrote.
fn changed_desired_fields(before: &DesiredState, after: &DesiredState) -> Vec<String> {
    let fields = |state: &DesiredState| {
        let serde_json::Value::Object(mut fields) = serde_json::to_value(state).unwrap() else {
            panic!("desired state serialises as an object");
        };
        fields.remove("last_applied_log");
        if let Some(serde_json::Value::Object(arrays)) = fields.get_mut("task_arrays") {
            arrays.remove("cron_observed_minute");
        }
        fields
    };
    let (before, after) = (fields(before), fields(after));
    before
        .keys()
        .filter(|key| before.get(*key) != after.get(*key))
        .cloned()
        .collect()
}

/// Router configuration from the existing api_tokens fixture, with its command
/// sender exposed for authority observations. No runtime/terminal proof is claimed.
fn shared_authority_router(
    council: Arc<CouncilNode>,
    tokens: reliaburger::sesame::auth::TokenStore,
    commands: tokio::sync::mpsc::Sender<reliaburger::bun::agent::AgentCommand>,
    membership: Arc<tokio::sync::RwLock<Vec<reliaburger::bun::api::NodeMembershipInfo>>>,
    reports: Option<
        tokio::sync::watch::Receiver<reliaburger::reporting::aggregator::AggregatedState>,
    >,
    name: &str,
) -> Router {
    reliaburger::bun::api::router_with_upgrade(
        commands,
        None,
        None,
        None,
        None,
        None,
        Some(council),
        Some(tokens),
        Some("authority-fixture-service-credential".into()),
        None,
        Some(membership),
        None,
        None,
        0,
        None,
        None,
        reports,
        "test".into(),
        Some(name.into()),
        reliaburger::bun::build_runner::BuildSettings::with_timeout(900),
        reliaburger::cluster::ClusterHttp::plaintext(),
        5050,
        "http",
        256 * 1024 * 1024,
        false,
        reliaburger::bun::capabilities::StaticCapabilities {
            test_policy: reliaburger::testkit::safety::ClusterTestPolicy {
                safety_class: reliaburger::testkit::safety::ClusterSafetyClass::Development,
                allowed_operations: [
                    reliaburger::testkit::safety::OperationPermission::ProvisionIsolatedWorkloads,
                ]
                .into(),
                ..Default::default()
            },
            ..Default::default()
        },
        reliaburger::bun::readiness::ReadinessTracker::new(),
        None,
        None,
        None,
        None,
    )
}

/// Two real API servers on one in-memory council, each with its own token
/// store and neither running the five-second refresh, so a token reaches a
/// store only through the create that made it.
struct TokenCluster {
    leader: Arc<CouncilNode>,
    follower: Arc<CouncilNode>,
    leader_url: String,
    follower_url: String,
    /// The follower's own auth store, for a credential only it has seen.
    follower_tokens: reliaburger::sesame::auth::TokenStore,
    /// Raft id of the node behind `follower_url`.
    follower_id: u64,
    /// Every Raft node but the one behind `follower_url`.
    other_ids: Vec<u64>,
    router: InMemoryRaftRouter,
    membership: Arc<tokio::sync::RwLock<Vec<reliaburger::bun::api::NodeMembershipInfo>>>,
    leader_name: String,
    admin: String,
    admin_principal: String,
    client: reqwest::Client,
    shutdown: tokio_util::sync::CancellationToken,
    _tasks: crate::task_harness::TestTasks,
}

impl TokenCluster {
    async fn start() -> Self {
        Self::start_with(false).await
    }

    /// The same cluster with a learner behind `follower_url`. A learner
    /// never campaigns, so a test can cut it off from replication and it
    /// keeps naming the leader, which is what a lagging follower looks like.
    async fn start_with_learner() -> Self {
        Self::start_with(true).await
    }

    async fn start_with(learner: bool) -> Self {
        use crate::cluster_support;
        use crate::task_harness::TestTasks;

        let (nodes, router) = cluster_support::build_council().await;
        let shutdown = tokio_util::sync::CancellationToken::new();
        let cleanup_nodes = nodes.clone();
        let cleanup_signal = shutdown.clone();
        let cleanup = tokio::spawn(async move {
            cleanup_signal.cancelled().await;
            for node in cleanup_nodes {
                node.shutdown().await.unwrap();
            }
        });
        let leader_index = cluster_support::initial_voter_leader_index(&nodes)
            .await
            .unwrap();
        let follower_index = if learner {
            3
        } else {
            (0..3).find(|index| *index != leader_index).unwrap()
        };
        let leader = nodes[leader_index].clone();
        let follower = nodes[follower_index].clone();
        if learner {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !leader.is_leader().await {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("no elected leader");
            leader
                .add_learner(
                    cluster_support::rid(follower_index),
                    cluster_support::node_info(follower_index),
                )
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while !leader.is_leader().await
                || follower.current_leader().await != Some(cluster_support::rid(leader_index))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("no elected leader and follower");

        let admin = reliaburger::sesame::token::create_token(
            "operator",
            reliaburger::sesame::types::ApiRole::Admin,
            Default::default(),
            None,
        )
        .unwrap();
        leader
            .write(reliaburger::council::RaftRequest::CreateApiToken(
                admin.token.clone(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while follower.security_state().await.api_tokens.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the follower never applied the admin token");

        let leader_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let follower_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let leader_address = leader_listener.local_addr().unwrap();
        let follower_address = follower_listener.local_addr().unwrap();
        let leader_name = cluster_support::NAMES[leader_index];
        let follower_name = cluster_support::NAMES[follower_index];
        let membership = Arc::new(tokio::sync::RwLock::new(vec![
            reliaburger::bun::api::NodeMembershipInfo {
                node_id: reliaburger::meat::NodeId::new(leader_name),
                address: leader_address,
                api_advertised: true,
                jobs: None,
            },
            reliaburger::bun::api::NodeMembershipInfo {
                node_id: reliaburger::meat::NodeId::new(follower_name),
                address: follower_address,
                api_advertised: true,
                jobs: None,
            },
        ]));
        let (commands, mut incoming) = tokio::sync::mpsc::channel(16);
        let agent_stop = shutdown.clone();
        let agent = tokio::spawn(async move {
            tokio::select! {
                () = agent_stop.cancelled() => {}
                () = async {
                    while let Some(command) = incoming.recv().await {
                        if let reliaburger::bun::agent::AgentCommand::Status { response } = command {
                            let _ = response.send(Vec::new());
                        }
                    }
                } => {}
            }
        });
        let mut tasks = vec![cleanup, agent];
        let follower_tokens = reliaburger::sesame::auth::new_token_store();
        for (listener, council, name, tokens) in [
            (
                leader_listener,
                leader.clone(),
                leader_name,
                reliaburger::sesame::auth::new_token_store(),
            ),
            (
                follower_listener,
                follower.clone(),
                follower_name,
                follower_tokens.clone(),
            ),
        ] {
            tokens.write().await.push(admin.token.clone());
            let router = shared_authority_router(
                council,
                tokens,
                commands.clone(),
                membership.clone(),
                None,
                name,
            );
            let stop = shutdown.clone();
            tasks.push(tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(stop.cancelled_owned())
                    .await
                    .unwrap();
            }));
        }
        Self {
            leader,
            follower,
            leader_url: format!("http://{leader_address}"),
            follower_url: format!("http://{follower_address}"),
            follower_tokens,
            follower_id: cluster_support::rid(follower_index),
            other_ids: (0..cluster_support::NAMES.len())
                .filter(|index| *index != follower_index)
                .map(cluster_support::rid)
                .collect(),
            router,
            membership,
            leader_name: leader_name.to_string(),
            admin_principal: reliaburger::sesame::auth::authenticate(
                &admin.plaintext,
                std::slice::from_ref(&admin.token),
            )
            .unwrap()
            .principal_id,
            admin: admin.plaintext,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            _tasks: TestTasks::new(shutdown.clone(), tasks),
            shutdown,
        }
    }

    async fn create(&self, url: &str, body: serde_json::Value) -> reqwest::Response {
        self.client
            .post(format!("{url}/v1/token/create"))
            .bearer_auth(&self.admin)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// What a read any valid token may make answers on `url` with `secret`.
    async fn read_status(&self, url: &str, secret: &str) -> reqwest::StatusCode {
        self.client
            .get(format!("{url}/v1/status"))
            .bearer_auth(secret)
            .send()
            .await
            .unwrap()
            .status()
    }

    /// POST `body` to `path` on `url` with the admin credential.
    async fn post(&self, url: &str, path: &str, body: serde_json::Value) -> reqwest::Response {
        self.post_as(url, path, &self.admin, body).await
    }

    async fn post_as(
        &self,
        url: &str,
        path: &str,
        bearer: &str,
        body: serde_json::Value,
    ) -> reqwest::Response {
        self.client
            .post(format!("{url}{path}"))
            .bearer_auth(bearer)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// Create a deployer token called `name` on the leader; its secret works
    /// on the leader and, through the fixture's store, on the follower.
    async fn deployer_on_both(&self, name: &str) -> String {
        let response = self
            .post(
                &self.leader_url,
                "/v1/token/create",
                serde_json::json!({"name": name, "role": "deployer"}),
            )
            .await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let answer: serde_json::Value = response.json().await.unwrap();
        let secret = answer["token"].as_str().unwrap().to_string();
        let stored = self
            .leader
            .security_state()
            .await
            .api_tokens
            .into_iter()
            .find(|token| token.name == name)
            .unwrap();
        self.follower_tokens.write().await.push(stored);
        assert_eq!(
            self.read_status(&self.follower_url, &secret).await,
            reqwest::StatusCode::OK
        );
        assert_eq!(
            self.read_status(&self.leader_url, &secret).await,
            reqwest::StatusCode::OK
        );
        secret
    }

    /// Cut the node behind `follower_url` off from replication. Its API can
    /// still reach the leader's, as a lagging node's can.
    async fn stop_replicating_to_the_follower(&self) {
        for other in &self.other_ids {
            self.router.partition(self.follower_id, *other).await;
        }
    }

    /// Make the follower forward to a port that accepts and never answers.
    /// Returns the listener, which has to outlive the request.
    async fn point_the_follower_at_a_silent_leader(&self) -> tokio::net::TcpListener {
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = silent.local_addr().unwrap();
        let leader = reliaburger::meat::NodeId::new(&self.leader_name);
        let mut members = self.membership.write().await;
        members
            .iter_mut()
            .find(|member| member.node_id == leader)
            .unwrap()
            .address = address;
        silent
    }

    async fn stored_named(&self, name: &str) -> usize {
        self.leader
            .security_state()
            .await
            .api_tokens
            .iter()
            .filter(|token| token.name == name)
            .count()
    }

    /// How many tokens called `name` the follower's own replica holds.
    async fn follower_replica_named(&self, name: &str) -> usize {
        self.follower
            .security_state()
            .await
            .api_tokens
            .iter()
            .filter(|token| token.name == name)
            .count()
    }
}

impl Drop for TokenCluster {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// #1496: a follower forwards the create with the caller's own credential,
/// and the new secret works on it, and on the leader, the moment the command
/// returns. Nothing in this fixture runs the periodic refresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_created_through_a_follower_works_on_both_nodes_at_once() {
    let cluster = TokenCluster::start().await;
    let response = cluster
        .create(
            &cluster.follower_url,
            serde_json::json!({"name": "ci", "role": "deployer"}),
        )
        .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_TYPE],
        "application/json"
    );
    let answer: serde_json::Value = response.json().await.unwrap();
    let secret = answer["token"].as_str().unwrap();

    assert_eq!(
        cluster.read_status(&cluster.follower_url, secret).await,
        reqwest::StatusCode::OK
    );
    assert_eq!(
        cluster.read_status(&cluster.leader_url, secret).await,
        reqwest::StatusCode::OK
    );
    assert_eq!(cluster.stored_named("ci").await, 1);
}

/// `relish test` mints its tokens through the same route: a leased create
/// forwarded from a follower has to work at once as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leased_token_created_through_a_follower_works_there_at_once() {
    use reliaburger::testkit::lease::{TestLease, now_unix_millis};
    let cluster = TokenCluster::start().await;
    let now = now_unix_millis();
    let lease = TestLease::new(
        "token-lease".into(),
        cluster.admin_principal.clone(),
        "ci".into(),
        "rbtest-token".into(),
        now,
        now + 60_000,
    )
    .unwrap();
    cluster
        .leader
        .write(reliaburger::council::RaftRequest::TestLeaseCreate(
            lease.clone(),
        ))
        .await
        .unwrap();

    let response = cluster
        .create(
            &cluster.follower_url,
            serde_json::json!({
                "name": "rbtest-token-ci", "role": "deployer",
                "namespaces": ["rbtest-token"], "lease_id": lease.lease_id,
            }),
        )
        .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let answer: serde_json::Value = response.json().await.unwrap();
    let secret = answer["token"].as_str().unwrap();
    assert_eq!(
        cluster.read_status(&cluster.follower_url, secret).await,
        reqwest::StatusCode::OK
    );
}

/// A refusal comes back as the leader gave it, with no secret, and a repeat
/// of a create can't leave a second token behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_create_the_leader_refuses_comes_back_through_the_follower_unchanged() {
    let cluster = TokenCluster::start().await;
    let body = serde_json::json!({"name": "ci", "role": "deployer"});
    assert_eq!(
        cluster
            .create(&cluster.follower_url, body.clone())
            .await
            .status(),
        reqwest::StatusCode::OK
    );

    let refused = cluster.create(&cluster.follower_url, body).await;
    assert_eq!(refused.status(), reqwest::StatusCode::CONFLICT);
    let text = refused.text().await.unwrap();
    assert!(!text.contains("rbrg_"), "{text}");
    assert_eq!(cluster.stored_named("ci").await, 1);
}

/// The forwarded mark stops a request bouncing between two nodes that each
/// think the other leads: the follower answers 503 and writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follower_refuses_a_create_another_node_already_forwarded() {
    let cluster = TokenCluster::start().await;
    let response = cluster
        .client
        .post(format!("{}/v1/token/create", cluster.follower_url))
        .bearer_auth(&cluster.admin)
        .header("x-reliaburger-forwarded", "1")
        .json(&serde_json::json!({"name": "ci", "role": "deployer"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(cluster.stored_named("ci").await, 0);
}

/// #1506: a rotation through a follower reaches the leader, the new secret
/// works on both nodes the moment the command returns, and the old one keeps
/// its grace period on both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_rotated_through_a_follower_works_on_both_nodes_at_once() {
    let cluster = TokenCluster::start().await;
    let old = cluster.deployer_on_both("ci").await;

    let response = cluster
        .post(
            &cluster.follower_url,
            "/v1/token/rotate",
            serde_json::json!({"name": "ci"}),
        )
        .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_TYPE],
        "application/json"
    );
    let answer: serde_json::Value = response.json().await.unwrap();
    let new = answer["token"].as_str().unwrap();
    assert!(answer["previous_valid_until"].is_u64(), "{answer}");

    for url in [&cluster.follower_url, &cluster.leader_url] {
        assert_eq!(
            cluster.read_status(url, new).await,
            reqwest::StatusCode::OK,
            "new secret on {url}"
        );
        assert_eq!(
            cluster.read_status(url, &old).await,
            reqwest::StatusCode::OK,
            "old secret on {url}"
        );
    }
    assert_eq!(cluster.stored_named("ci").await, 1);
}

/// `--grace-hours 0` is what you run on a leaked secret, so it's a revoke of
/// the old secret: it has to stop on the node you asked before you hear back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rotation_without_grace_through_a_follower_ends_the_old_secret_there_at_once() {
    let cluster = TokenCluster::start().await;
    let old = cluster.deployer_on_both("ci").await;

    let response = cluster
        .post(
            &cluster.follower_url,
            "/v1/token/rotate",
            serde_json::json!({"name": "ci", "grace_hours": 0}),
        )
        .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let answer: serde_json::Value = response.json().await.unwrap();
    let new = answer["token"].as_str().unwrap();

    for url in [&cluster.follower_url, &cluster.leader_url] {
        assert_eq!(
            cluster.read_status(url, &old).await,
            reqwest::StatusCode::UNAUTHORIZED,
            "old secret on {url}"
        );
        assert_eq!(
            cluster.read_status(url, new).await,
            reqwest::StatusCode::OK,
            "new secret on {url}"
        );
    }
}

/// #1506: the failure this guards against is a revoked token working on the
/// node you asked for up to five seconds. Nothing in this fixture refreshes,
/// so the token could only stop here because the revoke took it out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_revoked_through_a_follower_stops_working_on_both_nodes_at_once() {
    let cluster = TokenCluster::start().await;
    let secret = cluster.deployer_on_both("ci").await;

    let response = cluster
        .post(
            &cluster.follower_url,
            "/v1/token/revoke",
            serde_json::json!({"name": "ci"}),
        )
        .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_TYPE],
        "application/json"
    );
    let answer: serde_json::Value = response.json().await.unwrap();
    assert_eq!(answer["message"], "token ci revoked");

    assert_eq!(
        cluster.read_status(&cluster.follower_url, &secret).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        cluster.read_status(&cluster.leader_url, &secret).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(cluster.stored_named("ci").await, 0);
}

/// A revoke the leader answered directly ends the token on the leader too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_revoked_on_the_leader_stops_working_there_at_once() {
    let cluster = TokenCluster::start().await;
    let secret = cluster.deployer_on_both("ci").await;

    let response = cluster
        .post(
            &cluster.leader_url,
            "/v1/token/revoke",
            serde_json::json!({"name": "ci"}),
        )
        .await;

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        cluster.read_status(&cluster.leader_url, &secret).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
}

/// A node whose replica is behind can't wait for ever. It answers after two
/// seconds with the token already gone from its store, and says it's behind,
/// because a refresh from that replica could bring the token back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoke_through_a_lagging_node_drops_the_token_there_and_says_it_is_behind() {
    let cluster = TokenCluster::start_with_learner().await;
    let secret = cluster.deployer_on_both("ci").await;
    // The learner has to have the token before the cut, or its replica would
    // look as if the removal had already applied.
    let follower_has_it = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if cluster.follower_replica_named("ci").await == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    follower_has_it.await.expect("the learner never applied it");
    cluster.stop_replicating_to_the_follower().await;

    let started = tokio::time::Instant::now();
    let response = cluster
        .post(
            &cluster.follower_url,
            "/v1/token/revoke",
            serde_json::json!({"name": "ci"}),
        )
        .await;

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(started.elapsed() >= Duration::from_secs(2));
    let answer: serde_json::Value = response.json().await.unwrap();
    let message = answer["message"].as_str().unwrap();
    assert!(message.contains("token ci revoked"), "{message}");
    assert!(message.contains("behind"), "{message}");
    assert_eq!(cluster.follower_replica_named("ci").await, 1);
    assert_eq!(
        cluster.read_status(&cluster.follower_url, &secret).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(cluster.stored_named("ci").await, 0);
}

/// A rotation has committed by the time a lagging node gives up on its
/// replica, so the caller still gets the secret, the only copy there is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rotation_through_a_lagging_node_still_hands_back_the_new_secret() {
    let cluster = TokenCluster::start_with_learner().await;
    cluster.deployer_on_both("ci").await;
    cluster.stop_replicating_to_the_follower().await;

    let response = cluster
        .post(
            &cluster.follower_url,
            "/v1/token/rotate",
            serde_json::json!({"name": "ci"}),
        )
        .await;

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let answer: serde_json::Value = response.json().await.unwrap();
    let new = answer["token"].as_str().unwrap();
    assert_eq!(
        cluster.read_status(&cluster.leader_url, new).await,
        reqwest::StatusCode::OK
    );
}

/// #1506: a join token through a follower commits on the leader, and its
/// plaintext comes back once. Only the hash is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_join_token_created_through_a_follower_commits_on_the_leader() {
    let cluster = TokenCluster::start().await;

    let response = cluster
        .post(
            &cluster.follower_url,
            "/v1/join-token/create",
            serde_json::json!({"node_id": "node-7", "ttl_seconds": 900}),
        )
        .await;

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_TYPE],
        "application/json"
    );
    let answer: serde_json::Value = response.json().await.unwrap();
    assert!(answer["token"].is_string(), "{answer}");
    assert_eq!(answer["ttl_seconds"], 900);
    let stored = cluster.leader.security_state().await.join_tokens;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].node_id, "node-7");
}

/// Refusals are the leader's to give, and they come back as it gave them,
/// with no secret and nothing written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_the_leader_refuses_come_back_through_the_follower_unchanged() {
    let cluster = TokenCluster::start().await;

    let unknown = cluster
        .post(
            &cluster.follower_url,
            "/v1/token/rotate",
            serde_json::json!({"name": "ghost"}),
        )
        .await;
    assert_eq!(unknown.status(), reqwest::StatusCode::NOT_FOUND);

    let last_admin = cluster
        .post(
            &cluster.follower_url,
            "/v1/token/revoke",
            serde_json::json!({"name": "operator"}),
        )
        .await;
    assert_eq!(last_admin.status(), reqwest::StatusCode::CONFLICT);
    assert!(
        last_admin
            .text()
            .await
            .unwrap()
            .contains("last Admin token")
    );
    assert_eq!(
        cluster
            .read_status(&cluster.follower_url, &cluster.admin)
            .await,
        reqwest::StatusCode::OK,
        "a refused revoke leaves the token in the follower's store"
    );

    let no_node = cluster
        .post(
            &cluster.follower_url,
            "/v1/join-token/create",
            serde_json::json!({"ttl_seconds": 900}),
        )
        .await;
    assert_eq!(no_node.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(cluster.leader.security_state().await.join_tokens.is_empty());
}

/// The follower forwards the caller's own credential, so the leader judges
/// it. An Admin secret only the follower knows, as a token the leader has
/// just lost would be, gets nothing done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_leader_repeats_the_checks_on_a_forwarded_write() {
    let cluster = TokenCluster::start().await;
    let secret = cluster.deployer_on_both("ci").await;
    let unknown_to_the_leader = reliaburger::sesame::token::create_token(
        "follower-only",
        reliaburger::sesame::types::ApiRole::Admin,
        Default::default(),
        None,
    )
    .unwrap();
    cluster
        .follower_tokens
        .write()
        .await
        .push(unknown_to_the_leader.token);

    for (path, body) in [
        ("/v1/token/rotate", serde_json::json!({"name": "ci"})),
        ("/v1/token/revoke", serde_json::json!({"name": "ci"})),
        (
            "/v1/join-token/create",
            serde_json::json!({"node_id": "node-7"}),
        ),
    ] {
        let response = cluster
            .post_as(
                &cluster.follower_url,
                path,
                &unknown_to_the_leader.plaintext,
                body,
            )
            .await;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }

    assert_eq!(cluster.stored_named("ci").await, 1);
    assert!(cluster.leader.security_state().await.join_tokens.is_empty());
    assert_eq!(
        cluster.read_status(&cluster.follower_url, &secret).await,
        reqwest::StatusCode::OK,
        "a revoke the leader refused must not touch the follower's store"
    );
}

/// The forwarded mark stops any of these bouncing between two nodes that each
/// think the other leads: the follower answers 503 and writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follower_refuses_token_writes_another_node_already_forwarded() {
    let cluster = TokenCluster::start().await;
    let secret = cluster.deployer_on_both("ci").await;
    for (path, body) in [
        ("/v1/token/rotate", serde_json::json!({"name": "ci"})),
        ("/v1/token/revoke", serde_json::json!({"name": "ci"})),
        (
            "/v1/join-token/create",
            serde_json::json!({"node_id": "node-7"}),
        ),
    ] {
        let response = cluster
            .client
            .post(format!("{}{path}", cluster.follower_url))
            .bearer_auth(&cluster.admin)
            .header("x-reliaburger-forwarded", "1")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "{path}"
        );
    }
    assert_eq!(cluster.stored_named("ci").await, 1);
    assert!(cluster.leader.security_state().await.join_tokens.is_empty());
    assert_eq!(
        cluster.read_status(&cluster.leader_url, &secret).await,
        reqwest::StatusCode::OK
    );
}

/// A forwarded write the leader doesn't answer may still have committed. The
/// caller gets a 504 that says what that costs, for each route, because the
/// cost differs: a lost secret, nothing, or an orphan that expires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forwarded_write_that_times_out_says_what_it_may_have_cost() {
    let cluster = TokenCluster::start().await;
    let _silent = cluster.point_the_follower_at_a_silent_leader().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    let ask = |path: &'static str, body: serde_json::Value| {
        let request = client
            .post(format!("{}{path}", cluster.follower_url))
            .bearer_auth(&cluster.admin)
            .json(&body)
            .send();
        async move {
            let response = request.await.unwrap();
            (response.status(), response.text().await.unwrap())
        }
    };

    let (create, rotate, revoke, join) = tokio::join!(
        ask(
            "/v1/token/create",
            serde_json::json!({"name": "ci", "role": "deployer"})
        ),
        ask("/v1/token/rotate", serde_json::json!({"name": "ci"})),
        ask("/v1/token/revoke", serde_json::json!({"name": "ci"})),
        ask(
            "/v1/join-token/create",
            serde_json::json!({"node_id": "node-7"})
        ),
    );

    for (status, _) in [&create, &rotate, &revoke, &join] {
        assert_eq!(*status, reqwest::StatusCode::GATEWAY_TIMEOUT);
    }
    assert!(create.1.contains("relish token list"), "{}", create.1);
    assert!(rotate.1.contains("new secret"), "{}", rotate.1);
    assert!(rotate.1.contains("relish token list"), "{}", rotate.1);
    assert!(revoke.1.contains("safe"), "{}", revoke.1);
    assert!(join.1.contains("expires"), "{}", join.1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_scoped_bearer_has_matching_leader_apply_and_follower_batch_authority() {
    use crate::cluster_support;
    use crate::task_harness::TestTasks;
    use reliaburger::bun::agent::AgentCommand;
    use reliaburger::council::RaftRequest;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (nodes, _network) = cluster_support::build_council().await;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let cleanup_nodes = nodes.clone();
    let cleanup_signal = shutdown.clone();
    let cleanup = tokio::spawn(async move {
        cleanup_signal.cancelled().await;
        for node in cleanup_nodes {
            node.shutdown().await.unwrap();
        }
    });
    let _council_tasks = TestTasks::new(shutdown.clone(), vec![cleanup]);
    let leader_index = cluster_support::initial_voter_leader_index(&nodes)
        .await
        .unwrap();
    let follower_index = (0..3).find(|index| *index != leader_index).unwrap();
    let leader = nodes[leader_index].clone();
    let follower = nodes[follower_index].clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !leader.is_leader().await
            || follower.current_leader().await != Some(cluster_support::rid(leader_index))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("authority fixture did not establish the elected leader/follower");

    let names: Vec<String> = (0..6)
        .flat_map(|index| [format!("app-case-{index}"), format!("batch-case-{index}")])
        .collect();
    let created = reliaburger::sesame::token::create_token(
        "ci",
        reliaburger::sesame::types::ApiRole::Deployer,
        reliaburger::sesame::types::TokenScope {
            apps: Some(names),
            namespaces: Some(vec!["team".into()]),
        },
        None,
    )
    .unwrap();
    leader
        .write(RaftRequest::CreateApiToken(created.token.clone()))
        .await
        .unwrap();
    let tokens = reliaburger::sesame::auth::new_token_store();
    tokens.write().await.push(created.token);

    let leader_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let follower_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let leader_address = leader_listener.local_addr().unwrap();
    let follower_address = follower_listener.local_addr().unwrap();
    let leader_name = cluster_support::NAMES[leader_index];
    let follower_name = cluster_support::NAMES[follower_index];
    let membership = Arc::new(tokio::sync::RwLock::new(vec![
        reliaburger::bun::api::NodeMembershipInfo {
            node_id: reliaburger::meat::NodeId::new(leader_name),
            address: leader_address,
            api_advertised: true,
            jobs: None,
        },
        reliaburger::bun::api::NodeMembershipInfo {
            node_id: reliaburger::meat::NodeId::new(follower_name),
            address: follower_address,
            api_advertised: true,
            jobs: None,
        },
    ]));
    let node = reliaburger::meat::NodeId::new(leader_name);
    let mut report = reliaburger::reporting::aggregator::AggregatedState {
        leadership_epoch: Some(leader.current_term()),
        ..Default::default()
    };
    report.receive_deadlines.insert(
        node.clone(),
        tokio::time::Instant::now() + Duration::from_secs(30),
    );
    report.reports.insert(
        node.clone(),
        reliaburger::reporting::types::StateReport {
            node_id: node,
            timestamp: SystemTime::UNIX_EPOCH,
            running_apps: vec![],
            cached_specs: vec![],
            rollouts: Vec::new(),
            resource_usage: reliaburger::reporting::types::ResourceUsage {
                cpu_total_millicores: 8000,
                memory_total_mb: 16384,
                ..Default::default()
            },
            event_log: vec![],
            has_buildah: false,
            job_reservations: Default::default(),
            capacity_refusals: Vec::new(),
        },
    );
    let (reports, report_rx) = tokio::sync::watch::channel(report);
    let (commands, mut incoming) = tokio::sync::mpsc::channel(16);
    let dispatches = Arc::new(AtomicUsize::new(0));
    let counted_dispatches = dispatches.clone();
    let actor_shutdown = shutdown.clone();
    let actor = tokio::spawn(async move {
        loop {
            let command = tokio::select! { _ = actor_shutdown.cancelled() => break, command = incoming.recv() => command };
            match command {
                Some(AgentCommand::Status { response }) => {
                    let _ = response.send(Vec::new());
                }
                Some(_) => {
                    counted_dispatches.fetch_add(1, Ordering::SeqCst);
                    panic!("unexpected mutating command in workload authority fixture");
                }
                None => break,
            }
        }
    });
    let leader_router = shared_authority_router(
        leader.clone(),
        tokens.clone(),
        commands.clone(),
        membership.clone(),
        Some(report_rx),
        leader_name,
    );
    let expected_bearer = format!("Bearer {}", created.plaintext);
    let observed_principal = Arc::new(AtomicUsize::new(0));
    let observed = observed_principal.clone();
    let leader_router = leader_router.layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let expected_bearer = expected_bearer.clone();
            let observed = observed.clone();
            async move {
                if request.uri().path() == "/v1/apply" || request.uri().path() == "/v1/batch" {
                    assert!(
                        request
                            .headers()
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            == Some(expected_bearer.as_str()),
                        "leader received a different authority credential"
                    );
                    observed.fetch_add(1, Ordering::SeqCst);
                }
                next.run(request).await
            }
        },
    ));
    let follower_router = shared_authority_router(
        follower.clone(),
        tokens,
        commands.clone(),
        membership,
        None,
        follower_name,
    );
    let leader_stop = shutdown.clone();
    let leader_server = tokio::spawn(async move {
        axum::serve(leader_listener, leader_router)
            .with_graceful_shutdown(leader_stop.cancelled_owned())
            .await
            .unwrap();
    });
    let follower_stop = shutdown.clone();
    let follower_server = tokio::spawn(async move {
        axum::serve(follower_listener, follower_router)
            .with_graceful_shutdown(follower_stop.cancelled_owned())
            .await
            .unwrap();
    });
    let _http_tasks = TestTasks::new(
        shutdown.clone(),
        vec![actor, leader_server, follower_server],
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    for (index, (actions, fragment, allowed)) in [
        (vec!["logs"], "image='fixture:v1'", false),
        (vec!["deploy"], "exec='/bin/true'", false),
        (vec!["deploy"], "script='echo permitted'", false),
        (vec!["deploy"], "image='fixture:v1'", true),
        (vec!["deploy", "host-exec"], "exec='/bin/true'", true),
        (vec!["deploy", "host-exec"], "script='echo permitted'", true),
    ]
    .into_iter()
    .enumerate()
    {
        let grant = reliaburger::config::PermissionSpec {
            actions: actions.into_iter().map(str::to_owned).collect(),
            apps: vec!["*".into()],
            namespaces: None,
        };
        leader
            .write(RaftRequest::PermissionSpec {
                name: "ci".into(),
                spec: Box::new(grant.clone()),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while follower.desired_state().await.permissions.get("ci") != Some(&grant) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("follower did not receive current authoritative grant");
        reports.send_modify(|state| {
            state.leadership_epoch = Some(leader.current_term());
            for deadline in state.receive_deadlines.values_mut() {
                *deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            }
        });
        let mut cases = vec![(
            format!("app-case-{index}"),
            format!("batch-case-{index}"),
            "team",
            allowed,
        )];
        if index == 5 {
            // Grants permit deployment and host execution everywhere: these two
            // refusals therefore independently require app/namespace scope.
            cases.push((
                "outside-scope".into(),
                "outside-scope".into(),
                "team",
                false,
            ));
            cases.push((
                format!("app-case-{index}"),
                format!("batch-case-{index}"),
                "other",
                false,
            ));
        }
        for (app_name, job_name, namespace, allowed) in cases {
            let before = leader.desired_state().await;
            let dispatched_before = dispatches.load(Ordering::SeqCst);
            let principal_before = observed_principal.load(Ordering::SeqCst);
            let manifest = format!("[app.{app_name}]\nnamespace='{namespace}'\n{fragment}\n");
            let apply = client
                .post(format!("http://{leader_address}/v1/apply"))
                .bearer_auth(&created.plaintext)
                .body(manifest)
                .send()
                .await
                .unwrap();
            let apply_status = apply.status();
            let apply_body = apply.text().await.unwrap();
            let runtime = if fragment.starts_with("image") {
                ""
            } else {
                "runtime='process'\n"
            };
            let parsed = reliaburger::config::Config::parse(&format!(
                "[job.{job_name}]\nnamespace='{namespace}'\n{runtime}{fragment}\n"
            ))
            .unwrap();
            let batch = client.post(format!("http://{follower_address}/v1/batch")).bearer_auth(&created.plaintext).json(&serde_json::json!({"jobs":[{"name":job_name,"namespace":namespace,"spec":parsed.job[&job_name]}]})).send().await.unwrap();
            let batch_status = batch.status();
            if allowed {
                assert_eq!(apply_status, StatusCode::OK);
                assert!(
                    apply_body.contains("\"created\":1"),
                    "actual leader apply did not complete its desired write"
                );
                assert_eq!(batch_status, StatusCode::ACCEPTED);
                let batch: serde_json::Value = batch.json().await.unwrap();
                assert_eq!(batch["assigned"], 1);
                let id = batch["batch_id"].as_u64().unwrap();
                let desired = leader.desired_state().await;
                let parent = desired
                    .task_arrays
                    .manifest(id)
                    .expect("forwarded finite group is durable");
                assert_eq!(parent.cohorts.len(), 1);
                let run = desired.task_arrays.jobs().run(parent.cohorts[0].1).unwrap();
                assert_eq!(run.name, job_name);
                assert_eq!(run.namespace, namespace);
                assert!(!run.replay_unknown);
                assert_eq!(dispatches.load(Ordering::SeqCst), dispatched_before);
                assert_eq!(
                    observed_principal.load(Ordering::SeqCst),
                    principal_before + 2,
                    "follower batch did not reach the real leader with the same credential"
                );
                assert!(
                    leader
                        .desired_state()
                        .await
                        .apps
                        .contains_key(&reliaburger::meat::AppId::new(&app_name, namespace))
                );
            } else {
                assert_eq!(apply_status, StatusCode::FORBIDDEN);
                assert_eq!(batch_status, StatusCode::FORBIDDEN);
                // A finite read barrier drains previously enqueued commands;
                // zero-dispatch cannot pass merely because the actor is delayed.
                let (barrier, drained) = tokio::sync::oneshot::channel();
                commands
                    .send(AgentCommand::Status { response: barrier })
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(5), drained)
                    .await
                    .unwrap()
                    .unwrap();
                let after = leader.desired_state().await;
                assert_eq!(after.apps, before.apps);
                assert_eq!(
                    changed_desired_fields(&before, &after),
                    Vec::<String>::new(),
                    "denied pair changed desired state"
                );
                assert_eq!(
                    dispatches.load(Ordering::SeqCst),
                    dispatched_before,
                    "denied pair dispatched a workload command"
                );
            }
        }
    }
}

#[tokio::test]
async fn common_run_cancellation_and_output_obey_current_permissions() {
    use reliaburger::council::types::RaftRequest;
    use reliaburger::meat::task_array_store::TaskArrayWrite;
    let (council, router) = api_with_capacity(true).await;
    council
        .write(RaftRequest::TaskArray(Box::new(TaskArrayWrite::Register {
            name: "migration".into(),
            namespace: "default".into(),
            template: Box::new(toml::from_str("runtime='process'\nexec='/bin/true'").unwrap()),
            spec: reliaburger::meat::task_array::TaskArraySpec::with_count(1),
            submitted_at_epoch_secs: 1,
        })))
        .await
        .unwrap();
    for (actions, method, path) in [
        (vec!["logs"], "POST", "/v1/batch/1/cancel"),
        (vec!["deploy"], "GET", "/v1/batch/1/tasks/0/logs"),
    ] {
        council
            .write(RaftRequest::PermissionSpec {
                name: "ci".into(),
                spec: Box::new(reliaburger::config::PermissionSpec {
                    actions: actions.into_iter().map(str::to_owned).collect(),
                    apps: vec!["migration".into()],
                    namespaces: Some(vec!["default".into()]),
                }),
            })
            .await
            .unwrap();
        let before = council.desired_state().await;
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(owner());
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        // The router's leader loop may record a new cron minute meanwhile;
        // only a write the refused request made counts.
        assert_eq!(
            changed_desired_fields(&before, &council.desired_state().await),
            Vec::<String>::new(),
            "refused {method} {path} changed desired state"
        );
    }
}

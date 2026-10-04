//! Token issuance must reject invalid expiry before committing credentials.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use reliaburger::council::{
    CouncilConfig, CouncilNode, CouncilNodeInfo,
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
                resource_usage: reliaburger::reporting::types::ResourceUsage {
                    cpu_total_millicores: 8000,
                    memory_total_mb: 16384,
                    ..Default::default()
                },
                event_log: vec![],
                has_buildah: false,
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
            },
        ])))
    } else {
        membership
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        let _capacity_publisher = capacity_publisher;
        while let Some(command) = rx.recv().await {
            match command {
                reliaburger::bun::agent::AgentCommand::RunJobsWithLabels {
                    config,
                    execution_labels,
                    response,
                    ..
                } => {
                    assert_eq!(config.job.len(), execution_labels.len());
                    let _ = response.send(Ok(BTreeMap::new()));
                }
                reliaburger::bun::agent::AgentCommand::Status { response } => {
                    let _ = response.send(Vec::new());
                }
                _ => {}
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
    )
}

async fn create(router: Router, name: &str, ttl: Option<u64>) -> StatusCode {
    router
        .oneshot(
            Request::post("/v1/token/create")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"name": name, "role": "deployer", "ttl_days": ttl})
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
        (vec!["deploy"], serde_json::json!({"script":"echo denied"})),
        (vec!["deploy"], serde_json::json!({"exec":"/bin/true"})),
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
        assert!(council.desired_state().await.batch_state.get(1).is_none());
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
            serde_json::json!({"jobs":[{"name":"migration","spec":{"exec":"/bin/true"}}]})
                .to_string(),
        ))
        .unwrap();
    request.extensions_mut().insert(owner());
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(council.desired_state().await.batch_state.get(1).is_some());
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

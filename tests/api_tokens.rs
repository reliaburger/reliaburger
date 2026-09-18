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
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let router = reliaburger::bun::api::router_with_upgrade(
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
        None,
        0,
        None,
        None,
        None,
        "test".into(),
        None,
        900,
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
    );
    (council, router)
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

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
    let router = reliaburger::bun::api::router(
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

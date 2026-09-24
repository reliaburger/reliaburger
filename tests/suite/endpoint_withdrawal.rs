//! Withdrawal receipts bind committed cleanup to the authenticated original consumer.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use reliaburger::{
    council::{
        CouncilConfig, CouncilNode, CouncilNodeInfo, CouncilResponse, RaftRequest,
        log_store::MemLogStore,
        network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter},
        state_machine::CouncilStateMachine,
    },
    onion::{
        catalog::{CatalogBackend, EndpointCatalog},
        service_id::ServiceId,
    },
    sesame::{
        ca,
        renewal::TlsPeerCertificate,
        types::{CaRole, CrlEntry, SecurityState, SerialNumber},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, SystemTime},
};
use tower::ServiceExt;

const IKM: [u8; 32] = [42; 32];

async fn council(hierarchy: &ca::CaHierarchy, initialise: bool) -> Arc<CouncilNode> {
    let network = InMemoryRaftRouter::new();
    let node = Arc::new(
        CouncilNode::new(
            1,
            CouncilConfig {
                heartbeat_interval_ms: 50,
                election_timeout_min_ms: 150,
                election_timeout_max_ms: 300,
                ..Default::default()
            },
            InMemoryRaftNetworkFactory::new(1, network.clone()),
            MemLogStore::new(),
            CouncilStateMachine::new(),
            Some(IKM),
        )
        .await
        .unwrap(),
    );
    network.register(1, node.raft().clone()).await;
    if initialise {
        node.initialize(BTreeMap::from([(
            1,
            CouncilNodeInfo::new("127.0.0.1:9000".parse().unwrap(), "node"),
        )]))
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !node.is_leader().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        node.write(RaftRequest::SecurityStateInit(Box::new(SecurityState {
            certificate_authorities: vec![hierarchy.root.ca.clone(), hierarchy.node.ca.clone()],
            next_serial: 100,
            ..Default::default()
        })))
        .await
        .unwrap();
    }
    node
}

fn peer(hierarchy: &ca::CaHierarchy, name: &str, serial: u64) -> TlsPeerCertificate {
    let (leaf, _, _) = ca::issue_node_cert(
        name,
        SerialNumber(serial),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    TlsPeerCertificate(leaf.into())
}

fn router(council: Arc<CouncilNode>, peer: Option<TlsPeerCertificate>) -> Router {
    router_with_tokens(council, peer, None)
}

fn router_with_tokens(
    council: Arc<CouncilNode>,
    peer: Option<TlsPeerCertificate>,
    tokens: Option<reliaburger::sesame::auth::TokenStore>,
) -> Router {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let router = reliaburger::bun::api::router(
        tx,
        None,
        None,
        None,
        None,
        None,
        Some(council),
        tokens,
        Some("internal-token".into()),
        None,
        None,
        None,
        0,
        None,
    );
    match peer {
        Some(peer) => router.layer(axum::Extension(peer)),
        None => router,
    }
}

async fn seed_withdrawals(council: &CouncilNode) {
    for node_id in ["node", "offline"] {
        assert!(matches!(
            council
                .write(RaftRequest::RegisterEndpointConsumer {
                    node_id: node_id.into(),
                })
                .await
                .unwrap(),
            CouncilResponse::Applied { .. }
        ));
    }
    let first = EndpointCatalog::rebuild([(
        ServiceId::new("default", "web"),
        8080,
        vec![CatalogBackend {
            execution: None,
            node_id: "producer".into(),
            node_ip: "127.0.0.1".parse().unwrap(),
            host_port: 18080,
            healthy: true,
        }],
    )])
    .unwrap();
    let mut second = first.clone();
    second.services.get_mut("default__web").unwrap().backends[0].host_port = 18081;
    for (generation, catalog) in [first, second, EndpointCatalog::default()]
        .into_iter()
        .enumerate()
    {
        assert!(matches!(
            council
                .write(RaftRequest::PublishEndpoints {
                    expected_generation: generation as u64,
                    catalog: Box::new(catalog),
                })
                .await
                .unwrap(),
            CouncilResponse::Applied { .. }
        ));
    }
}

fn receipt(generation: u64) -> serde_json::Value {
    serde_json::json!({"compatibility": reliaburger::compatibility::CURRENT, "generation": generation})
}

async fn acknowledge(app: Router, body: serde_json::Value, bearer: &str) -> StatusCode {
    app.oneshot(
        Request::post("/v1/discovery/withdrawn")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

async fn poll_placements(app: Router, node: &str, bearer: &str) -> StatusCode {
    app.oneshot(
        Request::get(format!("/v1/placements/{node}"))
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

#[tokio::test]
async fn only_tls_authenticated_placement_polls_register_endpoint_consumers() {
    let hierarchy = ca::generate_ca_hierarchy("placements", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    // A plaintext consumer can never send a receipt, so it must never owe one.
    assert_eq!(
        poll_placements(router(council.clone(), None), "node", "internal-token").await,
        StatusCode::OK
    );
    assert!(
        council.desired_state().await.endpoint_consumers.is_empty(),
        "a plaintext poll registered an obligation nobody can discharge"
    );
    let authenticated = router(council.clone(), Some(peer(&hierarchy, "node", 10)));
    assert_eq!(
        poll_placements(authenticated, "node", "internal-token").await,
        StatusCode::OK
    );
    assert!(
        council
            .desired_state()
            .await
            .endpoint_consumers
            .contains("node")
    );
}

#[tokio::test]
async fn endpoint_receipts_discharge_only_the_authenticated_consumer_and_original_generation() {
    let hierarchy = ca::generate_ca_hierarchy("receipts", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    seed_withdrawals(&council).await;
    let app = router(council.clone(), Some(peer(&hierarchy, "node", 10)));
    let original = council.desired_state().await;
    for _ in 0..2 {
        assert_eq!(
            acknowledge(app.clone(), receipt(1), "internal-token").await,
            StatusCode::NO_CONTENT
        );
        let after = council.desired_state().await;
        assert_eq!(
            after.endpoint_withdrawals.pending[&1].consumers,
            BTreeSet::from(["offline".into()])
        );
        assert_eq!(
            after.endpoint_withdrawals.pending[&2],
            original.endpoint_withdrawals.pending[&2]
        );
        assert_eq!(after.endpoint_withdrawals.generation, 3);
        assert_eq!(after.endpoint_catalog, original.endpoint_catalog);
        assert_eq!(after.endpoint_consumers, original.endpoint_consumers);
    }
    council
        .write(RaftRequest::RegisterEndpointConsumer {
            node_id: "late".into(),
        })
        .await
        .unwrap();
    let before = council.desired_state().await.endpoint_withdrawals;
    assert_eq!(
        acknowledge(
            router(council.clone(), Some(peer(&hierarchy, "late", 12))),
            receipt(1),
            "internal-token"
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(council.desired_state().await.endpoint_withdrawals, before);
    let offline = router(council.clone(), Some(peer(&hierarchy, "offline", 11)));
    assert_eq!(
        acknowledge(offline.clone(), receipt(1), "internal-token").await,
        StatusCode::NO_CONTENT
    );
    assert!(
        !council
            .desired_state()
            .await
            .endpoint_withdrawals
            .pending
            .contains_key(&1)
    );
    // A delayed reply/retry for the old generation cannot discharge generation 2.
    let before = council.desired_state().await.endpoint_withdrawals;
    assert_eq!(
        acknowledge(app.clone(), receipt(1), "internal-token").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(council.desired_state().await.endpoint_withdrawals, before);
    assert_eq!(
        acknowledge(app, receipt(2), "internal-token").await,
        StatusCode::NO_CONTENT
    );
    assert!(
        council
            .desired_state()
            .await
            .endpoint_withdrawals
            .reserved_vips()
            .next()
            .is_some()
    );
    assert_eq!(
        acknowledge(offline, receipt(2), "internal-token").await,
        StatusCode::NO_CONTENT
    );
    let after = council.desired_state().await;
    assert!(after.endpoint_withdrawals.pending.is_empty());
    assert_eq!(after.endpoint_consumers.len(), 3);
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn endpoint_receipts_refuse_missing_foreign_revoked_or_forged_authority() {
    let hierarchy = ca::generate_ca_hierarchy("receipts-authority", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    seed_withdrawals(&council).await;
    let certificate = peer(&hierarchy, "node", 10);
    let app = router(council.clone(), Some(certificate.clone()));
    let before = council.desired_state().await.endpoint_withdrawals;
    assert_eq!(
        acknowledge(router(council.clone(), None), receipt(1), "internal-token").await,
        StatusCode::FORBIDDEN
    );
    assert!(matches!(
        acknowledge(app.clone(), receipt(1), "wrong-token").await,
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
    ));
    let foreign = ca::generate_ca_hierarchy("foreign", &IKM).unwrap();
    assert_eq!(
        acknowledge(
            router(council.clone(), Some(peer(&foreign, "node", 10))),
            receipt(1),
            "internal-token"
        )
        .await,
        StatusCode::FORBIDDEN
    );
    let mut forged = receipt(1);
    forged["node_id"] = "offline".into();
    assert_eq!(
        acknowledge(app.clone(), forged, "internal-token").await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut incompatible = receipt(1);
    incompatible["compatibility"]["protocol"] = 0.into();
    assert_eq!(
        acknowledge(app.clone(), incompatible, "internal-token").await,
        StatusCode::CONFLICT
    );
    for generation in [0, 3, 4, u64::MAX] {
        assert_eq!(
            acknowledge(app.clone(), receipt(generation), "internal-token").await,
            StatusCode::CONFLICT
        );
    }
    for field in ["compatibility", "generation"] {
        let mut incomplete = receipt(1);
        incomplete.as_object_mut().unwrap().remove(field);
        assert_eq!(
            acknowledge(app.clone(), incomplete, "internal-token").await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(
        acknowledge(
            router(council.clone(), Some(peer(&hierarchy, "unregistered", 13))),
            receipt(1),
            "internal-token"
        )
        .await,
        StatusCode::CONFLICT
    );
    council
        .write(RaftRequest::RevokeCertificate(CrlEntry {
            serial: SerialNumber(10),
            issuer: CaRole::Node,
            revoked_at: SystemTime::now(),
            reason: "revoked while connected".into(),
            expires_at: None,
        }))
        .await
        .unwrap();
    assert_eq!(
        acknowledge(app, receipt(1), "internal-token").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(council.desired_state().await.endpoint_withdrawals, before);
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn endpoint_receipts_refuse_without_current_leader_authority() {
    let hierarchy = ca::generate_ca_hierarchy("receipts-no-leader", &IKM).unwrap();
    let council = council(&hierarchy, false).await;
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(2),
            acknowledge(
                router(council.clone(), Some(peer(&hierarchy, "node", 10))),
                receipt(1),
                "internal-token"
            )
        )
        .await
        .unwrap(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(
        council
            .desired_state()
            .await
            .endpoint_withdrawals
            .pending
            .is_empty()
    );
    council.shutdown().await.unwrap();
}

fn producer_execution() -> reliaburger::grill::RuntimeExecution {
    serde_json::from_value(
        serde_json::json!({"instance_id": "default__web-0", "generation": "a".repeat(64)}),
    )
    .unwrap()
}

async fn retire_producer(app: Router, body: serde_json::Value, bearer: &str) -> StatusCode {
    app.oneshot(
        Request::post("/v1/discovery/retire")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

fn producer_request() -> serde_json::Value {
    serde_json::json!({"compatibility": reliaburger::compatibility::CURRENT, "execution": producer_execution()})
}

#[tokio::test]
async fn producer_release_requires_authenticated_identity_and_every_consumer_receipt() {
    let hierarchy = ca::generate_ca_hierarchy("producer-release", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    council
        .write(RaftRequest::RegisterEndpointConsumer {
            node_id: "offline".into(),
        })
        .await
        .unwrap();
    let catalog = EndpointCatalog::rebuild([(
        ServiceId::new("default", "web"),
        8080,
        vec![CatalogBackend {
            node_id: "producer".into(),
            node_ip: "127.0.0.1".parse().unwrap(),
            host_port: 18080,
            healthy: true,
            execution: Some(producer_execution()),
        }],
    )])
    .unwrap();
    council
        .write(RaftRequest::PublishEndpoints {
            expected_generation: 0,
            catalog: Box::new(catalog.clone()),
        })
        .await
        .unwrap();
    let app = router(council.clone(), Some(peer(&hierarchy, "producer", 10)));
    for _ in 0..2 {
        assert_eq!(
            retire_producer(app.clone(), producer_request(), "internal-token").await,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            council
                .desired_state()
                .await
                .endpoint_withdrawals
                .generation,
            2
        );
    }
    assert_eq!(
        acknowledge(
            router(council.clone(), Some(peer(&hierarchy, "offline", 11))),
            receipt(1),
            "internal-token"
        )
        .await,
        StatusCode::NO_CONTENT
    );
    for _ in 0..2 {
        assert_eq!(
            retire_producer(app.clone(), producer_request(), "internal-token").await,
            StatusCode::OK
        );
    }
    assert!(matches!(
        council
            .write(RaftRequest::PublishEndpoints {
                expected_generation: 2,
                catalog: Box::new(catalog)
            })
            .await
            .unwrap(),
        CouncilResponse::Refused { .. }
    ));
}

#[tokio::test]
async fn producer_release_refuses_missing_forged_retired_and_incompatible_authority() {
    let hierarchy = ca::generate_ca_hierarchy("producer-authority", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    let app = router(council.clone(), Some(peer(&hierarchy, "producer", 10)));
    let before = serde_json::to_value(council.desired_state().await).unwrap();
    assert_eq!(
        retire_producer(
            router(council.clone(), None),
            producer_request(),
            "internal-token"
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert!(
        !retire_producer(app.clone(), producer_request(), "wrong-token")
            .await
            .is_success()
    );
    let foreign = ca::generate_ca_hierarchy("foreign", &IKM).unwrap();
    assert_eq!(
        retire_producer(
            router(council.clone(), Some(peer(&foreign, "producer", 11))),
            producer_request(),
            "internal-token"
        )
        .await,
        StatusCode::FORBIDDEN
    );
    let mut forged = producer_request();
    forged["node_id"] = "victim".into();
    assert_eq!(
        retire_producer(app.clone(), forged, "internal-token").await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut incompatible = producer_request();
    incompatible["compatibility"]["state"] = 0.into();
    assert_eq!(
        retire_producer(app.clone(), incompatible, "internal-token").await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        serde_json::to_value(council.desired_state().await).unwrap(),
        before
    );
    let retired = council
        .write(RaftRequest::DecommissionNode {
            node_id: "producer".into(),
            retired_by: "operator".into(),
            reason: "fenced".into(),
            retired_at_unix_ms: 1,
            membership_log_id: *council.desired_state().await.last_membership.log_id(),
        })
        .await
        .unwrap();
    assert!(matches!(
        retired,
        CouncilResponse::NodeDecommissioned { .. }
    ));
    assert_eq!(
        retire_producer(app, producer_request(), "internal-token").await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn producer_release_without_a_quorum_is_not_confirmation() {
    let hierarchy = ca::generate_ca_hierarchy("producer-no-quorum", &IKM).unwrap();
    let council = council(&hierarchy, false).await;
    let app = router(council, Some(peer(&hierarchy, "producer", 10)));
    assert_eq!(
        retire_producer(app, producer_request(), "internal-token").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

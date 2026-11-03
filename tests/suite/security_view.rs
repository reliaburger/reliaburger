//! A node with no Raft seat gets its security state from the leader (#1477).
//!
//! Without it, an mTLS node past the council cap never received API tokens,
//! so its listener stayed closed and Bun restarted every 30 s, and a
//! revocation or a rotated CA never reached it.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use reliaburger::{
    council::{
        CouncilConfig, CouncilNode, CouncilNodeInfo, RaftRequest,
        log_store::MemLogStore,
        network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter},
        state_machine::CouncilStateMachine,
    },
    sesame::{
        ca,
        renewal::TlsPeerCertificate,
        security_view::SecurityView,
        types::{ApiRole, ApiToken, CrlEntry, SecurityState, SerialNumber, TokenScope},
    },
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tower::ServiceExt;

const IKM: [u8; 32] = [42; 32];

async fn council(hierarchy: &ca::CaHierarchy, lead: bool) -> Arc<CouncilNode> {
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
    if lead {
        node.initialize(BTreeMap::from([(
            1,
            CouncilNodeInfo::new("127.0.0.1:9000".parse().unwrap(), "leader"),
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
            certificate_authorities: vec![
                hierarchy.root.ca.clone(),
                hierarchy.node.ca.clone(),
                hierarchy.ingress.ca.clone(),
            ],
            api_tokens: vec![token("ops"), token("ci")],
            next_serial: 100,
            ..Default::default()
        })))
        .await
        .unwrap();
    }
    node
}

fn token(name: &str) -> ApiToken {
    ApiToken {
        name: name.into(),
        token_hash: name.as_bytes().to_vec(),
        role: ApiRole::Admin,
        scope: TokenScope::default(),
        expires_at: None,
        created_at: std::time::SystemTime::now(),
        previous_secret: None,
    }
}

fn names(tokens: &[ApiToken]) -> Vec<&str> {
    tokens.iter().map(|token| token.name.as_str()).collect()
}

/// The certificate a node past the cap presents: a current leaf for
/// `node-8`, signed by the Node CA.
fn peer(hierarchy: &ca::CaHierarchy, serial: u64) -> TlsPeerCertificate {
    let (leaf, _, _) = ca::issue_node_cert(
        "node-8",
        SerialNumber(serial),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    TlsPeerCertificate(leaf.into())
}

fn router(council: Arc<CouncilNode>, peer: Option<TlsPeerCertificate>) -> Router {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let router = reliaburger::bun::api::router(
        tx,
        None,
        None,
        None,
        None,
        None,
        Some(council),
        None,
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

async fn get_view(router: Router, token: Option<&str>) -> (StatusCode, Option<SecurityView>) {
    let mut request = Request::get("/v1/cluster/security-view");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).ok())
}

#[tokio::test]
async fn the_leader_serves_a_node_its_view_and_nothing_that_issues_identity() {
    let hierarchy = ca::generate_ca_hierarchy("view", &IKM).unwrap();
    let leader = council(&hierarchy, true).await;

    let (status, view) = get_view(
        router(Arc::clone(&leader), Some(peer(&hierarchy, 10))),
        Some("internal-token"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let view = view.unwrap().security;
    assert_eq!(names(&view.api_tokens), ["ops", "ci"]);
    for ca in &view.certificate_authorities {
        assert_eq!(
            ca.private_key_wrapped.is_some(),
            ca.role == reliaburger::sesame::types::CaRole::Ingress,
            "{:?}",
            ca.role
        );
    }
    assert_eq!(view.next_serial, 0);
    leader.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_view_needs_a_current_node_certificate_and_the_service_token() {
    let hierarchy = ca::generate_ca_hierarchy("view", &IKM).unwrap();
    let leader = council(&hierarchy, true).await;

    let (status, _) = get_view(router(Arc::clone(&leader), None), Some("internal-token")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no node certificate");

    let (status, _) = get_view(
        router(Arc::clone(&leader), Some(peer(&hierarchy, 10))),
        None,
    )
    .await;
    assert_ne!(status, StatusCode::OK, "no service token");

    let other = ca::generate_ca_hierarchy("other", &IKM).unwrap();
    let (status, _) = get_view(
        router(Arc::clone(&leader), Some(peer(&other, 10))),
        Some("internal-token"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a leaf from another cluster");

    leader
        .write(RaftRequest::RevokeCertificate(CrlEntry {
            serial: SerialNumber(11),
            issuer: reliaburger::sesame::types::CaRole::Node,
            revoked_at: std::time::SystemTime::now(),
            reason: "test".into(),
            expires_at: None,
        }))
        .await
        .unwrap();
    let (status, _) = get_view(
        router(Arc::clone(&leader), Some(peer(&hierarchy, 11))),
        Some("internal-token"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a revoked leaf");
    leader.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_node_without_a_seat_refuses_to_serve_a_view() {
    let hierarchy = ca::generate_ca_hierarchy("view", &IKM).unwrap();
    let outsider = council(&hierarchy, false).await;
    let (status, _) = get_view(
        router(Arc::clone(&outsider), Some(peer(&hierarchy, 10))),
        Some("internal-token"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    outsider.shutdown().await.unwrap();
}

/// What the node does with the view: its council handle serves it to
/// every reader while its own replica holds nothing, and the next view
/// replaces it, so a token revoked on the leader stops working there too.
#[tokio::test]
async fn a_node_without_a_seat_reads_the_leaders_view_and_follows_revocations() {
    let hierarchy = ca::generate_ca_hierarchy("view", &IKM).unwrap();
    let leader = council(&hierarchy, true).await;
    let outsider = council(&hierarchy, false).await;
    assert!(!outsider.holds_security_state().await);
    assert!(outsider.api_tokens().await.is_empty());

    let view = || async {
        get_view(
            router(Arc::clone(&leader), Some(peer(&hierarchy, 10))),
            Some("internal-token"),
        )
        .await
        .1
        .unwrap()
        .security
    };
    outsider.adopt_leader_security_view(view().await).await;
    assert_eq!(names(&outsider.api_tokens().await), ["ops", "ci"]);
    assert_eq!(
        outsider
            .security_state()
            .await
            .certificate_authorities
            .len(),
        3
    );

    leader
        .write(RaftRequest::RevokeApiToken { name: "ci".into() })
        .await
        .unwrap();
    outsider.adopt_leader_security_view(view().await).await;
    assert_eq!(names(&outsider.api_tokens().await), ["ops"]);

    // A node whose replica holds the state reads its own, whatever it was given.
    leader
        .adopt_leader_security_view(SecurityState::default())
        .await;
    assert!(leader.holds_security_state().await);
    assert_eq!(
        leader.security_state().await.certificate_authorities.len(),
        3
    );
    leader.shutdown().await.unwrap();
    outsider.shutdown().await.unwrap();
}

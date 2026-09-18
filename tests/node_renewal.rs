//! Node renewal requires a current TLS identity and quorum-backed issuance.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reliaburger::{
    council::{
        CouncilConfig, CouncilNode, CouncilNodeInfo, RaftRequest,
        log_store::MemLogStore,
        network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter},
        state_machine::CouncilStateMachine,
    },
    sesame::{
        ca, cert,
        renewal::{self, RenewalError, RenewalRequest, TlsPeerCertificate},
        types::{CaRole, CrlEntry, SecurityState, SerialNumber},
    },
};
use std::{
    collections::BTreeMap,
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

fn peer(hierarchy: &ca::CaHierarchy, serial: u64) -> TlsPeerCertificate {
    let (leaf, _, _) = ca::issue_node_cert(
        "node",
        SerialNumber(serial),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    TlsPeerCertificate(leaf.into())
}

fn request(node: &str) -> (RenewalRequest, Vec<u8>) {
    let (csr, key) = ca::create_node_csr(node).unwrap();
    (
        RenewalRequest {
            compatibility: reliaburger::compatibility::CURRENT,
            csr_b64: BASE64.encode(csr),
        },
        key,
    )
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

async fn post(
    router: Router,
    request: &RenewalRequest,
    token: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::post("/v1/cluster/renew").header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    router
        .oneshot(
            builder
                .body(Body::from(serde_json::to_vec(request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn renewal_allocates_distinct_committed_serials_and_keeps_the_requesters_key() {
    let hierarchy = ca::generate_ca_hierarchy("renewal", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    let peer = peer(&hierarchy, 10);
    let (first, first_key) = request("node");
    let (second, second_key) = request("node");
    let (one, two) = tokio::join!(
        renewal::issue_renewal(&council, &peer, &first),
        renewal::issue_renewal(&council, &peer, &second)
    );
    let one = one.unwrap();
    let two = two.unwrap();
    assert_ne!(one.serial, two.serial);
    assert_eq!(council.security_state().await.next_serial, 102);
    for (bundle, key) in [(one, first_key), (two, second_key)] {
        let identity = bundle.into_identity(key).unwrap();
        let dir = tempfile::tempdir().unwrap();
        reliaburger::sesame::identity_store::save(dir.path(), &identity).unwrap();
        assert_eq!(identity.node_id, "node");
        assert_eq!(identity.node_ca_der, hierarchy.node.ca.certificate_der);
    }
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn renewal_refuses_mismatched_csr_and_foreign_or_revoked_peer_before_allocating() {
    let hierarchy = ca::generate_ca_hierarchy("renewal", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    let valid = peer(&hierarchy, 10);
    let (wrong, _) = request("different-node");
    assert!(matches!(
        renewal::issue_renewal(&council, &valid, &wrong).await,
        Err(RenewalError::Request(_))
    ));
    let (request, _) = request("node");
    let foreign = peer(&ca::generate_ca_hierarchy("foreign", &IKM).unwrap(), 10);
    assert!(matches!(
        renewal::issue_renewal(&council, &foreign, &request).await,
        Err(RenewalError::Identity(_))
    ));
    council
        .write(RaftRequest::RevokeCertificate(CrlEntry {
            serial: SerialNumber(10),
            issuer: CaRole::Node,
            revoked_at: SystemTime::now(),
            reason: "revoked after connection establishment".into(),
            expires_at: None,
        }))
        .await
        .unwrap();
    assert!(matches!(
        renewal::issue_renewal(&council, &valid, &request).await,
        Err(RenewalError::Identity(_))
    ));
    assert_eq!(council.security_state().await.next_serial, 100);
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn renewal_requires_both_system_auth_and_tls_identity_and_rechecks_existing_connections() {
    let hierarchy = ca::generate_ca_hierarchy("renewal", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    let peer = peer(&hierarchy, 10);
    let (request, _) = request("node");
    let with_peer = router(council.clone(), Some(peer));
    assert_eq!(
        post(with_peer.clone(), &request, None).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            router(council.clone(), None),
            &request,
            Some("internal-token")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(with_peer.clone(), &request, Some("internal-token"))
            .await
            .status(),
        StatusCode::OK
    );
    council
        .write(RaftRequest::RevokeCertificate(CrlEntry {
            serial: SerialNumber(10),
            issuer: CaRole::Node,
            revoked_at: SystemTime::now(),
            reason: "revoked after connection establishment".into(),
            expires_at: None,
        }))
        .await
        .unwrap();
    assert_eq!(
        post(with_peer, &request, Some("internal-token"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn renewal_refuses_a_member_without_current_quorum_authority() {
    let hierarchy = ca::generate_ca_hierarchy("renewal", &IKM).unwrap();
    let council = council(&hierarchy, false).await;
    let (request, _) = request("node");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        renewal::issue_renewal(&council, &peer(&hierarchy, 10), &request),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(RenewalError::Unavailable(_))));
    assert_eq!(council.security_state().await.next_serial, 0);
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn renewal_rejects_an_expired_leaf_even_when_the_connection_used_to_be_valid() {
    let hierarchy = ca::generate_ca_hierarchy("renewal", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.subject_alt_names = vec![rcgen::SanType::URI(
        ca::node_spiffe_uri("node").try_into().unwrap(),
    )];
    params.serial_number = Some(10u64.into());
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::seconds(90);
    params.not_after = time::OffsetDateTime::now_utc() - time::Duration::seconds(5);
    let issuer = hierarchy
        .node
        .certificate_params
        .clone()
        .self_signed(&hierarchy.node.signing_keypair)
        .unwrap();
    let leaf = params
        .signed_by(&key, &issuer, &hierarchy.node.signing_keypair)
        .unwrap();
    assert!(cert::check_validity(leaf.der()).is_err());
    let (request, _) = request("node");
    let peer = TlsPeerCertificate(leaf.der().clone());
    assert!(matches!(
        renewal::issue_renewal(&council, &peer, &request).await,
        Err(RenewalError::Identity(_))
    ));
    assert_eq!(council.security_state().await.next_serial, 100);
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn renewal_refuses_malformed_or_incompatible_requests_without_spending_serials() {
    let hierarchy = ca::generate_ca_hierarchy("renewal", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    let valid = peer(&hierarchy, 10);
    for csr_b64 in [
        "not base64".to_string(),
        BASE64.encode(b"not a CSR"),
        "A".repeat(16 * 1024 + 1),
    ] {
        let malformed = RenewalRequest {
            compatibility: reliaburger::compatibility::CURRENT,
            csr_b64,
        };
        assert!(matches!(
            renewal::issue_renewal(&council, &valid, &malformed).await,
            Err(RenewalError::Request(_))
        ));
    }
    let (request, _) = request("node");
    let mut old_protocol = request.clone();
    old_protocol.compatibility.protocol = 0;
    assert!(matches!(
        renewal::issue_renewal(&council, &valid, &old_protocol).await,
        Err(RenewalError::Request(_))
    ));
    let mut incompatible = serde_json::to_value(&request).unwrap();
    // An absent required compatibility field must fail deserialisation.
    incompatible
        .as_object_mut()
        .unwrap()
        .remove("compatibility");
    assert!(serde_json::from_value::<RenewalRequest>(incompatible).is_err());
    let mut oversized = request;
    oversized.csr_b64 = "A".repeat(16 * 1024 + 1);
    assert_eq!(
        post(
            router(council.clone(), Some(valid)),
            &oversized,
            Some("internal-token")
        )
        .await
        .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(council.security_state().await.next_serial, 100);
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn renewal_refuses_revoked_node_and_root_issuers() {
    for role in [CaRole::Node, CaRole::Root] {
        let hierarchy = ca::generate_ca_hierarchy("renewal", &IKM).unwrap();
        let council = council(&hierarchy, true).await;
        let issuer_serial = council.security_state().await.get_ca(role).unwrap().serial;
        council
            .write(RaftRequest::RevokeCertificate(CrlEntry {
                serial: issuer_serial,
                issuer: role,
                revoked_at: SystemTime::now(),
                reason: "issuer revoked".into(),
                expires_at: None,
            }))
            .await
            .unwrap();
        let (request, _) = request("node");
        assert!(matches!(
            renewal::issue_renewal(&council, &peer(&hierarchy, 10), &request).await,
            Err(RenewalError::Identity(_))
        ));
        assert_eq!(council.security_state().await.next_serial, 100);
        council.shutdown().await.unwrap();
    }
}

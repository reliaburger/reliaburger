//! Node renewal requires a current TLS identity and quorum-backed issuance.
//!
//! The last test drives the real `bun` binary: a node whose leaf is due
//! renews it at startup and reuses the renewed leaf after a restart.

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
    time::{Duration, Instant, SystemTime},
};
use tower::ServiceExt;

use crate::bun_process;
use bun_process::{
    BunProcess, WAIT, assert_success, reserve_address, reserve_ports, run_relish,
    spawn_bun_with_port_retry, wait_for_relish,
};

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

fn worker_identity(
    hierarchy: &ca::CaHierarchy,
    serial: u64,
    due: bool,
) -> reliaburger::sesame::identity_store::NodeIdentity {
    let (certificate_der, private_key_der) = if due {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::default();
        params.subject_alt_names = vec![rcgen::SanType::URI(
            ca::node_spiffe_uri("node").try_into().unwrap(),
        )];
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        params.serial_number = Some(serial.into());
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::seconds(300);
        params.not_after = now + time::Duration::seconds(120);
        let issuer = hierarchy
            .node
            .certificate_params
            .clone()
            .self_signed(&hierarchy.node.signing_keypair)
            .unwrap();
        (
            params
                .signed_by(&key, &issuer, &hierarchy.node.signing_keypair)
                .unwrap()
                .der()
                .to_vec(),
            key.serialize_der(),
        )
    } else {
        let (cert, key, _) = ca::issue_node_cert(
            "node",
            SerialNumber(serial),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        (cert, key)
    };
    reliaburger::sesame::identity_store::NodeIdentity {
        node_id: "node".into(),
        certificate_der,
        private_key_der,
        serial: SerialNumber(serial),
        ca_generation: 0,
        node_ca_der: hierarchy.node.ca.certificate_der.clone(),
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        not_before: SystemTime::UNIX_EPOCH,
        not_after: SystemTime::UNIX_EPOCH,
    }
}

struct WorkerFixture {
    council: Arc<CouncilNode>,
    live: reliaburger::sesame::credentials::LiveNodeIdentity,
    directory: tempfile::TempDir,
    tasks: tokio::task::JoinSet<()>,
    shutdown: tokio_util::sync::CancellationToken,
    mode: Arc<std::sync::atomic::AtomicU8>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    address: std::net::SocketAddr,
}

impl Drop for WorkerFixture {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.tasks.abort_all();
    }
}

impl WorkerFixture {
    async fn new(hierarchy: &ca::CaHierarchy, due: bool) -> Self {
        use reliaburger::sesame::{credentials::LiveNodeIdentity, identity_store, mtls};
        use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
        let council = council(hierarchy, true).await;
        let directory = tempfile::tempdir().unwrap();
        identity_store::save(directory.path(), &worker_identity(hierarchy, 10, due)).unwrap();
        let live = LiveNodeIdentity::load(directory.path()).unwrap();
        let mode = Arc::new(AtomicU8::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let route_mode = mode.clone();
        let route_calls = calls.clone();
        let app = router(council.clone(), None).layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let mode = route_mode.clone();
                let calls = route_calls.clone();
                async move {
                    use axum::response::IntoResponse;
                    calls.fetch_add(1, Ordering::SeqCst);
                    match mode.load(Ordering::SeqCst) {
                        1 => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                        2 => (StatusCode::TEMPORARY_REDIRECT, [("location", "/leak")])
                            .into_response(),
                        3 => std::future::pending().await,
                        4 => (StatusCode::OK, "not a renewal bundle").into_response(),
                        5 => {
                            let response = next.run(request).await;
                            assert_eq!(response.status(), StatusCode::OK);
                            let body = axum::body::to_bytes(response.into_body(), 65536)
                                .await
                                .unwrap();
                            let mut value: serde_json::Value =
                                serde_json::from_slice(&body).unwrap();
                            // Valid bundle plus an ignored field: without the response
                            // limit this would install successfully instead of refusing.
                            value["padding"] = serde_json::Value::String("x".repeat(65536));
                            axum::Json(value).into_response()
                        }
                        _ => next.run(request).await,
                    }
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(
            mtls::build_live_api_server_config(&live, mtls::CrlHandle::default()).unwrap(),
        );
        let shutdown = tokio_util::sync::CancellationToken::new();
        let stop = shutdown.clone();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept() => {
                        let (tcp, _) = accepted.unwrap();
                        let acceptor = acceptor.clone(); let app = app.clone();
                        connections.spawn(async move {
                            let tls = acceptor.accept(tcp).await.unwrap();
                            let leaf = tls.get_ref().1.peer_certificates().unwrap()[0].clone();
                            let service = app.layer(axum::Extension(TlsPeerCertificate(leaf)));
                            let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                                .serve_connection(hyper_util::rt::TokioIo::new(tls), hyper_util::service::TowerToHyperService::new(service)).await;
                        });
                    }
                }
            }
        });
        Self {
            council,
            live,
            directory,
            tasks,
            shutdown,
            mode,
            calls,
            address,
        }
    }

    fn start(&mut self) -> reliaburger::sesame::renewal_worker::RenewalMonitor {
        let (worker, monitor) = reliaburger::sesame::renewal_worker::NodeRenewalWorker::new(
            self.live.clone(),
            reliaburger::sesame::mtls::CrlHandle::default(),
            "internal-token",
        )
        .unwrap();
        // Several tests drive two or three failures in a row. None asserts the
        // spacing, so the production five-second pause only adds wall time.
        let worker = worker.with_retry_delay(Duration::from_millis(200));
        let council = self.council.clone();
        let stop = self.shutdown.clone();
        let address = self.address;
        self.tasks.spawn(async move {
            worker
                .run(
                    council,
                    Arc::new(tokio::sync::RwLock::new(Vec::new())),
                    address,
                    stop,
                )
                .await;
        });
        monitor
    }
}

async fn wait_for_condition(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(12), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn worker_waits_until_midpoint_then_retries_failed_persistence_without_publishing() {
    use reliaburger::sesame::{identity_store, renewal_worker::RenewalState};
    use std::sync::atomic::Ordering;
    let hierarchy = ca::generate_ca_hierarchy("renewal", &IKM).unwrap();
    let mut fixture = WorkerFixture::new(&hierarchy, false).await;
    let monitor = fixture.start();
    wait_for_condition(|| monitor.state() == RenewalState::Valid).await;
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    fixture
        .live
        .replace(worker_identity(&hierarchy, 20, true))
        .await
        .unwrap();
    let key = fixture.directory.path().join("node.key");
    std::fs::remove_file(&key).unwrap();
    std::fs::create_dir(&key).unwrap();
    wait_for_condition(|| monitor.state() == RenewalState::Retrying).await;
    assert_eq!(fixture.live.snapshot().serial, SerialNumber(20));
    assert_eq!(
        identity_store::load(fixture.directory.path())
            .unwrap()
            .unwrap()
            .serial,
        SerialNumber(20)
    );
    std::fs::remove_dir(&key).unwrap();
    wait_for_condition(|| {
        fixture.live.snapshot().serial.0 >= 100 && monitor.state() == RenewalState::Valid
    })
    .await;
    assert_eq!(
        fixture.live.snapshot().serial,
        identity_store::load(fixture.directory.path())
            .unwrap()
            .unwrap()
            .serial
    );
    fixture.shutdown.cancel();
    wait_for_condition(|| monitor.state() == RenewalState::Stopped).await;
    fixture.council.shutdown().await.unwrap();
}

#[tokio::test]
async fn worker_refuses_redirects_then_recovers_from_a_retryable_member() {
    use reliaburger::sesame::renewal_worker::RenewalState;
    use std::sync::atomic::Ordering;
    let hierarchy = ca::generate_ca_hierarchy("renewal", &IKM).unwrap();
    let mut fixture = WorkerFixture::new(&hierarchy, true).await;
    fixture.mode.store(2, Ordering::SeqCst);
    let monitor = fixture.start();
    wait_for_condition(|| monitor.state() == RenewalState::Retrying).await;
    assert_eq!(fixture.live.snapshot().serial, SerialNumber(10));
    assert_eq!(fixture.council.security_state().await.next_serial, 100);
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        1,
        "the client followed a redirect"
    );
    fixture.mode.store(1, Ordering::SeqCst);
    wait_for_condition(|| fixture.calls.load(Ordering::SeqCst) >= 2).await;
    assert_eq!(fixture.live.snapshot().serial, SerialNumber(10));
    fixture.mode.store(0, Ordering::SeqCst);
    wait_for_condition(|| fixture.live.snapshot().serial.0 >= 100).await;
    fixture.shutdown.cancel();
    wait_for_condition(|| monitor.state() == RenewalState::Stopped).await;
    fixture.council.shutdown().await.unwrap();
}

#[tokio::test]
async fn worker_shutdown_cancels_an_inflight_request_and_exposes_stopped_health() {
    use reliaburger::sesame::renewal_worker::RenewalState;
    use std::sync::atomic::Ordering;
    let hierarchy = ca::generate_ca_hierarchy("renewal", &IKM).unwrap();
    let mut fixture = WorkerFixture::new(&hierarchy, true).await;
    fixture.mode.store(3, Ordering::SeqCst);
    let monitor = fixture.start();
    wait_for_condition(|| fixture.calls.load(Ordering::SeqCst) > 0).await;
    assert_eq!(monitor.state(), RenewalState::Renewing);
    fixture.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(1), async {
        while monitor.state() != RenewalState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.live.snapshot().serial, SerialNumber(10));
    fixture.council.shutdown().await.unwrap();
}

#[tokio::test]
async fn diagnostics_does_not_report_a_stopped_renewal_owner_as_healthy() {
    use reliaburger::bun::diagnostics::{DiagnosticSource, LocalDiagnosticSnapshot};
    use reliaburger::sesame::{
        credentials::LiveNodeIdentity, identity_store, mtls::CrlHandle,
        renewal_worker::NodeRenewalWorker,
    };
    let hierarchy = ca::generate_ca_hierarchy("renewal", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    let dir = tempfile::tempdir().unwrap();
    identity_store::save(dir.path(), &worker_identity(&hierarchy, 10, false)).unwrap();
    let live = LiveNodeIdentity::load(dir.path()).unwrap();
    let (worker, monitor) =
        NodeRenewalWorker::new(live.clone(), CrlHandle::default(), "internal-token").unwrap();
    let app = router(council.clone(), None)
        .layer(axum::Extension(live))
        .layer(axum::Extension(monitor));
    let mut worker = Some(worker);
    for (label, automatic) in [("starting", true), ("stopped", false)] {
        if !automatic {
            drop(worker.take());
        }
        let response = app
            .clone()
            .oneshot(Request::get("/v1/diagnostics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let snapshot: LocalDiagnosticSnapshot = serde_json::from_slice(&bytes).unwrap();
        let DiagnosticSource::Available { value, .. } = snapshot.certificates else {
            panic!("missing node certificate");
        };
        assert_eq!(value[0].rotation_state, label);
        assert_eq!(value[0].automatic_rotation, automatic);
    }
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn worker_keeps_its_identity_after_malformed_or_oversized_success_responses() {
    use reliaburger::sesame::{identity_store, renewal_worker::RenewalState};
    use std::sync::atomic::Ordering;
    for mode in [4, 5] {
        let hierarchy = ca::generate_ca_hierarchy("renewal", &IKM).unwrap();
        let mut fixture = WorkerFixture::new(&hierarchy, true).await;
        fixture.mode.store(mode, Ordering::SeqCst);
        let monitor = fixture.start();
        wait_for_condition(|| monitor.state() == RenewalState::Retrying).await;
        assert_eq!(fixture.live.snapshot().serial, SerialNumber(10));
        assert_eq!(
            identity_store::load(fixture.directory.path())
                .unwrap()
                .unwrap()
                .serial,
            SerialNumber(10)
        );
        fixture.shutdown.cancel();
        fixture.council.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn decommission_rejects_existing_connections_and_requires_fresh_enrolment() {
    use reliaburger::council::CouncilResponse;
    let hierarchy = ca::generate_ca_hierarchy("retirement", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    let (leaf, _, _) = ca::issue_node_cert(
        "old-worker",
        SerialNumber(10),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    let peer = TlsPeerCertificate(leaf.into());
    let (old_request, _) = request("old-worker");
    let connected = router(council.clone(), Some(peer.clone()));
    assert_eq!(
        post(connected.clone(), &old_request, Some("internal-token"))
            .await
            .status(),
        StatusCode::OK
    );
    let membership_log_id = *council.metrics().borrow().membership_config.log_id();
    assert!(matches!(
        council
            .write(RaftRequest::DecommissionNode {
                node_id: "old-worker".into(),
                retired_by: "operator".into(),
                reason: "externally fenced".into(),
                retired_at_unix_ms: 30,
                membership_log_id
            })
            .await
            .unwrap(),
        CouncilResponse::NodeDecommissioned { .. }
    ));
    let serial = council.security_state().await.next_serial;
    assert_eq!(
        post(connected, &old_request, Some("internal-token"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(matches!(
        renewal::issue_renewal(&council, &peer, &old_request).await,
        Err(RenewalError::Identity(_))
    ));
    assert_eq!(council.security_state().await.next_serial, serial);
    let state = council.security_state_linearizable().await.unwrap();
    let old_csr = BASE64.decode(&old_request.csr_b64).unwrap();
    assert!(matches!(
        reliaburger::sesame::join::sign_join_csr(
            &old_csr,
            "old-worker",
            SerialNumber(serial),
            &state,
            &IKM
        ),
        Err(reliaburger::sesame::join::JoinError::NodeRetired)
    ));
    let (token_text, token) =
        reliaburger::sesame::join::create_join_token(Duration::from_secs(60), "replacement-worker")
            .unwrap();
    council
        .write(RaftRequest::CreateJoinToken(token.clone()))
        .await
        .unwrap();
    let state = council.security_state_linearizable().await.unwrap();
    assert!(
        reliaburger::sesame::join::check_join_token(&token_text, "replacement-worker", &state)
            .is_ok()
    );
    let issued_serial = match council
        .write(RaftRequest::ConsumeJoinTokenForIssue {
            token_hash: token.token_hash,
        })
        .await
        .unwrap()
    {
        CouncilResponse::JoinTokenConsumed { serial } => serial,
        other => panic!("unexpected join result: {other:?}"),
    };
    let (csr, private_key) = ca::create_node_csr("replacement-worker").unwrap();
    let fresh = reliaburger::sesame::join::sign_join_csr(
        &csr,
        "replacement-worker",
        SerialNumber(issued_serial),
        &state,
        &IKM,
    )
    .unwrap();
    let enrolled = reliaburger::sesame::join::JoinBundle::from_result(&fresh)
        .into_identity(private_key)
        .unwrap();
    assert_eq!(enrolled.node_id, "replacement-worker");
    council.shutdown().await.unwrap();
}

#[test]
fn secure_bun_renews_a_due_node_leaf_and_reuses_it_after_restart() {
    use reliaburger::sesame::{bootstrap, ca, crypto, identity_store, types::CaRole};
    let root = tempfile::tempdir().unwrap();
    let cluster_dir = root.path().join("cluster");
    assert_success(
        &run_relish(&[
            "init",
            cluster_dir.to_str().unwrap(),
            "--cluster-name",
            "renewal-startup",
            "--node-id",
            "node-01",
        ]),
        "initialise renewal fixture",
    );
    let node_path = cluster_dir.join("reliaburger.toml");
    let mut node = reliaburger::config::NodeConfig::from_file(&node_path).unwrap();
    node.node.name = Some("node-01".into());
    node.network.advertise_address = Some("127.0.0.1".into());
    node.storage.data = root.path().join("data");
    node.storage.images = root.path().join("images");
    node.storage.logs = root.path().join("logs");
    node.storage.metrics = root.path().join("metrics");
    node.storage.volumes = root.path().join("volumes");
    node.images.registry_port = 0;
    let identity_dir = node.security.identity_dir.as_ref().unwrap().clone();
    let mut identity = identity_store::load(&identity_dir).unwrap().unwrap();
    let original_serial = identity.serial;
    let master =
        bootstrap::load_master_key(node.security.master_key_path.as_ref().unwrap()).unwrap();
    let state =
        bootstrap::load_bootstrap_state(node.security.bootstrap_path.as_ref().unwrap()).unwrap();
    let issuer = state.get_ca(CaRole::Node).unwrap();
    let ca_key = rustls::pki_types::PrivateKeyDer::try_from(
        crypto::unwrap_key(&master, issuer.private_key_wrapped.as_ref().unwrap()).unwrap(),
    )
    .unwrap();
    let ca_key =
        rcgen::KeyPair::from_der_and_sign_algo(&ca_key, &rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let issuer_params = rcgen::CertificateParams::from_ca_cert_der(
        &rustls::pki_types::CertificateDer::from(issuer.certificate_der.clone()),
    )
    .unwrap();
    let issuer = issuer_params.self_signed(&ca_key).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.serial_number = Some(identity.serial.0.into());
    params.subject_alt_names = vec![rcgen::SanType::URI(
        ca::node_spiffe_uri("node-01").try_into().unwrap(),
    )];
    params.extended_key_usages = vec![
        rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        rcgen::ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::seconds(300);
    params.not_after = now + time::Duration::seconds(120);
    identity.certificate_der = params
        .signed_by(&key, &issuer, &ca_key)
        .unwrap()
        .der()
        .to_vec();
    identity.private_key_der = key.serialize_der();
    identity_store::save(&identity_dir, &identity).unwrap();
    let (mut bun, address) = spawn_bun_with_port_retry(true, || {
        let [gossip, raft, reporting] = reserve_ports();
        node.cluster.gossip_port = gossip;
        node.cluster.raft_port = raft;
        node.cluster.reporting_port = reporting;
        std::fs::write(&node_path, toml::to_string_pretty(&node).unwrap()).unwrap();
        (
            node_path.clone(),
            reserve_address(),
            root.path().join("renewal-bun.log"),
        )
    });
    let deadline = Instant::now() + WAIT;
    let renewed = loop {
        bun.assert_running();
        let current = identity_store::load(&identity_dir).unwrap().unwrap();
        if current.serial.0 > original_serial.0 {
            break current;
        }
        assert!(
            Instant::now() < deadline,
            "Bun did not renew its due node certificate: {}",
            std::fs::read_to_string(&bun.log_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(renewed.not_after > std::time::SystemTime::now() + Duration::from_secs(24 * 3600));
    let endpoint = format!("https://{address}");
    let ca = cluster_dir.join("identity/root-ca.crt");
    wait_for_relish(
        &mut bun,
        &[
            "--endpoint",
            &endpoint,
            "--ca-cert",
            ca.to_str().unwrap(),
            "status",
        ],
    );
    drop(bun);
    let mut restarted = BunProcess::spawn(
        &node_path,
        address,
        true,
        root.path().join("renewal-restart.log"),
    );
    wait_for_relish(
        &mut restarted,
        &[
            "--endpoint",
            &endpoint,
            "--ca-cert",
            ca.to_str().unwrap(),
            "status",
        ],
    );
    let after_restart = identity_store::load(&identity_dir).unwrap().unwrap();
    assert_eq!(after_restart.certificate_der, renewed.certificate_der);
}

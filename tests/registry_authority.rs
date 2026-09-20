//! Registry proposals require service authority, a current TLS node identity and quorum.
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
    pickle::{
        authority::{
            REGISTRY_PROPOSAL_PATH, REGISTRY_QUERY_PATH, RegistryMutation, RegistryProposal,
            RegistryQuery, RegistryQueryRequest, RegistryQueryResponse, RegistryRetirement,
        },
        types::GcReport,
    },
    sesame::{
        ca,
        renewal::TlsPeerCertificate,
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

async fn propose(
    app: Router,
    body: &RegistryProposal,
    bearer: Option<&str>,
) -> axum::response::Response {
    let mut request =
        Request::post(REGISTRY_PROPOSAL_PATH).header("content-type", "application/json");
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    app.oneshot(
        request
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn registry_proposals_require_service_and_current_node_authority() {
    let hierarchy = ca::generate_ca_hierarchy("registry", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    let certificate = peer(&hierarchy, 10);
    let proposal = RegistryProposal {
        compatibility: reliaburger::compatibility::CURRENT,
        mutation: RegistryMutation::GarbageCollection(GcReport {
            node_id: reliaburger::cluster::identity::raft_id_from_name("node"),
            deleted_layers: vec![reliaburger::pickle::store::compute_sha256(b"orphan")],
        }),
    };
    assert_eq!(
        propose(
            router(council.clone(), None),
            &proposal,
            Some("internal-token")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert!(matches!(
        propose(
            router(council.clone(), Some(certificate.clone())),
            &proposal,
            Some("wrong-token")
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ));
    let mut forged = proposal.clone();
    if let RegistryMutation::GarbageCollection(report) = &mut forged.mutation {
        report.node_id = 99;
    }
    assert_eq!(
        propose(
            router(council.clone(), Some(certificate.clone())),
            &forged,
            Some("internal-token")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let mut incompatible = proposal.clone();
    incompatible.compatibility.protocol += 1;
    assert_eq!(
        propose(
            router(council.clone(), Some(certificate.clone())),
            &incompatible,
            Some("internal-token")
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let foreign = peer(&ca::generate_ca_hierarchy("foreign", &IKM).unwrap(), 10);
    assert_eq!(
        propose(
            router(council.clone(), Some(foreign)),
            &proposal,
            Some("internal-token")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let response = propose(
        router(council.clone(), Some(certificate.clone())),
        &proposal,
        Some("internal-token"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert!(
        matches!(serde_json::from_slice::<reliaburger::council::CouncilResponse>(&body).unwrap(), reliaburger::council::CouncilResponse::GcApproved { approved } if approved.len() == 1)
    );
    council
        .write(RaftRequest::RevokeCertificate(CrlEntry {
            serial: SerialNumber(10),
            issuer: CaRole::Node,
            revoked_at: SystemTime::now(),
            reason: "revoked during an existing connection".into(),
            expires_at: None,
        }))
        .await
        .unwrap();
    assert_eq!(
        propose(
            router(council.clone(), Some(certificate)),
            &proposal,
            Some("internal-token")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    council.shutdown().await.unwrap();
}

async fn query(
    app: Router,
    body: &RegistryQueryRequest,
    bearer: Option<&str>,
) -> axum::response::Response {
    let mut request = Request::post(REGISTRY_QUERY_PATH).header("content-type", "application/json");
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    app.oneshot(
        request
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn registry_queries_require_current_node_authority_and_only_return_its_ready_receipts() {
    use reliaburger::council::CouncilResponse;
    use reliaburger::testkit::lease::{TestLease, now_unix_millis};
    let hierarchy = ca::generate_ca_hierarchy("registry-queries", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    let certificate = peer(&hierarchy, 10);
    let node_id = reliaburger::cluster::identity::raft_id_from_name("node");
    let request = RegistryQueryRequest {
        compatibility: reliaburger::compatibility::CURRENT,
        node_id,
        query: RegistryQuery::Retirements,
    };
    assert_eq!(
        query(
            router(council.clone(), None),
            &request,
            Some("internal-token")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let app = router(council.clone(), Some(certificate.clone()));
    for bearer in [None, Some("wrong-token")] {
        assert!(matches!(
            query(app.clone(), &request, bearer).await.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ));
    }
    let mut invalid = request.clone();
    invalid.node_id = 99;
    assert_eq!(
        query(app.clone(), &invalid, Some("internal-token"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    invalid = request.clone();
    invalid.compatibility.protocol += 1;
    assert_eq!(
        query(app.clone(), &invalid, Some("internal-token"))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let foreign = peer(&ca::generate_ca_hierarchy("foreign", &IKM).unwrap(), 10);
    assert_eq!(
        query(
            router(council.clone(), Some(foreign)),
            &request,
            Some("internal-token")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    let now = now_unix_millis();
    let lease = TestLease::new(
        "query-run".into(),
        "token:ci".into(),
        "ci".into(),
        "rbtest-query-run".into(),
        now,
        now + 60_000,
    )
    .unwrap();
    assert!(matches!(
        council
            .write(RaftRequest::TestLeaseCreate(lease))
            .await
            .unwrap(),
        CouncilResponse::Applied { .. }
    ));
    for (repository, writer) in [
        ("rbtest-query-run/web", node_id),
        ("rbtest-query-run/other", 99),
    ] {
        assert!(matches!(
            council
                .write(RaftRequest::TestLeaseRegistryWriter {
                    lease_id: "query-run".into(),
                    repository: repository.into(),
                    node_id: writer,
                    owner_id: Some("token:ci".into()),
                    observed_at_unix_ms: now
                })
                .await
                .unwrap(),
            CouncilResponse::Applied { .. }
        ));
    }
    let lookup = RegistryQueryRequest {
        query: RegistryQuery::Lease {
            repository: "rbtest-query-run/web".into(),
        },
        ..request.clone()
    };
    assert!(
        matches!(read_query(app.clone(), &lookup).await, RegistryQueryResponse::Lease(Some(id)) if id == "query-run")
    );
    assert!(
        matches!(read_query(app.clone(), &request).await, RegistryQueryResponse::Retirements(items) if items.is_empty())
    );
    council
        .write(RaftRequest::TestLeaseBeginCleanup {
            lease_id: "query-run".into(),
        })
        .await
        .unwrap();
    assert!(matches!(
        read_query(app.clone(), &lookup).await,
        RegistryQueryResponse::Lease(None)
    ));
    assert!(
        matches!(read_query(app.clone(), &request).await, RegistryQueryResponse::Retirements(items) if items.is_empty())
    );
    council
        .write(RaftRequest::TestLeaseWorkloadsRetired {
            lease_id: "query-run".into(),
        })
        .await
        .unwrap();
    assert!(
        matches!(read_query(app.clone(), &request).await, RegistryQueryResponse::Retirements(items) if items == vec![RegistryRetirement { lease_id: "query-run".into(), repository: "rbtest-query-run/web".into() }])
    );
    council
        .write(RaftRequest::TestLeaseRegistryRetired {
            lease_id: "query-run".into(),
            repository: "rbtest-query-run/web".into(),
            node_id,
        })
        .await
        .unwrap();
    assert!(
        matches!(read_query(app.clone(), &request).await, RegistryQueryResponse::Retirements(items) if items.is_empty())
    );
    council
        .write(RaftRequest::RevokeCertificate(CrlEntry {
            serial: SerialNumber(10),
            issuer: CaRole::Node,
            revoked_at: SystemTime::now(),
            reason: "revoked on existing connection".into(),
            expires_at: None,
        }))
        .await
        .unwrap();
    assert_eq!(
        query(app, &request, Some("internal-token")).await.status(),
        StatusCode::FORBIDDEN
    );
    council.shutdown().await.unwrap();
}

async fn read_query(app: Router, request: &RegistryQueryRequest) -> RegistryQueryResponse {
    let response = query(app, request, Some("internal-token")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn worker_and_follower_pushes_commit_through_the_advertised_leader() {
    use reliaburger::mustard::{directory::NodeDirectory, message::LeaderHint};
    use reliaburger::pickle::{
        api::PickleState,
        authority::RegistryForwarder,
        registry_auth::{DEFAULT_UPLOAD_TTL, QuotaConfig, UploadSessions},
        store::{BlobStore, compute_sha256},
        types::ManifestCatalog,
    };
    use tokio::sync::{RwLock, watch};
    let hierarchy = ca::generate_ca_hierarchy("registry", &IKM).unwrap();
    let leader = council(&hierarchy, true).await;
    let follower = council(&hierarchy, false).await;
    let (address, server) =
        tls_server(router(leader.clone(), None), &hierarchy, "leader", 11).await;
    let (directory_tx, directory_rx) = watch::channel(NodeDirectory {
        leader: Some(LeaderHint {
            node_id: reliaburger::meat::NodeId::new("node"),
            term: 100,
            api_address: address,
            reporting_address: address,
        }),
        ..Default::default()
    });
    let client_identity = node_identity(&hierarchy, "node", 10);
    let tls = reliaburger::sesame::mtls::build_mtls_client_config(
        &client_identity,
        reliaburger::sesame::mtls::CrlHandle::default(),
    )
    .unwrap();
    let http = reliaburger::cluster::ClusterHttp::secure(
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .use_preconfigured_tls((*tls).clone())
            .build()
            .unwrap(),
    )
    .with_bearer(Some("internal-token".into()));
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(BlobStore::new(root.path().join("blobs")));
    let config = compute_sha256(b"config");
    store.write_blob(b"config", &config).unwrap();
    let body = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"digest": config.as_str(), "size": 6}, "layers": []
    }))
    .unwrap();
    let state = PickleState {
        store: store.clone(),
        catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
        node_raft_id: reliaburger::cluster::identity::raft_id_from_name("node"),
        council: None,
        forwarder: Some(RegistryForwarder::new(http, directory_rx)),
        persist_path: Some(root.path().join("catalog.json")),
        auth: None,
        require_read_auth: false,
        allow_unauthenticated_bootstrap: true,
        quota: QuotaConfig::default(),
        sessions: UploadSessions::new(DEFAULT_UPLOAD_TTL),
    };
    for (tag, council) in [("worker", None), ("follower", Some(follower.clone()))] {
        let mut state = state.clone();
        state.council = council;
        let response = reliaburger::pickle::api::router(state)
            .oneshot(
                Request::put(format!("/v2/ordinary/manifests/{tag}"))
                    .header("content-type", "application/vnd.oci.image.manifest.v1+json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let text = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{tag}: {}",
            String::from_utf8_lossy(&text)
        );
        assert!(
            leader
                .manifest_catalog()
                .await
                .get_manifest_by_tag("ordinary", tag)
                .is_some()
        );
    }
    // Losing the leader's route cannot silently change a worker to standalone.
    directory_tx.send(NodeDirectory::default()).unwrap();
    let response = reliaburger::pickle::api::router(state)
        .oneshot(
            Request::put("/v2/ordinary/manifests/unconfirmed")
                .header("content-type", "application/vnd.oci.image.manifest.v1+json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        leader
            .manifest_catalog()
            .await
            .get_manifest_by_tag("ordinary", "unconfirmed")
            .is_none()
    );
    assert!(store.has_blob(&config));
    server.abort();
    let _ = server.await;
    follower.shutdown().await.unwrap();
    leader.shutdown().await.unwrap();
}

fn node_identity(
    hierarchy: &ca::CaHierarchy,
    node: &str,
    serial: u64,
) -> reliaburger::sesame::identity_store::NodeIdentity {
    let (certificate_der, private_key_der, serial) = ca::issue_node_cert(
        node,
        SerialNumber(serial),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    reliaburger::sesame::identity_store::NodeIdentity {
        node_id: node.into(),
        certificate_der,
        private_key_der,
        serial,
        ca_generation: 0,
        node_ca_der: hierarchy.node.ca.certificate_der.clone(),
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        not_before: SystemTime::UNIX_EPOCH,
        not_after: SystemTime::UNIX_EPOCH,
    }
}

async fn tls_server(
    app: Router,
    hierarchy: &ca::CaHierarchy,
    node_name: &str,
    serial: u64,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let identity = node_identity(hierarchy, node_name, serial);
    let acceptor = tokio_rustls::TlsAcceptor::from(
        reliaburger::sesame::mtls::build_api_server_config(
            &identity,
            reliaburger::sesame::mtls::CrlHandle::default(),
        )
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = connections.join_next(), if !connections.is_empty() => {},
                accepted = listener.accept() => {
                    let (tcp, _) = accepted.unwrap();
                    let acceptor = acceptor.clone(); let app = app.clone();
                    connections.spawn(async move {
                        let tls = acceptor.accept(tcp).await.unwrap();
                        let leaf = tls.get_ref().1.peer_certificates().unwrap()[0].clone();
                        let app = app.layer(axum::Extension(TlsPeerCertificate(leaf)));
                        let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                            .serve_connection(hyper_util::rt::TokioIo::new(tls), hyper_util::service::TowerToHyperService::new(app)).await;
                    });
                }
            }
        }
    });
    (address, task)
}

#[tokio::test]
async fn registry_forwarding_recovers_after_election_and_refuses_lost_quorum() {
    use reliaburger::mustard::{directory::NodeDirectory, message::LeaderHint};
    use reliaburger::pickle::authority::RegistryForwarder;
    use tokio::sync::watch;
    let hierarchy = ca::generate_ca_hierarchy("registry-quorum", &IKM).unwrap();
    let network = InMemoryRaftRouter::new();
    let mut nodes = Vec::new();
    let mut members = BTreeMap::new();
    for id in 1..=3 {
        let node = Arc::new(
            CouncilNode::new(
                id,
                CouncilConfig::default(),
                InMemoryRaftNetworkFactory::new(id, network.clone()),
                MemLogStore::new(),
                CouncilStateMachine::new(),
                Some(IKM),
            )
            .await
            .unwrap(),
        );
        network.register(id, node.raft().clone()).await;
        members.insert(
            id,
            CouncilNodeInfo::new(
                format!("127.0.0.1:{}", 19000 + id).parse().unwrap(),
                format!("node-{id}"),
            ),
        );
        nodes.push(node);
    }
    nodes[0].initialize(members).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !nodes[0].is_leader().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    nodes[0]
        .write(RaftRequest::SecurityStateInit(Box::new(SecurityState {
            certificate_authorities: vec![hierarchy.root.ca.clone(), hierarchy.node.ca.clone()],
            next_serial: 2000,
            ..Default::default()
        })))
        .await
        .unwrap();
    let mut servers = Vec::new();
    let mut addresses = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let (address, server) = tls_server(
            router(node.clone(), None),
            &hierarchy,
            &format!("node-{}", index + 1),
            1000 + index as u64,
        )
        .await;
        addresses.push(address);
        servers.push(server);
    }
    let view = |index: usize| NodeDirectory {
        leader: Some(LeaderHint {
            node_id: reliaburger::meat::NodeId::new(format!("node-{}", index + 1)),
            term: nodes[index].metrics().borrow().current_term,
            api_address: addresses[index],
            reporting_address: addresses[index],
        }),
        ..Default::default()
    };
    let (directory_tx, directory_rx) = watch::channel(view(0));
    let tls = reliaburger::sesame::mtls::build_mtls_client_config(
        &node_identity(&hierarchy, "node", 10),
        reliaburger::sesame::mtls::CrlHandle::default(),
    )
    .unwrap();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .use_preconfigured_tls((*tls).clone())
        .build()
        .unwrap();
    let forwarder = RegistryForwarder::new(
        reliaburger::cluster::ClusterHttp::secure(client)
            .with_bearer(Some("internal-token".into())),
        directory_rx,
    );
    let mutation = RegistryMutation::GarbageCollection(GcReport {
        node_id: reliaburger::cluster::identity::raft_id_from_name("node"),
        deleted_layers: vec![reliaburger::pickle::store::compute_sha256(b"orphan")],
    });
    // TLS setup can overlap an election. Retry using the current route before
    // introducing faults, rather than assuming the bootstrap node still leads.
    let initial = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for (index, node) in nodes.iter().enumerate() {
                if node.current_leader().await == Some(index as u64 + 1) && node.is_leader().await {
                    directory_tx.send(view(index)).unwrap();
                    if matches!(
                        forwarder.write(None, mutation.clone()).await,
                        Ok(reliaburger::council::CouncilResponse::GcApproved { .. })
                    ) {
                        return index;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no quorum-backed registry route became available");
    assert!(
        matches!(forwarder.query(None, reliaburger::cluster::identity::raft_id_from_name("node"), RegistryQuery::Retirements).await.unwrap(), RegistryQueryResponse::Retirements(items) if items.is_empty())
    );
    let survivors: Vec<usize> = (0..3).filter(|index| *index != initial).collect();
    for index in &survivors {
        network
            .partition(initial as u64 + 1, *index as u64 + 1)
            .await;
    }
    let elected = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for &index in &survivors {
                if nodes[index].current_leader().await == Some(index as u64 + 1)
                    && nodes[index].is_leader().await
                {
                    return index;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        forwarder.write(None, mutation.clone()).await.is_err(),
        "isolated old leader accepted a proposal"
    );
    assert!(
        forwarder
            .query(
                None,
                reliaburger::cluster::identity::raft_id_from_name("node"),
                RegistryQuery::Retirements
            )
            .await
            .is_err(),
        "isolated old leader served ownership"
    );
    directory_tx.send(view(elected)).unwrap();
    assert!(
        matches!(forwarder.query(None, reliaburger::cluster::identity::raft_id_from_name("node"), RegistryQuery::Retirements).await.unwrap(), RegistryQueryResponse::Retirements(items) if items.is_empty())
    );
    assert!(matches!(
        forwarder.write(None, mutation.clone()).await.unwrap(),
        reliaburger::council::CouncilResponse::GcApproved { .. }
    ));
    network
        .partition(survivors[0] as u64 + 1, survivors[1] as u64 + 1)
        .await;
    assert!(
        forwarder.write(None, mutation).await.is_err(),
        "registry committed without quorum"
    );
    assert!(
        forwarder
            .query(
                None,
                reliaburger::cluster::identity::raft_id_from_name("node"),
                RegistryQuery::Retirements
            )
            .await
            .is_err(),
        "ownership read succeeded without quorum"
    );
    for server in servers {
        server.abort();
        let _ = server.await;
    }
    for node in nodes {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn registry_control_routes_bound_oversized_and_stalled_request_bodies() {
    let hierarchy = ca::generate_ca_hierarchy("registry-bounds", &IKM).unwrap();
    let council = council(&hierarchy, true).await;
    let app = router(council.clone(), Some(peer(&hierarchy, 10)));
    for (path, maximum) in [
        (
            REGISTRY_PROPOSAL_PATH,
            reliaburger::pickle::authority::MAX_REGISTRY_PROPOSAL_BYTES,
        ),
        (REGISTRY_QUERY_PATH, 16 * 1024),
    ] {
        let request = |body| {
            Request::post(path)
                .header("content-type", "application/json")
                .header("authorization", "Bearer internal-token")
                .body(body)
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(request(Body::from(vec![b'x'; maximum + 1])))
                .await
                .unwrap()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let stalled = Body::from_stream(futures_util::stream::pending::<
            Result<axum::body::Bytes, std::io::Error>,
        >());
        let response = tokio::time::timeout(
            Duration::from_secs(12),
            app.clone().oneshot(request(stalled)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    }
    council.shutdown().await.unwrap();
}

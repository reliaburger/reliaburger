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
    for mutation in [
        RegistryMutation::ClaimWriter {
            lease_id: "run".into(),
            repository: "rbtest-run/web".into(),
            node_id: 99,
            owner_id: None,
            observed_at_unix_ms: 1,
        },
        RegistryMutation::WriterRetired {
            lease_id: "run".into(),
            repository: "rbtest-run/web".into(),
            node_id: 99,
        },
    ] {
        let forged = RegistryProposal {
            mutation,
            ..proposal.clone()
        };
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
    }
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
        test_leases: Default::default(),
        repository_writers: Default::default(),
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
    // A different receiving node supplies its own TLS-bound proof; it never
    // trusts the sender's old HEAD result or publishes the sender's holder set.
    let receiving_identity = node_identity(&hierarchy, "receiver", 14);
    let receiving_tls = reliaburger::sesame::mtls::build_mtls_client_config(
        &receiving_identity,
        reliaburger::sesame::mtls::CrlHandle::default(),
    )
    .unwrap();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        "Bearer internal-token".parse().unwrap(),
    );
    let receiving_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .use_preconfigured_tls((*receiving_tls).clone())
        .default_headers(headers)
        .build()
        .unwrap();
    let receiving_root = tempfile::tempdir().unwrap();
    let mut receiving = state.clone();
    receiving.node_raft_id = reliaburger::cluster::identity::raft_id_from_name("receiver");
    receiving.catalog = Arc::new(RwLock::new(ManifestCatalog::default()));
    receiving.store = Arc::new(BlobStore::new(receiving_root.path().join("blobs")));
    receiving.persist_path = Some(receiving_root.path().join("catalog.json"));
    receiving.forwarder = Some(RegistryForwarder::new(
        reliaburger::cluster::ClusterHttp::secure(receiving_client.clone())
            .with_bearer(Some("internal-token".into())),
        directory_tx.subscribe(),
    ));
    receiving.auth = Some(reliaburger::sesame::auth::AuthState::new(
        reliaburger::sesame::auth::new_token_store(),
        Some("internal-token".into()),
    ));
    receiving.allow_unauthenticated_bootstrap = false;
    receiving.require_read_auth = true;
    let digest = compute_sha256(&body);
    receiving.store.write_blob(b"config", &config).unwrap();
    receiving.store.write_blob(&body, &digest).unwrap();
    let (receiving_address, receiving_server) = tls_server(
        reliaburger::pickle::api::router(receiving.clone()),
        &hierarchy,
        "receiver",
        15,
    )
    .await;
    let receiving_peer = reliaburger::pickle::replication::Peer {
        node_id: receiving.node_raft_id,
        base_url: format!("https://{receiving_address}"),
    };
    let tags_before = leader.manifest_catalog().await.tags.clone();
    let receipt = reliaburger::pickle::replication::confirm_peer_copy(
        &receiving_peer,
        "ordinary",
        &digest,
        &receiving_client,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(receipt.node_id, receiving.node_raft_id);
    assert!(
        receiving.catalog.read().await.manifests.is_empty(),
        "confirmation must not replay remote tags into a worker projection"
    );
    let catalogue = leader.manifest_catalog().await;
    assert_eq!(catalogue.tags, tags_before);
    for blob in [&config, &digest] {
        assert_eq!(
            catalogue.layer_holders(blob.as_str()),
            std::collections::BTreeSet::from([state.node_raft_id, receiving.node_raft_id])
        );
    }
    let forged = RegistryMutation::Copy(reliaburger::pickle::types::ImageCopyConfirmation {
        repository: "ordinary".into(),
        manifest_digest: digest.clone(),
        node_id: state.node_raft_id,
        lease_id: None,
        observed_gc_generation: 0,
        observed_at_unix_ms: reliaburger::testkit::lease::now_unix_millis(),
    });
    assert!(
        receiving
            .forwarder
            .as_ref()
            .unwrap()
            .write(None, forged)
            .await
            .is_err()
    );
    receiving_server.abort();
    let response = router(leader.clone(), None)
        .oneshot(
            Request::get("/v1/images")
                .header("authorization", "Bearer internal-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let listed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        listed["images"].as_array().unwrap().len(),
        1,
        "image list must use committed metadata, even without a local projection"
    );
    // A worker must refresh its fencing generation after physical collection,
    // including for the leased publication exercised later in this test.
    let orphan = compute_sha256(b"collectable orphan");
    state
        .store
        .write_blob(b"collectable orphan", &orphan)
        .unwrap();
    assert_eq!(
        state
            .collect_garbage(GcReport {
                node_id: state.node_raft_id,
                deleted_layers: vec![orphan.clone()]
            })
            .await
            .unwrap(),
        vec![orphan.clone()]
    );
    assert!(!state.store.has_blob(&orphan));
    assert_eq!(
        leader.desired_state().await.registry_gc_generations[&state.node_raft_id],
        1
    );
    let old_manifest = leader
        .manifest_catalog()
        .await
        .get_manifest_by_tag("ordinary", "worker")
        .unwrap()
        .clone();
    let stale = RegistryMutation::Manifest(Box::new(reliaburger::pickle::types::ManifestCommit {
        observed_gc_generation: 0,
        manifest: old_manifest,
        tag: "stale-after-gc".into(),
        holder_nodes: std::collections::BTreeSet::from([state.node_raft_id]),
    }));
    assert!(matches!(
        state
            .forwarder
            .as_ref()
            .unwrap()
            .write(None, stale)
            .await
            .unwrap(),
        reliaburger::council::CouncilResponse::RegistryPublicationStale
    ));
    assert!(
        leader
            .manifest_catalog()
            .await
            .get_manifest_by_tag("ordinary", "stale-after-gc")
            .is_none()
    );
    let response = reliaburger::pickle::api::router(state.clone())
        .oneshot(
            Request::put("/v2/ordinary/manifests/after-gc")
                .header("content-type", "application/vnd.oci.image.manifest.v1+json")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let read_tokens = reliaburger::sesame::auth::new_token_store();
    read_tokens.write().await.push(
        reliaburger::sesame::token::create_token(
            "reader",
            reliaburger::sesame::types::ApiRole::ReadOnly,
            Default::default(),
            None,
        )
        .unwrap()
        .token,
    );
    for local_council in [None, Some(follower.clone())] {
        let mut fresh = state.clone();
        fresh.catalog = Arc::new(RwLock::new(ManifestCatalog::default()));
        fresh.council = local_council;
        let public_reader = router_with_tokens(follower.clone(), None, Some(read_tokens.clone()))
            .layer(axum::Extension(
                reliaburger::pickle::authority::RegistryReadAuthority {
                    forwarder: fresh.forwarder.clone().unwrap(),
                    node_id: fresh.node_raft_id,
                },
            ));
        assert_eq!(
            public_reader
                .clone()
                .oneshot(Request::get("/v1/images").body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let response = public_reader
            .oneshot(
                Request::get("/v1/images")
                    .header("authorization", "Bearer internal-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let listed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(listed["images"].as_array().unwrap().len(), 1);
        assert_eq!(listed["images"][0]["repository"], "ordinary");
        let source = reliaburger::pickle::p2p::ClusterSource {
            state: fresh.clone(),
            members: None,
            registry_port: 0,
            peer_scheme: "https".into(),
            concurrency: 2,
            client: reqwest::Client::new(),
            upstream: None,
            pull_through: false,
            cache_recheck_secs: 300,
            fill_lock: tokio::sync::Mutex::new(()),
        };
        assert!(
            source
                .ensure_image_local_with_peers("ordinary", "worker", &[])
                .await
                .unwrap()
                .is_some(),
            "fresh workers/followers must resolve committed registry metadata"
        );
        let reader = reliaburger::pickle::api::router(fresh.clone());
        for path in ["/v2/ordinary/manifests/worker", "/v2/ordinary/tags/list"] {
            assert_eq!(
                reader
                    .clone()
                    .oneshot(Request::get(path).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
        }
        // A fresh projection must not turn an already-full repository into zero usage.
        fresh.quota = QuotaConfig {
            per_repository_bytes: 11,
            total_bytes: 0,
        };
        let response = reliaburger::pickle::api::router(fresh)
            .oneshot(
                Request::put("/v2/ordinary/manifests/over-quota")
                    .header("content-type", "application/vnd.oci.image.manifest.v1+json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    // The same actual TLS route owns and retires a worker's leased repository.
    let mut leased = state.clone();
    let publisher = reliaburger::sesame::token::create_token(
        "publisher",
        reliaburger::sesame::types::ApiRole::Deployer,
        Default::default(),
        None,
    )
    .unwrap();
    let principal = reliaburger::sesame::auth::authenticate(
        &publisher.plaintext,
        std::slice::from_ref(&publisher.token),
    )
    .unwrap();
    let tokens = reliaburger::sesame::auth::new_token_store();
    tokens.write().await.push(publisher.token);
    leased.auth = Some(reliaburger::sesame::auth::AuthState::new(
        tokens,
        Some("internal-token".into()),
    ));
    leased.allow_unauthenticated_bootstrap = false;
    let now = reliaburger::testkit::lease::now_unix_millis();
    let lease = reliaburger::testkit::lease::TestLease::new(
        "tls-run".into(),
        principal.principal_id,
        "publisher".into(),
        "rbtest-tls-run".into(),
        now,
        now + 60_000,
    )
    .unwrap();
    leader
        .write(RaftRequest::TestLeaseCreate(lease))
        .await
        .unwrap();
    let leased_app = reliaburger::pickle::api::router(leased.clone());
    let push = || {
        Request::put("/v2/rbtest-tls-run/web/manifests/latest")
            .header("content-type", "application/vnd.oci.image.manifest.v1+json")
            .header("authorization", format!("Bearer {}", publisher.plaintext))
            .header("x-reliaburger-test-lease", "tls-run")
            .body(Body::from(body.clone()))
            .unwrap()
    };
    let response = leased_app.clone().oneshot(push()).await.unwrap();
    let status = response.status();
    let detail = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&detail)
    );
    assert!(
        leader
            .manifest_catalog()
            .await
            .get_manifest_by_tag("rbtest-tls-run/web", "latest")
            .is_some()
    );
    leader
        .write(RaftRequest::TestLeaseBeginCleanup {
            lease_id: "tls-run".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        leased_app.oneshot(push()).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    leased.reap_registry_leases_once().await.unwrap();
    assert!(
        leased
            .catalog
            .read()
            .await
            .get_manifest_by_tag("rbtest-tls-run/web", "latest")
            .is_some()
    );
    leader
        .write(RaftRequest::TestLeaseWorkloadsRetired {
            lease_id: "tls-run".into(),
        })
        .await
        .unwrap();
    assert!(matches!(
        leader
            .write(RaftRequest::TestLeaseFinishCleanup {
                lease_id: "tls-run".into()
            })
            .await
            .unwrap(),
        reliaburger::council::CouncilResponse::Refused { .. }
    ));
    leased.reap_registry_leases_once().await.unwrap();
    assert!(
        leased
            .catalog
            .read()
            .await
            .get_manifest_by_tag("rbtest-tls-run/web", "latest")
            .is_none()
    );
    assert!(matches!(
        leader
            .write(RaftRequest::TestLeaseFinishCleanup {
                lease_id: "tls-run".into()
            })
            .await
            .unwrap(),
        reliaburger::council::CouncilResponse::Applied { .. }
    ));
    let catalog = leader.manifest_catalog().await;
    assert!(
        catalog
            .get_manifest_by_tag("rbtest-tls-run/web", "latest")
            .is_none()
    );
    assert!(catalog.get_manifest_by_tag("ordinary", "worker").is_some());
    assert!(store.has_blob(&config));

    // Losing the leader's route cannot silently change a worker to standalone.
    directory_tx.send(NodeDirectory::default()).unwrap();
    let public_reader = router_with_tokens(follower.clone(), None, Some(read_tokens.clone()))
        .layer(axum::Extension(
            reliaburger::pickle::authority::RegistryReadAuthority {
                forwarder: state.forwarder.clone().unwrap(),
                node_id: state.node_raft_id,
            },
        ));
    assert_eq!(
        public_reader
            .oneshot(
                Request::get("/v1/images")
                    .header("authorization", "Bearer internal-token")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        router(follower.clone(), None)
            .oneshot(
                Request::get("/v1/images")
                    .header("authorization", "Bearer internal-token")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
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
    for query in [
        RegistryQuery::Repository {
            repository: "ordinary".into(),
        },
        RegistryQuery::Usage {
            repository: "ordinary".into(),
        },
    ] {
        assert!(
            forwarder
                .query(
                    None,
                    reliaburger::cluster::identity::raft_id_from_name("node"),
                    query
                )
                .await
                .is_err(),
            "catalogue reads must refuse without quorum"
        );
    }
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

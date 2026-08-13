//! Reference-integrity tests for the Pickle registry (Phase 12b.1,
//! REG1/REG3/IMG1). Real axum registries on ephemeral ports, real HTTP
//! pushes; GC and heal run against the same catalogue the API writes.
//!
//! The headline scenario: push → GC past the grace window → the
//! manifest is still fetchable → a peer pull of the full image
//! succeeds. Before REG1 was fixed, GC deleted the manifest's own blob
//! as an orphan and the catalogue pointed at a 404.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::pickle::api::{PickleState, router};
use reliaburger::pickle::gc::{GcConfig, delete_approved, gc_candidates, referenced_digests};
use reliaburger::pickle::replication::Peer;
use reliaburger::pickle::store::{BlobStore, compute_sha256};
use reliaburger::pickle::types::{Digest, ManifestCatalog};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// A real Pickle registry serving over HTTP.
struct Registry {
    state: PickleState,
    addr: SocketAddr,
    shutdown: CancellationToken,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

impl Registry {
    async fn start(node_raft_id: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = PickleState {
            store: Arc::new(BlobStore::new(dir.path().join("blobs"))),
            catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
            node_raft_id,
            council: None,
            persist_path: None,
            auth: None,
            require_read_auth: false,
            allow_unauthenticated_bootstrap: true,
            quota: reliaburger::pickle::registry_auth::QuotaConfig::default(),
            sessions: reliaburger::pickle::registry_auth::UploadSessions::new(
                reliaburger::pickle::registry_auth::DEFAULT_UPLOAD_TTL,
            ),
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let app = router(state.clone());
        let serve_shutdown = shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
                .await
                .ok();
        });

        Self {
            state,
            addr,
            shutdown,
            dir,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn peer(&self, node_id: u64) -> Peer {
        Peer {
            node_id,
            base_url: self.base_url(),
        }
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Push a one-layer image over the wire, the same flow docker uses.
/// The layer bytes are `layer_seed` so distinct images get distinct
/// digests. Returns (manifest_bytes, manifest_digest).
async fn push_test_image(
    base_url: &str,
    repo: &str,
    tag: &str,
    layer_seed: &str,
) -> (Vec<u8>, Digest) {
    let client = reqwest::Client::new();

    let config_bytes = br#"{"architecture":"arm64"}"#.to_vec();
    let layer_bytes = format!("layer-bytes-{layer_seed}").into_bytes();
    let config_size = config_bytes.len();
    let layer_size = layer_bytes.len();
    let config_digest = compute_sha256(&config_bytes);
    let layer_digest = compute_sha256(&layer_bytes);

    for (digest, bytes) in [(&config_digest, config_bytes), (&layer_digest, layer_bytes)] {
        let response = client
            .post(format!(
                "{base_url}/v2/{repo}/blobs/uploads/?digest={digest}",
                digest = digest.as_str()
            ))
            .body(bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 201, "blob upload failed");
    }

    let manifest_bytes = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest.as_str(),
            "size": config_size,
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer_digest.as_str(),
            "size": layer_size,
        }],
    })
    .to_string()
    .into_bytes();
    let response = client
        .put(format!("{base_url}/v2/{repo}/manifests/{tag}"))
        .body(manifest_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201, "manifest put failed");

    let manifest_digest = compute_sha256(&manifest_bytes);
    (manifest_bytes, manifest_digest)
}

/// Run one full two-phase GC sweep on a registry with a zero orphan
/// grace window and no retention — the harshest legal configuration.
async fn run_gc(registry: &Registry) {
    let catalog_snapshot = registry.state.catalog.read().await.clone();
    let config = GcConfig {
        retain_days: 0,
        node_id: registry.state.node_raft_id,
        orphan_grace: Duration::ZERO,
    };
    let nominated = gc_candidates(
        &registry.state.store,
        &catalog_snapshot,
        &HashSet::new(),
        &config,
    )
    .unwrap();
    if let Some(report) = nominated.report(config.node_id) {
        let approved = registry
            .state
            .catalog
            .write()
            .await
            .apply_gc_report(&report);
        let referenced = referenced_digests(&registry.state.catalog.read().await.clone());
        delete_approved(&registry.state.store, &approved, &referenced);
    }
}

fn cluster_source_for(registry: &Registry) -> reliaburger::pickle::p2p::ClusterSource {
    reliaburger::pickle::p2p::ClusterSource {
        state: registry.state.clone(),
        members: None,
        registry_port: 0,
        peer_scheme: "http".to_string(),
        concurrency: 4,
        client: reqwest::Client::new(),
        upstream: None,
        pull_through: false,
        cache_recheck_secs: 3600,
        fill_lock: tokio::sync::Mutex::new(()),
    }
}

/// REG1 acceptance: push → GC past the grace window → the manifest GET
/// still answers 200 with the exact pushed bytes → a peer pull of the
/// full image (manifest blob included) succeeds.
#[tokio::test]
async fn gc_past_grace_keeps_manifest_fetchable_and_peer_pullable() {
    let node1 = Registry::start(1).await;
    let (manifest_bytes, manifest_digest) =
        push_test_image(&node1.base_url(), "web", "v1", "web").await;

    // GC past the grace window (orphan grace zero, no retention).
    run_gc(&node1).await;

    // The manifest is still fetchable, byte-identical.
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/v2/web/manifests/v1", node1.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        200,
        "the tagged manifest must survive GC"
    );
    assert_eq!(response.bytes().await.unwrap().to_vec(), manifest_bytes);

    // A peer that knows the catalogue pulls the full image.
    let node2 = Registry::start(2).await;
    *node2.state.catalog.write().await = node1.state.catalog.read().await.clone();
    let source = cluster_source_for(&node2);
    let paths = source
        .ensure_image_local_with_peers("web", "v1", &[node1.peer(1)])
        .await
        .unwrap()
        .expect("the catalogue knows the image");
    assert_eq!(paths.len(), 1, "one layer expected");
    assert!(
        node2.state.store.has_blob(&manifest_digest),
        "the peer pull must fetch the manifest blob too"
    );

    // The peer can now serve the manifest itself.
    let response = client
        .get(format!("{}/v2/web/manifests/v1", node2.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.bytes().await.unwrap().to_vec(), manifest_bytes);
}

/// REG1 upgrade path: a catalogue persisted *before* manifest blobs
/// joined holder tracking (no holder entry for the manifest's own
/// digest) must load without panicking, must not have its manifest
/// blob collected, and must get healed to redundancy rather than lost.
#[tokio::test]
async fn old_catalogue_without_manifest_holders_is_healed_not_collected() {
    use reliaburger::pickle::replication::heal_tick;

    // Node 1 pushed an image with the current code…
    let node1 = Registry::start(1).await;
    let (_, manifest_digest) = push_test_image(&node1.base_url(), "legacy", "v1", "legacy").await;

    // …but its persisted catalogue predates the fix: rewrite it on
    // disk without the manifest-blob holder entry (the exact shape the
    // old code persisted) and reload from that fixture.
    let old_format = {
        let catalog = node1.state.catalog.read().await;
        let mut value = serde_json::to_value(&*catalog).unwrap();
        let locations = value["layer_locations"].as_array_mut().unwrap();
        locations.retain(|entry| entry[0] != manifest_digest.as_str());
        value
    };
    let fixture_path = node1.dir.path().join("old-catalog.json");
    std::fs::write(
        &fixture_path,
        serde_json::to_vec_pretty(&old_format).unwrap(),
    )
    .unwrap();
    let reloaded = ManifestCatalog::load_from(&fixture_path).unwrap();
    assert!(
        reloaded.layer_holders(manifest_digest.as_str()).is_empty(),
        "fixture must have no manifest-blob holders"
    );
    *node1.state.catalog.write().await = reloaded;

    // GC on the old catalogue must not collect the manifest blob even
    // though it has no holder entry (it looks exactly like an orphan).
    run_gc(&node1).await;
    assert!(
        node1.state.store.has_blob(&manifest_digest),
        "the manifest blob must be healed, not collected"
    );

    // One heal tick replicates the whole image — manifest blob
    // included — to a new peer and records the holders.
    let node2 = Registry::start(2).await;
    let catalog_snapshot = node1.state.catalog.read().await.clone();
    let outcome = heal_tick(
        &catalog_snapshot,
        &node1.state.store,
        1,
        &[node1.peer(1), node2.peer(2)],
        2,
        10,
        &reqwest::Client::new(),
    )
    .await;
    assert!(
        outcome.errors.is_empty(),
        "heal errors: {:?}",
        outcome.errors
    );
    assert!(
        node2.state.store.has_blob(&manifest_digest),
        "heal must replicate the manifest blob"
    );
    let healed_manifest_holders = outcome
        .updates
        .iter()
        .flat_map(|u| u.updates.iter())
        .find(|(d, _)| d.as_str() == manifest_digest.as_str())
        .map(|(_, holders)| holders.clone());
    assert_eq!(
        healed_manifest_holders,
        Some([1u64, 2].into_iter().collect()),
        "the heal update must record manifest-blob holders"
    );
}

/// IMG1: verification resolves the tag to a digest and the pull uses
/// that digest — moving the tag between verify and pull cannot swap
/// the image underneath the policy check.
#[tokio::test]
async fn peer_pull_uses_the_verified_digest_when_the_tag_moves() {
    use reliaburger::config::node::TrustPolicySection;
    use reliaburger::meat::scheduler::{pin_image_reference, verify_image_signature};
    use reliaburger::pickle::types::AttachSignature;
    use reliaburger::sesame::ca;
    use reliaburger::sesame::identity::{CertUsage, create_workload_csr, validate_and_sign_csr};
    use reliaburger::sesame::types::{SerialNumber, SpiffeUri, WorkloadType};

    // Node 1 holds image A tagged web:v1, signed.
    let node1 = Registry::start(1).await;
    let (_, digest_a) = push_test_image(&node1.base_url(), "web", "v1", "image-a").await;

    let hierarchy = ca::generate_ca_hierarchy("test", b"test-ikm-32-bytes-padding-here!!").unwrap();
    // A real code-signing leaf whose SPIFFE identity matches the signature
    // (IMG3: code-signing EKU + identity binding).
    let uri = SpiffeUri {
        trust_domain: "test".to_string(),
        namespace: "ci".to_string(),
        workload_type: WorkloadType::Job,
        name: "build-signer".to_string(),
    };
    let (csr_der, key_der) = create_workload_csr(&uri).unwrap();
    let cert_der = validate_and_sign_csr(
        &csr_der,
        &uri,
        SerialNumber(100),
        CertUsage::CodeSigning,
        &hierarchy.workload.signing_keypair,
        &hierarchy.workload.certificate_params,
        std::time::SystemTime::now(),
    )
    .unwrap();
    let signature = reliaburger::pickle::signing::create_keyless_signature(
        &digest_a,
        &cert_der,
        &key_der,
        std::slice::from_ref(&hierarchy.workload.ca.certificate_der),
        "reliaburger-council",
        &uri.to_uri(),
    )
    .unwrap();
    node1
        .state
        .catalog
        .write()
        .await
        .apply_attach_signature(&AttachSignature {
            manifest_digest: digest_a.clone(),
            signature,
        });

    // Admission control verifies web:v1 and pins the digest.
    let policy = TrustPolicySection {
        require_signatures: true,
        keys: vec![],
    };
    let catalog_at_verify = node1.state.catalog.read().await.clone();
    let verified = verify_image_signature(
        Some("web:v1"),
        &catalog_at_verify,
        &policy,
        Some(&hierarchy.root.ca.certificate_der),
        None,
    )
    .unwrap()
    .expect("a signed pickle image must come back pinned");
    assert_eq!(verified, digest_a);
    let pinned = pin_image_reference("web:v1", &verified);

    // Between verify and pull, the tag moves to image B.
    let (_, digest_b) = push_test_image(&node1.base_url(), "web", "v1", "image-b").await;
    assert_ne!(digest_a, digest_b);

    // The deploying node pulls the pinned reference: the tag position
    // carries the digest after parsing, and the resolution is
    // content-addressed — image A arrives, not B.
    let image = reliaburger::grill::image::ImageReference::parse(&pinned).unwrap();
    assert_eq!(image.tag, digest_a.as_str());

    let node2 = Registry::start(2).await;
    *node2.state.catalog.write().await = node1.state.catalog.read().await.clone();
    let source = cluster_source_for(&node2);
    let paths = source
        .ensure_image_local_with_peers("web", &image.tag, &[node1.peer(1)])
        .await
        .unwrap()
        .expect("the catalogue knows the pinned digest");

    let layer_bytes = std::fs::read(&paths[0]).unwrap();
    assert_eq!(
        layer_bytes, b"layer-bytes-image-a",
        "the pulled layer must belong to the verified image"
    );
}

// ---------------------------------------------------------------------------
// REG4: registry auth
// ---------------------------------------------------------------------------

/// Build a registry whose writes require a `Deployer` principal, reusing
/// the same `sesame::auth` material as the agent API.
async fn start_authenticated_registry(deployer_plaintext: &mut String) -> Registry {
    use reliaburger::sesame::auth::{AuthState, new_token_store};
    use reliaburger::sesame::token::create_token;
    use reliaburger::sesame::types::{ApiRole, TokenScope};

    let dir = tempfile::tempdir().unwrap();
    let store = new_token_store();
    let created = create_token("ci", ApiRole::Deployer, TokenScope::default(), None).unwrap();
    *deployer_plaintext = created.plaintext.clone();
    store.write().await.push(created.token);
    let auth = AuthState::new(store, Some("rbrg_service".to_string()));

    let state = PickleState {
        store: Arc::new(BlobStore::new(dir.path().join("blobs"))),
        catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
        node_raft_id: 1,
        council: None,
        persist_path: None,
        auth: Some(auth),
        require_read_auth: false,
        allow_unauthenticated_bootstrap: false,
        quota: reliaburger::pickle::registry_auth::QuotaConfig::default(),
        sessions: reliaburger::pickle::registry_auth::UploadSessions::new(
            reliaburger::pickle::registry_auth::DEFAULT_UPLOAD_TTL,
        ),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let app = router(state.clone());
    let serve_shutdown = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
            .await
            .ok();
    });
    Registry {
        state,
        addr,
        shutdown,
        dir,
    }
}

/// REG4: an anonymous push is refused once auth is configured; the same
/// push with a `Deployer` bearer succeeds.
#[tokio::test]
async fn anonymous_push_is_refused_authenticated_push_succeeds() {
    let mut deployer = String::new();
    let registry = start_authenticated_registry(&mut deployer).await;
    let base = registry.base_url();
    let client = reqwest::Client::new();

    let blob = b"a config blob".to_vec();
    let digest = compute_sha256(&blob);

    // Anonymous monolithic upload: refused with 401.
    let resp = client
        .post(format!(
            "{base}/v2/web/blobs/uploads/?digest={}",
            digest.as_str()
        ))
        .body(blob.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        401,
        "anonymous push must be refused"
    );
    assert!(!registry.state.store.has_blob(&digest), "no blob stored");

    // The same push with a Deployer bearer succeeds.
    let resp = client
        .post(format!(
            "{base}/v2/web/blobs/uploads/?digest={}",
            digest.as_str()
        ))
        .bearer_auth(&deployer)
        .body(blob)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        201,
        "authenticated push must succeed"
    );
    assert!(registry.state.store.has_blob(&digest));
}

// ---------------------------------------------------------------------------
// REG2: one authoritative catalogue
// ---------------------------------------------------------------------------

/// REG2: a manifest committed to the council's Raft catalogue (as a peer
/// would) is visible through `manifest_get`/`tags_list` on a node that
/// never received the original PUT — because the read path now consults
/// the authoritative catalogue, not the node-local one.
#[tokio::test]
async fn a_peer_committed_manifest_is_visible_through_the_authoritative_catalogue() {
    use std::collections::{BTreeMap, BTreeSet};

    use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
    use reliaburger::council::node::CouncilNode;
    use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest};
    use reliaburger::pickle::types::{ImageManifest, LayerDescriptor, ManifestCommit};

    // A single-node council standing in for the cluster's Raft.
    let raft_router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(1, raft_router.clone());
    let raft_dir = tempfile::tempdir().unwrap();
    let (log_store, _fresh, state_machine) =
        reliaburger::cluster::runtime::open_raft_storage(raft_dir.path(), None)
            .await
            .unwrap();
    let council = CouncilNode::new(
        1,
        CouncilConfig::default(),
        network,
        log_store,
        state_machine,
        None,
    )
    .await
    .unwrap();
    raft_router.register(1, council.raft().clone()).await;
    council
        .initialize(BTreeMap::from([(
            1,
            CouncilNodeInfo {
                addr: "127.0.0.1:9555".parse().unwrap(),
                name: "n1".to_string(),
            },
        )]))
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !council.is_leader().await {
        assert!(tokio::time::Instant::now() < deadline, "no leader");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let council = Arc::new(council);

    // Commit a manifest via Raft directly — as a peer's push would.
    let manifest_bytes = b"a peer-committed manifest".to_vec();
    let manifest_digest = compute_sha256(&manifest_bytes);
    let config_digest = compute_sha256(b"peer config");
    let manifest = ImageManifest {
        digest: manifest_digest.clone(),
        config: LayerDescriptor {
            digest: config_digest.clone(),
            size: 11,
            media_type: "application/vnd.oci.image.config.v1+json".to_string(),
        },
        layers: vec![],
        repository: "team/service".to_string(),
        tags: BTreeSet::new(),
        total_size: 11,
        pushed_at: std::time::SystemTime::UNIX_EPOCH,
        pushed_by: 2, // a *different* node pushed it
        signature: None,
    };
    council
        .write(RaftRequest::ManifestCommit(ManifestCommit {
            manifest: manifest.clone(),
            tag: "v1".to_string(),
            holder_nodes: BTreeSet::from([2]),
        }))
        .await
        .unwrap();

    // This node's registry: an *empty* local catalogue, but the council
    // handle attached. It must still serve the peer's manifest bytes if
    // present locally, and always list the tag from the authoritative
    // catalogue.
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(BlobStore::new(dir.path().join("blobs")));
    // The manifest bytes happen to be local (a peer replicated them here),
    // so the GET returns them; the tag visibility is the REG2 point.
    store.write_blob(&manifest_bytes, &manifest_digest).unwrap();
    let state = PickleState {
        store,
        catalog: Arc::new(RwLock::new(ManifestCatalog::default())), // empty local
        node_raft_id: 1,
        council: Some(council.clone()),
        persist_path: None,
        auth: None,
        require_read_auth: false,
        allow_unauthenticated_bootstrap: true,
        quota: reliaburger::pickle::registry_auth::QuotaConfig::default(),
        sessions: reliaburger::pickle::registry_auth::UploadSessions::new(
            reliaburger::pickle::registry_auth::DEFAULT_UPLOAD_TTL,
        ),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let app = router(state);
    let serve_shutdown = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
            .await
            .ok();
    });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // tags_list must report v1 even though the local catalogue is empty.
    let resp = client
        .get(format!("{base}/v2/team/service/tags/list"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["tags"],
        serde_json::json!(["v1"]),
        "a peer-committed tag must be visible (REG2)"
    );

    // manifest_get by tag resolves through the authoritative catalogue.
    let resp = client
        .get(format!("{base}/v2/team/service/manifests/v1"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "a peer-committed manifest must be fetchable by tag (REG2)"
    );

    shutdown.cancel();
}

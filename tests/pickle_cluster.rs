//! Integration tests for the Pickle catalog, replication, and pull
//! paths (Stage 4 W5, L10/M2). Real axum registries on ephemeral
//! ports; real HTTP pushes and layer transfers between them.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::pickle::api::{PickleState, router};
use reliaburger::pickle::replication::{Peer, ReplicationConfig, replicate_manifest};
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
    /// Start a registry that counts every HTTP request it serves —
    /// how the pull-through tests prove "zero upstream hits".
    async fn start_counted(node_raft_id: u64) -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
        let registry = Self::start(node_raft_id, false).await;
        // Re-serve on a fresh port with a counting layer.
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting = counter.clone();
        let app = router(registry.state.clone()).layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let counting = counting.clone();
                async move {
                    counting.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    next.run(req).await
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serve_shutdown = registry.shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
                .await
                .ok();
        });
        let mut registry = registry;
        registry.addr = addr;
        (registry, counter)
    }

    async fn start(node_raft_id: u64, persist: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let persist_path = persist.then(|| dir.path().join("catalog.json"));
        let catalog = match &persist_path {
            Some(path) => ManifestCatalog::load_from(path).unwrap(),
            None => ManifestCatalog::default(),
        };
        let state = PickleState {
            store: Arc::new(BlobStore::new(dir.path().join("blobs"))),
            catalog: Arc::new(RwLock::new(catalog)),
            node_raft_id,
            council: None,
            persist_path,
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
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Push a one-layer image over the wire: monolithic blob uploads for
/// config + layer, then a manifest PUT — the same flow docker uses.
async fn push_test_image(base_url: &str, repo: &str, tag: &str) -> (Digest, Digest) {
    let client = reqwest::Client::new();

    let config_bytes = br#"{"architecture":"arm64"}"#.to_vec();
    // Layer content varies by repo so distinct images get distinct
    // digests (and therefore distinct manifest digests).
    let layer_bytes = format!("layer-bytes-for-test-{repo}").into_bytes();
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

    let manifest = serde_json::json!({
        "schemaVersion": 2,
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
    });
    let response = client
        .put(format!("{base_url}/v2/{repo}/manifests/{tag}"))
        .body(manifest.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201, "manifest put failed");

    (config_digest, layer_digest)
}

/// L10: pushes must record the pushing node's real raft id as the
/// holder — not the hardcoded `{0}` they used to.
#[tokio::test]
async fn push_records_real_holder_id() {
    let registry = Registry::start(42, false).await;
    let (config_digest, layer_digest) = push_test_image(&registry.base_url(), "web", "v1").await;

    let catalog = registry.state.catalog.read().await;
    assert_eq!(
        catalog.layer_holders(config_digest.as_str()),
        BTreeSet::from([42])
    );
    assert_eq!(
        catalog.layer_holders(layer_digest.as_str()),
        BTreeSet::from([42])
    );
}

/// L10: the catalog must survive a restart (it used to be `default()`
/// on every boot).
#[tokio::test]
async fn catalog_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let persist_path = dir.path().join("catalog.json");

    // First "boot": push an image, catalog persists on commit.
    {
        let state = PickleState {
            store: Arc::new(BlobStore::new(dir.path().join("blobs"))),
            catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
            node_raft_id: 7,
            council: None,
            persist_path: Some(persist_path.clone()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let app = router(state);
        let serve_shutdown = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
                .await
                .ok();
        });
        push_test_image(&format!("http://{addr}"), "keeper", "v1").await;
        shutdown.cancel();
        let _ = server.await;
    }

    // Second "boot": load from disk; the manifest is still there.
    let reloaded = ManifestCatalog::load_from(&persist_path).unwrap();
    assert!(
        reloaded.get_manifest_by_tag("keeper", "v1").is_some(),
        "manifest lost across restart"
    );
}

/// L10: the leader-driven replication path copies layers to a peer
/// registry over real HTTP.
#[tokio::test]
async fn replication_copies_layers_to_peer() {
    let source = Registry::start(1, false).await;
    let target = Registry::start(2, false).await;

    push_test_image(&source.base_url(), "app", "v2").await;
    let manifest = {
        let catalog = source.state.catalog.read().await;
        catalog.get_manifest_by_tag("app", "v2").unwrap().clone()
    };

    let peers = vec![Peer {
        node_id: 2,
        base_url: target.base_url(),
    }];
    let config = ReplicationConfig {
        redundancy: 2,
        peer_timeout: Duration::from_secs(5),
    };
    let client = reqwest::Client::new();
    let result = replicate_manifest(&manifest, &source.state.store, &peers, &config, &client)
        .await
        .unwrap();

    assert_eq!(result.successful_nodes, BTreeSet::from([2]));
    for digest in manifest.all_digests() {
        assert!(
            target.state.store.has_blob(digest),
            "peer missing replicated blob {digest}"
        );
    }
}

/// L10: a node that lacks an image's layers pulls them from a holder
/// peer before deploy.
#[tokio::test]
async fn pull_fetches_missing_layers_from_peer() {
    use reliaburger::pickle::pull::{image_available_locally, pull_manifest_layers};

    let holder = Registry::start(1, false).await;
    push_test_image(&holder.base_url(), "shared", "v3").await;
    let catalog = holder.state.catalog.read().await.clone();
    let manifest = catalog.get_manifest_by_tag("shared", "v3").unwrap().clone();

    // The "deploying node": empty local store.
    let local_dir = tempfile::tempdir().unwrap();
    let local_store = BlobStore::new(local_dir.path());
    assert!(!image_available_locally(
        "shared",
        "v3",
        &catalog,
        &local_store
    ));

    let peers = vec![Peer {
        node_id: 1,
        base_url: holder.base_url(),
    }];
    let digests: Vec<Digest> = manifest.all_digests().into_iter().cloned().collect();
    pull_manifest_layers(
        &digests,
        "shared",
        &catalog,
        &peers,
        &local_store,
        &reqwest::Client::new(),
        Duration::from_secs(5),
    )
    .await
    .unwrap();

    assert!(image_available_locally(
        "shared",
        "v3",
        &catalog,
        &local_store
    ));
}

/// Roadmap (Phase 12): an under-replicated image auto-heals when a new
/// node joins — one heal tick replicates it and records the holder.
#[tokio::test]
async fn heal_tick_replicates_to_new_peer() {
    use reliaburger::pickle::replication::heal_tick;

    // Node 1 pushed an image; it is the sole holder.
    let leader = Registry::start(1, false).await;
    push_test_image(&leader.base_url(), "app", "v1").await;
    let catalog = leader.state.catalog.read().await.clone();

    // Node 2 joins, empty.
    let joiner = Registry::start(2, false).await;
    let peers = vec![
        Peer {
            node_id: 1,
            base_url: leader.base_url(),
        },
        Peer {
            node_id: 2,
            base_url: joiner.base_url(),
        },
    ];

    let outcome = heal_tick(
        &catalog,
        &leader.state.store,
        1,
        &peers,
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
    assert_eq!(outcome.updates.len(), 1);
    for (_, holders) in &outcome.updates[0].updates {
        assert_eq!(holders, &BTreeSet::from([1, 2]));
    }

    let manifest = catalog.get_manifest_by_tag("app", "v1").unwrap().clone();
    for digest in manifest.all_digests() {
        assert!(
            joiner.state.store.has_blob(digest),
            "joiner missing healed blob {digest}"
        );
    }
}

/// Phase 12 B5: the leader pulls layers it lacks from a holder before
/// replicating onward, so images pushed to non-leader nodes still gain
/// redundancy — and the leader records itself as a new holder.
#[tokio::test]
async fn heal_tick_pulls_missing_layers_first() {
    use reliaburger::pickle::replication::heal_tick;

    // The image lives only on node 2; the leader (node 1) lacks it.
    let holder = Registry::start(2, false).await;
    push_test_image(&holder.base_url(), "remote", "v1").await;
    let catalog = holder.state.catalog.read().await.clone();

    let leader_dir = tempfile::tempdir().unwrap();
    let leader_store = BlobStore::new(leader_dir.path());
    let peers = vec![Peer {
        node_id: 2,
        base_url: holder.base_url(),
    }];

    let outcome = heal_tick(
        &catalog,
        &leader_store,
        1,
        &peers,
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

    // The leader pulled every layer locally…
    let manifest = catalog.get_manifest_by_tag("remote", "v1").unwrap().clone();
    for digest in manifest.all_digests() {
        assert!(
            leader_store.has_blob(digest),
            "leader missing pulled blob {digest}"
        );
    }

    // …and the proposed update records it as a holder alongside node 2.
    assert_eq!(outcome.updates.len(), 1);
    for (_, holders) in &outcome.updates[0].updates {
        assert_eq!(holders, &BTreeSet::from([1, 2]));
    }
}

// ---------------------------------------------------------------------------
// P2P parallel pulls (Phase 12 C2)
// ---------------------------------------------------------------------------

/// Build a manifest over the given blob digests and record it in a
/// catalog with the given holders. Returns (manifest, catalog).
fn manifest_with_holders(
    repo: &str,
    config_digest: &Digest,
    layer_digests: &[Digest],
    layer_sizes: &[u64],
    holders: &[u64],
) -> (reliaburger::pickle::types::ImageManifest, ManifestCatalog) {
    use reliaburger::pickle::types::{ImageManifest, LayerDescriptor, ManifestCommit};

    let manifest = ImageManifest {
        digest: compute_sha256(repo.as_bytes()),
        config: LayerDescriptor {
            digest: config_digest.clone(),
            size: 24,
            media_type: "application/vnd.oci.image.config.v1+json".to_string(),
        },
        layers: layer_digests
            .iter()
            .zip(layer_sizes)
            .map(|(d, size)| LayerDescriptor {
                digest: d.clone(),
                size: *size,
                media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            })
            .collect(),
        repository: repo.to_string(),
        tags: std::iter::once("v1".to_string()).collect(),
        total_size: layer_sizes.iter().sum(),
        pushed_at: std::time::SystemTime::UNIX_EPOCH,
        pushed_by: holders.first().copied().unwrap_or(1),
        signature: None,
    };
    let mut catalog = ManifestCatalog::default();
    catalog.apply_manifest_commit(&ManifestCommit {
        manifest: manifest.clone(),
        tag: "v1".to_string(),
        holder_nodes: holders.iter().copied().collect(),
    });
    (manifest, catalog)
}

fn cluster_source_for(registry: &Registry) -> reliaburger::pickle::p2p::ClusterSource {
    reliaburger::pickle::p2p::ClusterSource {
        state: registry.state.clone(),
        members: None,
        registry_port: 0,
        concurrency: 4,
        client: reqwest::Client::new(),
        upstream: None,
        pull_through: true,
        cache_recheck_secs: 3600,
        fill_lock: tokio::sync::Mutex::new(()),
    }
}

/// A cluster source whose pull-through cache points at `upstream_host`
/// (an in-process registry standing in for Docker Hub).
fn cluster_source_with_upstream(registry: &Registry) -> reliaburger::pickle::p2p::ClusterSource {
    let mut source = cluster_source_for(registry);
    source.upstream = Some(Arc::new(
        reliaburger::pickle::upstream::OciUpstream::insecure_http(Default::default()),
    ));
    source
}

/// Roadmap (Phase 12): a multi-layer 100 MB image pulls from a peer in
/// parallel in under five seconds, and the blobs land locally.
#[tokio::test]
async fn p2p_pull_100mb_image_under_5s() {
    use rand::RngCore;

    // Node 1 holds five 20 MB layers of incompressible bytes.
    let holder = Registry::start(1, false).await;
    let mut rng = rand::thread_rng();
    let mut layer_digests = Vec::new();
    let config_bytes = br#"{"architecture":"arm64"}"#.to_vec();
    let config_digest = compute_sha256(&config_bytes);
    holder
        .state
        .store
        .write_blob(&config_bytes, &config_digest)
        .unwrap();
    for _ in 0..5 {
        let mut bytes = vec![0u8; 20 * 1024 * 1024];
        rng.fill_bytes(&mut bytes);
        let digest = compute_sha256(&bytes);
        holder.state.store.write_blob(&bytes, &digest).unwrap();
        layer_digests.push(digest);
    }
    let (_, catalog) = manifest_with_holders(
        "big",
        &config_digest,
        &layer_digests,
        &[20 * 1024 * 1024; 5],
        &[1],
    );

    // Node 3 pulls it via the cluster source.
    let puller = Registry::start(3, false).await;
    *puller.state.catalog.write().await = catalog;
    let source = cluster_source_for(&puller);
    let peers = vec![Peer {
        node_id: 1,
        base_url: holder.base_url(),
    }];

    let started = std::time::Instant::now();
    let paths = source
        .ensure_image_local_with_peers("big", "v1", &peers)
        .await
        .unwrap()
        .expect("catalog should know the image");
    let elapsed = started.elapsed();

    assert_eq!(paths.len(), 5);
    for digest in &layer_digests {
        assert!(puller.state.store.has_blob(digest), "missing {digest}");
    }
    assert!(
        elapsed < Duration::from_secs(5),
        "100 MB P2P pull took {elapsed:?} (roadmap target: < 5s)"
    );
}

/// C2: with two holders, the plan spreads fetches across both, and the
/// pull completes.
#[tokio::test]
async fn p2p_pull_spreads_across_holders() {
    use reliaburger::pickle::p2p::plan_downloads;
    use std::collections::HashSet;

    let holder_a = Registry::start(1, false).await;
    let holder_b = Registry::start(2, false).await;

    let config_bytes = br#"{"architecture":"arm64"}"#.to_vec();
    let config_digest = compute_sha256(&config_bytes);
    let mut layer_digests = Vec::new();
    for i in 0..4u8 {
        let bytes = vec![i; 4096];
        let digest = compute_sha256(&bytes);
        for registry in [&holder_a, &holder_b] {
            registry.state.store.write_blob(&bytes, &digest).unwrap();
        }
        layer_digests.push(digest);
    }
    for registry in [&holder_a, &holder_b] {
        registry
            .state
            .store
            .write_blob(&config_bytes, &config_digest)
            .unwrap();
    }
    let (manifest, catalog) = manifest_with_holders(
        "spread",
        &config_digest,
        &layer_digests,
        &[4096; 4],
        &[1, 2],
    );

    let puller = Registry::start(3, false).await;
    *puller.state.catalog.write().await = catalog.clone();
    let peers = vec![
        Peer {
            node_id: 1,
            base_url: holder_a.base_url(),
        },
        Peer {
            node_id: 2,
            base_url: holder_b.base_url(),
        },
    ];

    // The plan uses both sources…
    let digests: Vec<Digest> = manifest.all_digests().into_iter().cloned().collect();
    let plan = plan_downloads(&digests, &HashSet::new(), &catalog, &peers, 3);
    let used: HashSet<u64> = plan.fetches.iter().map(|f| f.peer.node_id).collect();
    assert_eq!(used, [1u64, 2].into_iter().collect());

    // …and the pull completes.
    let source = cluster_source_for(&puller);
    source
        .ensure_image_local_with_peers("spread", "v1", &peers)
        .await
        .unwrap()
        .expect("catalog should know the image");
    for digest in &layer_digests {
        assert!(puller.state.store.has_blob(digest));
    }
}

/// C2: a fetch that fails against its planned peer retries against an
/// alternate holder and succeeds.
#[tokio::test]
async fn p2p_pull_retries_alternate_holder() {
    // A dead peer: bind, take the port, drop the listener.
    let dead_port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };

    let real = Registry::start(2, false).await;
    let config_bytes = br#"{"architecture":"arm64"}"#.to_vec();
    let config_digest = compute_sha256(&config_bytes);
    let layer_bytes = vec![7u8; 4096];
    let layer_digest = compute_sha256(&layer_bytes);
    real.state
        .store
        .write_blob(&config_bytes, &config_digest)
        .unwrap();
    real.state
        .store
        .write_blob(&layer_bytes, &layer_digest)
        .unwrap();

    // Catalog claims both node 1 (dead) and node 2 hold everything.
    let (_, catalog) = manifest_with_holders(
        "flaky",
        &config_digest,
        &[layer_digest.clone()],
        &[4096],
        &[1, 2],
    );

    let puller = Registry::start(3, false).await;
    *puller.state.catalog.write().await = catalog;
    let source = cluster_source_for(&puller);
    let peers = vec![
        Peer {
            node_id: 1,
            base_url: format!("http://127.0.0.1:{dead_port}"),
        },
        Peer {
            node_id: 2,
            base_url: real.base_url(),
        },
    ];

    source
        .ensure_image_local_with_peers("flaky", "v1", &peers)
        .await
        .unwrap()
        .expect("catalog should know the image");
    assert!(puller.state.store.has_blob(&layer_digest));
}

/// C2: when every holder is unreachable, the pull fails — it must NOT
/// fall back silently (the caller decides).
#[tokio::test]
async fn p2p_pull_fails_when_all_holders_unreachable() {
    let dead_port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };

    let config_bytes = br#"{"architecture":"arm64"}"#.to_vec();
    let config_digest = compute_sha256(&config_bytes);
    let layer_digest = compute_sha256(b"unreachable-layer");
    let (_, catalog) = manifest_with_holders("lost", &config_digest, &[layer_digest], &[17], &[1]);

    let puller = Registry::start(3, false).await;
    *puller.state.catalog.write().await = catalog;
    let source = cluster_source_for(&puller);
    let peers = vec![Peer {
        node_id: 1,
        base_url: format!("http://127.0.0.1:{dead_port}"),
    }];

    let result = source
        .ensure_image_local_with_peers("lost", "v1", &peers)
        .await;
    assert!(result.is_err(), "expected failure, got {result:?}");
}

/// C2: a catalog miss returns `None` — the caller falls through to the
/// external registry path.
#[tokio::test]
async fn p2p_catalog_miss_returns_none() {
    let puller = Registry::start(3, false).await;
    let source = cluster_source_for(&puller);

    let result = source
        .ensure_image_local_with_peers("unknown", "v1", &[])
        .await
        .unwrap();
    assert!(result.is_none());
}

// ---------------------------------------------------------------------------
// Pull-through cache (Phase 12 D2)
// ---------------------------------------------------------------------------

/// Roadmap (Phase 12): the first pull of an external image is fetched
/// from upstream and cached; a second node's pull is served entirely
/// by the cluster — zero upstream requests.
#[tokio::test]
async fn pull_through_caches_once_then_serves_peers() {
    use reliaburger::grill::image::ImageReference;
    use std::sync::atomic::Ordering;

    // The "upstream" (Docker Hub stand-in): an in-process registry
    // with a request counter. Single-segment repo name — our registry
    // routes one path segment; real upstreams with nested names work
    // through OciUpstream just the same.
    let (upstream, upstream_hits) = Registry::start_counted(9).await;
    push_test_image(&upstream.base_url(), "nginx", "v1").await;

    let image = ImageReference::parse(&format!("{}/nginx:v1", upstream.addr)).unwrap();
    let cached_repo = format!("cache/{}/nginx", upstream.addr);

    // Node A: first pull fills the cache from upstream.
    let node_a = Registry::start(1, false).await;
    let source_a = cluster_source_with_upstream(&node_a);
    let paths = source_a
        .ensure_external_image_with_peers(&image, &[])
        .await
        .unwrap()
        .expect("pull-through should serve the image");
    assert_eq!(paths.len(), 1);
    let first_pull_hits = upstream_hits.load(Ordering::SeqCst);
    assert!(first_pull_hits > 0, "first pull must reach upstream");
    {
        let catalog = node_a.state.catalog.read().await;
        assert!(
            catalog.get_manifest_by_tag(&cached_repo, "v1").is_some(),
            "cache fill must commit under {cached_repo}"
        );
    }

    // Node B: same image, catalog replicated (copied here), peers = A.
    // Zero further upstream requests.
    let node_b = Registry::start(2, false).await;
    *node_b.state.catalog.write().await = node_a.state.catalog.read().await.clone();
    let source_b = cluster_source_with_upstream(&node_b);
    let peers = vec![Peer {
        node_id: 1,
        base_url: node_a.base_url(),
    }];
    source_b
        .ensure_external_image_with_peers(&image, &peers)
        .await
        .unwrap()
        .expect("cached image should be served");

    assert_eq!(
        upstream_hits.load(Ordering::SeqCst),
        first_pull_hits,
        "second node's pull must not touch upstream"
    );
}

/// D2: `pull_through = false` is a clean fall-through, not an error.
#[tokio::test]
async fn pull_through_disabled_is_a_fall_through() {
    use reliaburger::grill::image::ImageReference;

    let node = Registry::start(1, false).await;
    let mut source = cluster_source_with_upstream(&node);
    source.pull_through = false;

    let image = ImageReference::parse("redis:7").unwrap();
    let result = source
        .ensure_external_image_with_peers(&image, &[])
        .await
        .unwrap();
    assert!(result.is_none());
}

/// D2: a stale cache whose upstream is unreachable serves the stale
/// copy — availability over freshness.
#[tokio::test]
async fn pull_through_serves_stale_when_upstream_down() {
    use reliaburger::grill::image::ImageReference;

    // The upstream host is a dead port.
    let dead_port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let image = ImageReference::parse(&format!("127.0.0.1:{dead_port}/library/redis:v1")).unwrap();
    let cached_repo = format!("cache/127.0.0.1:{dead_port}/library/redis");

    // Node A holds the (ancient — UNIX_EPOCH, i.e. stale) cached copy.
    let node_a = Registry::start(1, false).await;
    let config_bytes = br#"{"architecture":"arm64"}"#.to_vec();
    let config_digest = compute_sha256(&config_bytes);
    let layer_bytes = vec![9u8; 2048];
    let layer_digest = compute_sha256(&layer_bytes);
    node_a
        .state
        .store
        .write_blob(&config_bytes, &config_digest)
        .unwrap();
    node_a
        .state
        .store
        .write_blob(&layer_bytes, &layer_digest)
        .unwrap();
    let (_, catalog) = manifest_with_holders(
        &cached_repo,
        &config_digest,
        &[layer_digest.clone()],
        &[2048],
        &[1],
    );

    // Node B pulls it: stale → HEAD fails → serve stale via P2P.
    let node_b = Registry::start(2, false).await;
    *node_b.state.catalog.write().await = catalog;
    let source_b = cluster_source_with_upstream(&node_b);
    let peers = vec![Peer {
        node_id: 1,
        base_url: node_a.base_url(),
    }];

    source_b
        .ensure_external_image_with_peers(&image, &peers)
        .await
        .unwrap()
        .expect("stale cache should still serve when upstream is down");
    assert!(node_b.state.store.has_blob(&layer_digest));
}

/// The heal loop caps its work per tick: with three under-replicated
/// images and a cap of one, exactly one is healed per pass.
#[tokio::test]
async fn heal_tick_respects_per_tick_cap() {
    use reliaburger::pickle::replication::heal_tick;

    let leader = Registry::start(1, false).await;
    push_test_image(&leader.base_url(), "app-a", "v1").await;
    push_test_image(&leader.base_url(), "app-b", "v1").await;
    push_test_image(&leader.base_url(), "app-c", "v1").await;
    let catalog = leader.state.catalog.read().await.clone();

    let joiner = Registry::start(2, false).await;
    let peers = vec![
        Peer {
            node_id: 1,
            base_url: leader.base_url(),
        },
        Peer {
            node_id: 2,
            base_url: joiner.base_url(),
        },
    ];

    let outcome = heal_tick(
        &catalog,
        &leader.state.store,
        1,
        &peers,
        2,
        1,
        &reqwest::Client::new(),
    )
    .await;

    assert!(outcome.errors.is_empty());
    assert_eq!(
        outcome.updates.len(),
        1,
        "cap of 1 means one manifest per tick"
    );
}

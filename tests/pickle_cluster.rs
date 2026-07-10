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

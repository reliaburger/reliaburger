//! Peer pull and pull-through cache for Pickle.
//!
//! When a node needs an image it doesn't have locally, it can:
//! 1. Pull layers from peers that already hold them (recorded in Raft)
//! 2. Fall back to Docker Hub via `oci-distribution` and cache the
//!    result in Pickle for other nodes to pull from peers next time.

use std::collections::BTreeSet;
use std::time::Duration;

use futures_util::StreamExt as _;
use sha2::{Digest as Sha2Digest, Sha256};

use super::replication::Peer;
use super::store::BlobStore;
use super::types::{Digest, ManifestCatalog, PickleError};

/// Hard ceiling on a single blob pulled from a peer (REG6).
///
/// A compromised peer could otherwise stream an unbounded body and exhaust
/// this node's memory. Real image layers are far smaller; a layer bigger
/// than this is refused rather than buffered. The read is also wrapped in
/// the caller's timeout, so a slow trickle can't hold the connection open
/// forever either.
const MAX_PEER_BLOB_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Pull a layer from a peer node's OCI API.
///
/// Downloads the blob via `GET /v2/{repository}/blobs/{digest}` and
/// verifies the SHA-256 digest before storing locally.
pub async fn pull_layer_from_peer(
    peer: &Peer,
    repository: &str,
    digest: &Digest,
    store: &BlobStore,
    client: &reqwest::Client,
    timeout: Duration,
) -> Result<(), PickleError> {
    // Trust a cached blob only after re-verifying its digest (REG5). A
    // blob truncated by a crash mid-write, or corrupted on disk, must not
    // be served as valid — `revalidate_blob` removes it if it no longer
    // hashes to `digest`, so we refetch clean bytes below.
    if store.has_blob(digest) && store.revalidate_blob(digest) {
        return Ok(()); // Already cached locally and verified
    }

    // Peer blob URLs flatten multi-segment names (see peer_blob_repo):
    // the blob store is content-addressed and ignores the name anyway.
    let url = format!(
        "{}/v2/{}/blobs/{}",
        peer.base_url,
        super::replication::peer_blob_repo(repository),
        digest.as_str()
    );

    let resp = tokio::time::timeout(timeout, client.get(&url).send())
        .await
        .map_err(|_| {
            PickleError::ReplicationFailed(format!("timeout pulling from peer {}", peer.node_id))
        })?
        .map_err(|e| PickleError::ReplicationFailed(format!("peer {}: {e}", peer.node_id)))?;

    if !resp.status().is_success() {
        return Err(PickleError::BlobNotFound(digest.clone()));
    }

    // Bound the peer body (REG6): stream chunks under the timeout, refusing
    // anything past the hard cap so a hostile peer can't exhaust memory or
    // hold the connection open with a slow trickle. If the peer advertised
    // a Content-Length larger than the cap, reject before reading a byte.
    if let Some(len) = resp.content_length()
        && len > MAX_PEER_BLOB_BYTES as u64
    {
        return Err(PickleError::ReplicationFailed(format!(
            "peer {} blob is {len} bytes, over the {MAX_PEER_BLOB_BYTES} cap",
            peer.node_id
        )));
    }

    // Stream to a temp file, hashing incrementally, instead of buffering the
    // whole blob in RAM (M11). A 2 GiB layer used to sit fully in memory —
    // multiplied by every concurrent pull — before it was written. Now peak
    // memory is one chunk: chunks stream straight to the upload temp, the hash
    // is computed as they pass, and the verified temp is committed by rename
    // (no re-read). On any error the temp is dropped.
    let upload_id = store.initiate_upload().await.map_err(|e| {
        PickleError::ReplicationFailed(format!("peer {}: staging upload: {e}", peer.node_id))
    })?;

    let streamed = tokio::time::timeout(timeout, async {
        let mut stream = resp.bytes_stream();
        let mut hasher = Sha256::new();
        let mut total: usize = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                PickleError::ReplicationFailed(format!(
                    "reading blob from peer {}: {e}",
                    peer.node_id
                ))
            })?;
            total = total.saturating_add(chunk.len());
            if total > MAX_PEER_BLOB_BYTES {
                return Err(PickleError::ReplicationFailed(format!(
                    "peer {} blob exceeded the {MAX_PEER_BLOB_BYTES} byte cap",
                    peer.node_id
                )));
            }
            hasher.update(&chunk);
            store
                .write_upload_chunk(&upload_id, &chunk)
                .await
                .map_err(|e| {
                    PickleError::ReplicationFailed(format!(
                        "peer {}: writing blob to disk: {e}",
                        peer.node_id
                    ))
                })?;
        }
        let actual = Digest::from_sha256_hex(&hex::encode(hasher.finalize()));
        if actual.as_str() != digest.as_str() {
            return Err(PickleError::DigestMismatch {
                expected: digest.clone(),
                actual,
            });
        }
        Ok(())
    })
    .await
    .map_err(|_| {
        PickleError::ReplicationFailed(format!("timeout reading blob from peer {}", peer.node_id))
    });

    // Flatten the timeout and inner results; on any failure drop the temp.
    match streamed.and_then(|inner| inner) {
        Ok(()) => {}
        Err(e) => {
            store.cancel_upload(&upload_id).await;
            return Err(e);
        }
    }

    // Digest verified above; commit the temp into the blob store by rename.
    store.commit_upload_as_blob(&upload_id, digest).await?;
    Ok(())
}

/// Pull all layers of a manifest from peers.
///
/// For each layer, finds a peer that holds it (from Raft layer_locations)
/// and downloads it. Skips layers already cached locally.
pub async fn pull_manifest_layers(
    digests: &[Digest],
    repository: &str,
    catalog: &ManifestCatalog,
    available_peers: &[Peer],
    store: &BlobStore,
    client: &reqwest::Client,
    timeout: Duration,
) -> Result<(), PickleError> {
    for digest in digests {
        if store.has_blob(digest) {
            continue;
        }

        let holders = catalog.layer_holders(digest.as_str());
        let peer = find_peer_for_layer(&holders, available_peers);

        match peer {
            Some(p) => {
                pull_layer_from_peer(p, repository, digest, store, client, timeout).await?;
            }
            None => {
                return Err(PickleError::ReplicationFailed(format!(
                    "no peer holds layer {}",
                    digest
                )));
            }
        }
    }
    Ok(())
}

/// Find a peer from the available list that holds a specific layer.
fn find_peer_for_layer<'a>(holders: &BTreeSet<u64>, peers: &'a [Peer]) -> Option<&'a Peer> {
    peers.iter().find(|p| holders.contains(&p.node_id))
}

/// Resolve an image: check the local Pickle store, then fall back
/// to external pull.
///
/// Returns `true` if the image was found locally (no external fetch needed).
/// Returns `false` if the caller should fall back to the original pull path.
pub fn image_available_locally(
    repository: &str,
    tag: &str,
    catalog: &ManifestCatalog,
    store: &BlobStore,
) -> bool {
    let Some(manifest) = catalog.get_manifest_by_tag(repository, tag) else {
        return false;
    };

    // Everything the tag pins must be local *and valid* — the manifest
    // blob too (REG1), or this node cannot serve the manifest GET. On the
    // deploy/verify path we revalidate rather than trust existence (REG5):
    // a truncated or corrupt cached blob is not "available", and
    // `revalidate_blob` removes it so the next pull refetches clean bytes.
    manifest
        .referenced_digests()
        .iter()
        .all(|d| store.revalidate_blob(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pickle::replication::Peer;
    use crate::pickle::store::compute_sha256;
    use crate::pickle::types::{ImageManifest, LayerDescriptor, ManifestCatalog, ManifestCommit};
    use std::collections::BTreeSet;
    use std::time::SystemTime;

    fn test_digest(suffix: &str) -> Digest {
        Digest(format!("sha256:{suffix:0>64}"))
    }

    /// A digest that actually matches `content`, so `revalidate_blob`
    /// accepts a blob written with these bytes (REG5).
    fn content_digest(content: &[u8]) -> Digest {
        compute_sha256(content)
    }

    /// Write a blob whose stored bytes hash to its own digest, so the
    /// REG5 revalidation on the deploy path accepts it.
    fn write_valid_blob(store: &BlobStore, digest: &Digest, content: &[u8]) {
        store.write_blob(content, digest).unwrap();
    }

    fn test_layer(suffix: &str) -> LayerDescriptor {
        LayerDescriptor {
            digest: test_digest(suffix),
            size: 1024,
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
        }
    }

    fn test_manifest(repo: &str) -> ImageManifest {
        ImageManifest {
            digest: test_digest("manifest1"),
            config: test_layer("config1"),
            layers: vec![test_layer("layer1"), test_layer("layer2")],
            repository: repo.to_string(),
            tags: BTreeSet::new(),
            total_size: 3072,
            pushed_at: SystemTime::UNIX_EPOCH,
            pushed_by: 1,
            signature: None,
        }
    }

    #[test]
    fn find_peer_for_layer_returns_matching_peer() {
        let peers = vec![
            Peer {
                node_id: 1,
                base_url: "http://1".to_string(),
            },
            Peer {
                node_id: 2,
                base_url: "http://2".to_string(),
            },
        ];
        let holders = BTreeSet::from([2]);
        let peer = find_peer_for_layer(&holders, &peers).unwrap();
        assert_eq!(peer.node_id, 2);
    }

    #[test]
    fn find_peer_for_layer_returns_none_when_no_match() {
        let peers = vec![Peer {
            node_id: 1,
            base_url: "http://1".to_string(),
        }];
        let holders = BTreeSet::from([99]);
        assert!(find_peer_for_layer(&holders, &peers).is_none());
    }

    /// A manifest whose every referenced digest matches real content, so
    /// the REG5 revalidation on the deploy path can accept the blobs.
    fn valid_manifest(repo: &str) -> (ImageManifest, Vec<(Digest, Vec<u8>)>) {
        let config = content_digest(b"config-content");
        let layer1 = content_digest(b"layer1-content");
        let layer2 = content_digest(b"layer2-content");
        let manifest_bytes = b"manifest-content".to_vec();
        let manifest_digest = content_digest(&manifest_bytes);
        let mk = |digest: Digest| LayerDescriptor {
            digest,
            size: 1024,
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
        };
        let manifest = ImageManifest {
            digest: manifest_digest.clone(),
            config: mk(config.clone()),
            layers: vec![mk(layer1.clone()), mk(layer2.clone())],
            repository: repo.to_string(),
            tags: BTreeSet::new(),
            total_size: 3072,
            pushed_at: SystemTime::UNIX_EPOCH,
            pushed_by: 1,
            signature: None,
        };
        let blobs = vec![
            (manifest_digest, manifest_bytes),
            (config, b"config-content".to_vec()),
            (layer1, b"layer1-content".to_vec()),
            (layer2, b"layer2-content".to_vec()),
        ];
        (manifest, blobs)
    }

    #[test]
    fn image_available_locally_true_when_all_blobs_present() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let (manifest, blobs) = valid_manifest("myapp");
        // Write all blobs with content matching their digest (REG5).
        for (digest, content) in &blobs {
            write_valid_blob(&store, digest, content);
        }

        let mut catalog = ManifestCatalog::default();
        catalog.apply_manifest_commit(&ManifestCommit {
            manifest,
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        assert!(image_available_locally("myapp", "latest", &catalog, &store));
    }

    /// REG5: a locally-cached blob that got corrupted on disk means the
    /// image is not "available" — the deploy path must refetch, not serve
    /// truncated bytes.
    #[test]
    fn image_available_locally_false_when_a_cached_blob_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let (manifest, blobs) = valid_manifest("myapp");
        for (digest, content) in &blobs {
            write_valid_blob(&store, digest, content);
        }
        // Corrupt one layer's on-disk bytes.
        let victim = &manifest.layers[0].digest;
        std::fs::write(store.blob_path(victim), b"tampered").unwrap();

        let mut catalog = ManifestCatalog::default();
        catalog.apply_manifest_commit(&ManifestCommit {
            manifest,
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        assert!(!image_available_locally(
            "myapp", "latest", &catalog, &store
        ));
    }

    #[test]
    fn image_available_locally_false_when_not_in_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let catalog = ManifestCatalog::default();

        assert!(!image_available_locally(
            "myapp", "latest", &catalog, &store
        ));
    }

    /// REG1: without its own manifest blob a node cannot serve the
    /// manifest GET, so the image is not fully available.
    #[test]
    fn image_available_locally_false_when_manifest_blob_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let (manifest, blobs) = valid_manifest("myapp");
        // Config and layers present (valid), the manifest blob absent.
        let manifest_digest = manifest.digest.clone();
        for (digest, content) in &blobs {
            if digest != &manifest_digest {
                write_valid_blob(&store, digest, content);
            }
        }

        let mut catalog = ManifestCatalog::default();
        catalog.apply_manifest_commit(&ManifestCommit {
            manifest,
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        assert!(!image_available_locally(
            "myapp", "latest", &catalog, &store
        ));
    }

    #[test]
    fn image_available_locally_false_when_blobs_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let manifest = test_manifest("myapp");
        let mut catalog = ManifestCatalog::default();
        catalog.apply_manifest_commit(&ManifestCommit {
            manifest,
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        // Don't write any blobs — they're missing
        assert!(!image_available_locally(
            "myapp", "latest", &catalog, &store
        ));
    }

    /// Serve `body` at `GET /v2/{repo}/blobs/{digest}` on an ephemeral port,
    /// returning the base URL. Used to drive the real streaming pull path.
    async fn serve_blob(body: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/v2/{*rest}",
            get(move || {
                let body = body.clone();
                async move { body }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    /// M11: a peer pull streams the blob to disk and commits it. The stored
    /// bytes are byte-identical and no upload temp is left behind.
    #[tokio::test]
    async fn pull_layer_streams_and_commits_a_valid_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let content = b"a realistic layer body streamed in".to_vec();
        let digest = compute_sha256(&content);

        let (base_url, server) = serve_blob(content.clone()).await;
        let peer = Peer {
            node_id: 2,
            base_url,
        };
        let client = reqwest::Client::new();

        pull_layer_from_peer(
            &peer,
            "myapp",
            &digest,
            &store,
            &client,
            Duration::from_secs(10),
        )
        .await
        .expect("valid blob should pull and commit");

        assert!(store.has_blob(&digest));
        assert_eq!(store.read_blob(&digest).unwrap(), content);
        // The uploads dir holds no leftover temps.
        let uploads = dir.path().join("uploads");
        let leftover = std::fs::read_dir(&uploads)
            .map(|rd| rd.count())
            .unwrap_or(0);
        assert_eq!(leftover, 0, "a committed pull must leave no upload temp");
        server.abort();
    }

    /// M11: a peer that serves bytes not matching the requested digest is
    /// rejected, and the streamed temp is dropped rather than committed.
    #[tokio::test]
    async fn pull_layer_rejects_a_digest_mismatch_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        // Ask for one digest, serve different bytes.
        let requested = compute_sha256(b"what we asked for");
        let (base_url, server) = serve_blob(b"something else entirely".to_vec()).await;
        let peer = Peer {
            node_id: 2,
            base_url,
        };
        let client = reqwest::Client::new();

        let err = pull_layer_from_peer(
            &peer,
            "myapp",
            &requested,
            &store,
            &client,
            Duration::from_secs(10),
        )
        .await
        .expect_err("a digest mismatch must be refused");
        assert!(matches!(err, PickleError::DigestMismatch { .. }));
        assert!(!store.has_blob(&requested));
        let uploads = dir.path().join("uploads");
        let leftover = std::fs::read_dir(&uploads)
            .map(|rd| rd.count())
            .unwrap_or(0);
        assert_eq!(leftover, 0, "a rejected pull must leave no upload temp");
        server.abort();
    }
}

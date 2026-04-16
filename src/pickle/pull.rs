//! Peer pull and pull-through cache for Pickle.
//!
//! When a node needs an image it doesn't have locally, it can:
//! 1. Pull layers from peers that already hold them (recorded in Raft)
//! 2. Fall back to Docker Hub via `oci-distribution` and cache the
//!    result in Pickle for other nodes to pull from peers next time.

use std::collections::BTreeSet;
use std::time::Duration;

use super::replication::Peer;
use super::store::{BlobStore, compute_sha256};
use super::types::{Digest, ManifestCatalog, PickleError};

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
    if store.has_blob(digest) {
        return Ok(()); // Already cached locally
    }

    let url = format!(
        "{}/v2/{}/blobs/{}",
        peer.base_url,
        repository,
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

    let data = resp.bytes().await.map_err(|e| {
        PickleError::ReplicationFailed(format!("reading blob from peer {}: {e}", peer.node_id))
    })?;

    // Verify digest
    let actual = compute_sha256(&data);
    if actual.as_str() != digest.as_str() {
        return Err(PickleError::DigestMismatch {
            expected: digest.clone(),
            actual,
        });
    }

    store.write_blob(&data, digest)?;
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

    // Check that all layers are available locally
    manifest.all_digests().iter().all(|d| store.has_blob(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pickle::replication::Peer;
    use crate::pickle::types::{ImageManifest, LayerDescriptor, ManifestCatalog, ManifestCommit};
    use std::collections::BTreeSet;
    use std::time::SystemTime;

    fn test_digest(suffix: &str) -> Digest {
        Digest(format!("sha256:{suffix:0>64}"))
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

    #[test]
    fn image_available_locally_true_when_all_blobs_present() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let manifest = test_manifest("myapp");

        // Write all blobs
        for digest in manifest.all_digests() {
            // Write dummy data with matching digest structure
            let path = store.blob_path(digest);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"data").unwrap();
        }

        let mut catalog = ManifestCatalog::default();
        catalog.apply_manifest_commit(&ManifestCommit {
            manifest,
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        assert!(image_available_locally("myapp", "latest", &catalog, &store));
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
}

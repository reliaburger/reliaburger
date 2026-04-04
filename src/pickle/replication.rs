//! Synchronous replication for Pickle.
//!
//! After a manifest is pushed locally, layers are replicated to N peer
//! nodes before the Raft commit. Replication uses the same OCI Distribution
//! API endpoints that clients use, so each peer validates digests and
//! stores blobs identically.

use std::collections::BTreeSet;
use std::time::Duration;

use super::store::BlobStore;
use super::types::{Digest, ImageManifest, PickleError};

/// Configuration for replication.
#[derive(Debug, Clone)]
pub struct ReplicationConfig {
    /// Number of total copies (including the pushing node).
    pub redundancy: u32,
    /// Timeout for replicating to a single peer.
    pub peer_timeout: Duration,
}

impl Default for ReplicationConfig {
    fn default() -> Self {
        Self {
            redundancy: 2,
            peer_timeout: Duration::from_secs(30),
        }
    }
}

/// A cluster peer that can receive replicated layers.
#[derive(Debug, Clone)]
pub struct Peer {
    /// Raft node ID.
    pub node_id: u64,
    /// Base URL for the peer's OCI API (e.g. `https://10.0.1.5:5000`).
    pub base_url: String,
}

/// Select peers for replication.
///
/// Picks up to `count` peers from the available set, excluding the
/// local node and any nodes that already hold all layers. Prefers
/// nodes not already in the holder set.
pub fn select_peers(
    available: &[Peer],
    local_node_id: u64,
    existing_holders: &BTreeSet<u64>,
    count: usize,
) -> Vec<Peer> {
    let mut candidates: Vec<&Peer> = available
        .iter()
        .filter(|p| p.node_id != local_node_id)
        .collect();

    // Sort: prefer peers not already holding the layers
    candidates.sort_by_key(|p| existing_holders.contains(&p.node_id));

    candidates.into_iter().take(count).cloned().collect()
}

/// The result of a replication attempt.
#[derive(Debug)]
pub struct ReplicationResult {
    /// Nodes that successfully received all layers.
    pub successful_nodes: BTreeSet<u64>,
    /// Nodes that failed (with reason).
    pub failed_nodes: Vec<(u64, String)>,
}

/// Check which layers a peer already has.
///
/// Returns the set of digests the peer already holds (via HEAD requests).
/// This avoids re-uploading layers that the peer already cached from
/// a previous push or pull-through.
pub async fn check_peer_has_layers(
    peer: &Peer,
    repository: &str,
    digests: &[&Digest],
    client: &reqwest::Client,
    timeout: Duration,
) -> BTreeSet<String> {
    let mut has = BTreeSet::new();
    for digest in digests {
        let url = format!(
            "{}/v2/{}/blobs/{}",
            peer.base_url,
            repository,
            digest.as_str()
        );
        let result = tokio::time::timeout(timeout, client.head(&url).send()).await;
        if let Ok(Ok(resp)) = result
            && resp.status().is_success()
        {
            has.insert(digest.as_str().to_string());
        }
    }
    has
}

/// Replicate a single layer to a peer via its OCI upload API.
pub async fn replicate_layer_to_peer(
    peer: &Peer,
    repository: &str,
    digest: &Digest,
    data: &[u8],
    client: &reqwest::Client,
    timeout: Duration,
) -> Result<(), PickleError> {
    // Initiate upload
    let initiate_url = format!("{}/v2/{}/blobs/uploads/", peer.base_url, repository);
    let resp = tokio::time::timeout(timeout, client.post(&initiate_url).send())
        .await
        .map_err(|_| {
            PickleError::ReplicationFailed(format!(
                "timeout initiating upload to peer {}",
                peer.node_id
            ))
        })?
        .map_err(|e| PickleError::ReplicationFailed(format!("peer {}: {e}", peer.node_id)))?;

    if !resp.status().is_success() {
        return Err(PickleError::ReplicationFailed(format!(
            "peer {} rejected upload initiation: {}",
            peer.node_id,
            resp.status()
        )));
    }

    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            PickleError::ReplicationFailed(format!(
                "peer {} did not return upload location",
                peer.node_id
            ))
        })?
        .to_string();

    // Upload data + complete in one PUT
    let complete_url = if location.starts_with("http") {
        format!("{location}?digest={}", digest.as_str())
    } else {
        format!("{}{}?digest={}", peer.base_url, location, digest.as_str())
    };

    let resp = tokio::time::timeout(
        timeout,
        client.put(&complete_url).body(data.to_vec()).send(),
    )
    .await
    .map_err(|_| {
        PickleError::ReplicationFailed(format!("timeout uploading to peer {}", peer.node_id))
    })?
    .map_err(|e| PickleError::ReplicationFailed(format!("peer {}: {e}", peer.node_id)))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        Err(PickleError::ReplicationFailed(format!(
            "peer {} rejected blob upload: {}",
            peer.node_id,
            resp.status()
        )))
    }
}

/// Replicate all layers of a manifest to selected peers.
///
/// Returns the set of node IDs that successfully received all layers.
/// If fewer than `config.redundancy - 1` peers succeed, returns an error.
pub async fn replicate_manifest(
    manifest: &ImageManifest,
    store: &BlobStore,
    peers: &[Peer],
    config: &ReplicationConfig,
    client: &reqwest::Client,
) -> Result<ReplicationResult, PickleError> {
    let all_digests: Vec<&Digest> = manifest.all_digests();
    let mut successful = BTreeSet::new();
    let mut failed = Vec::new();

    for peer in peers {
        // Check which layers the peer already has
        let already_has = check_peer_has_layers(
            peer,
            &manifest.repository,
            &all_digests,
            client,
            config.peer_timeout,
        )
        .await;

        let mut peer_ok = true;
        for digest in &all_digests {
            if already_has.contains(digest.as_str()) {
                continue; // Skip layers the peer already has
            }

            let data = match store.read_blob(digest) {
                Ok(d) => d,
                Err(e) => {
                    failed.push((peer.node_id, format!("missing local blob: {e}")));
                    peer_ok = false;
                    break;
                }
            };

            if let Err(e) = replicate_layer_to_peer(
                peer,
                &manifest.repository,
                digest,
                &data,
                client,
                config.peer_timeout,
            )
            .await
            {
                failed.push((peer.node_id, e.to_string()));
                peer_ok = false;
                break;
            }
        }

        if peer_ok {
            successful.insert(peer.node_id);
        }
    }

    // Check if we have enough replicas
    let required = (config.redundancy as usize).saturating_sub(1); // minus the local copy
    if successful.len() < required && !peers.is_empty() {
        return Err(PickleError::ReplicationFailed(format!(
            "only {}/{} peers succeeded",
            successful.len(),
            required
        )));
    }

    Ok(ReplicationResult {
        successful_nodes: successful,
        failed_nodes: failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_peers() -> Vec<Peer> {
        vec![
            Peer {
                node_id: 1,
                base_url: "http://10.0.1.1:5000".to_string(),
            },
            Peer {
                node_id: 2,
                base_url: "http://10.0.1.2:5000".to_string(),
            },
            Peer {
                node_id: 3,
                base_url: "http://10.0.1.3:5000".to_string(),
            },
            Peer {
                node_id: 4,
                base_url: "http://10.0.1.4:5000".to_string(),
            },
        ]
    }

    #[test]
    fn select_peers_excludes_self() {
        let peers = test_peers();
        let selected = select_peers(&peers, 1, &BTreeSet::new(), 2);
        assert_eq!(selected.len(), 2);
        assert!(selected.iter().all(|p| p.node_id != 1));
    }

    #[test]
    fn select_peers_respects_count() {
        let peers = test_peers();
        let selected = select_peers(&peers, 1, &BTreeSet::new(), 1);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn select_peers_fewer_available_than_requested() {
        let peers = vec![Peer {
            node_id: 2,
            base_url: "http://10.0.1.2:5000".to_string(),
        }];
        let selected = select_peers(&peers, 1, &BTreeSet::new(), 5);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn select_peers_prefers_non_holders() {
        let peers = test_peers();
        let holders = BTreeSet::from([2, 3]);
        let selected = select_peers(&peers, 1, &holders, 2);

        // Should prefer node 4 (non-holder) over 2, 3
        assert!(selected.iter().any(|p| p.node_id == 4));
    }

    #[test]
    fn select_peers_empty_available() {
        let selected = select_peers(&[], 1, &BTreeSet::new(), 2);
        assert!(selected.is_empty());
    }

    #[test]
    fn select_peers_all_are_self() {
        let peers = vec![Peer {
            node_id: 1,
            base_url: "http://10.0.1.1:5000".to_string(),
        }];
        let selected = select_peers(&peers, 1, &BTreeSet::new(), 2);
        assert!(selected.is_empty());
    }

    #[test]
    fn replication_config_defaults() {
        let config = ReplicationConfig::default();
        assert_eq!(config.redundancy, 2);
        assert_eq!(config.peer_timeout, Duration::from_secs(30));
    }
}

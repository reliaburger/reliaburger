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

// ---------------------------------------------------------------------------
// The heal loop (Phase 12 B5)
// ---------------------------------------------------------------------------

/// A manifest that needs more copies, and the nodes currently holding
/// *all* of its layers. Produced by [`plan_heal`], consumed by
/// [`heal_tick`].
#[derive(Debug)]
pub struct HealCandidate<'a> {
    pub manifest: &'a super::types::ImageManifest,
    pub full_holders: BTreeSet<u64>,
}

/// Find under-replicated manifests, rarest first.
///
/// "Rarest first" means ascending holder count: the manifests closest
/// to loss are healed before merely under-replicated ones. The result
/// is capped at `max_per_tick`, so a freshly joined empty node cannot
/// trigger a cluster-wide replication storm — the loop catches up a
/// slice at a time.
pub fn plan_heal<'a>(
    catalog: &'a super::types::ManifestCatalog,
    redundancy: u32,
    max_per_tick: usize,
) -> Vec<HealCandidate<'a>> {
    let mut candidates: Vec<HealCandidate<'a>> = catalog
        .manifests
        .iter()
        .filter_map(|(_, manifest)| {
            let mut full_holders: Option<BTreeSet<u64>> = None;
            for digest in manifest.all_digests() {
                let holders = catalog.layer_holders(digest.as_str());
                full_holders = Some(match full_holders {
                    Some(acc) => acc.intersection(&holders).copied().collect(),
                    None => holders,
                });
            }
            let full_holders = full_holders.unwrap_or_default();
            (full_holders.len() < redundancy as usize).then_some(HealCandidate {
                manifest,
                full_holders,
            })
        })
        .collect();

    candidates.sort_by_key(|c| c.full_holders.len());
    candidates.truncate(max_per_tick);
    candidates
}

/// What one heal pass did: holder updates to propose to Raft, and the
/// per-manifest failures (the caller logs them; a failure on one
/// manifest never aborts the pass).
#[derive(Debug, Default)]
pub struct HealOutcome {
    pub updates: Vec<super::types::UpdateLayerLocations>,
    pub errors: Vec<String>,
}

/// One heal pass over the catalog.
///
/// For each under-replicated manifest (rarest first, capped at
/// `max_per_tick`): first pull any layers this node lacks from a
/// holder — so manifests pushed to, or cache-filled on, *other* nodes
/// still gain redundancy — then replicate to peers that lack them.
/// Returns the holder updates for the caller to propose to Raft.
#[allow(clippy::too_many_arguments)]
pub async fn heal_tick(
    catalog: &super::types::ManifestCatalog,
    store: &BlobStore,
    self_node: u64,
    peers: &[Peer],
    redundancy: u32,
    max_per_tick: usize,
    client: &reqwest::Client,
) -> HealOutcome {
    let config = ReplicationConfig {
        redundancy,
        peer_timeout: Duration::from_secs(30),
    };
    let mut outcome = HealOutcome::default();

    for candidate in plan_heal(catalog, redundancy, max_per_tick) {
        let manifest = candidate.manifest;
        let mut full_holders = candidate.full_holders;
        let digests: Vec<Digest> = manifest.all_digests().into_iter().cloned().collect();

        // Pull-first: become a holder before replicating onward.
        if !digests.iter().all(|d| store.has_blob(d))
            && let Err(e) = super::pull::pull_manifest_layers(
                &digests,
                &manifest.repository,
                catalog,
                peers,
                store,
                client,
                config.peer_timeout,
            )
            .await
        {
            outcome
                .errors
                .push(format!("cannot pull {}: {e}", manifest.repository));
            continue;
        }
        let self_is_new_holder = full_holders.insert(self_node);

        let mut new_holders: BTreeSet<u64> = BTreeSet::new();
        if self_is_new_holder {
            new_holders.insert(self_node);
        }

        let needed = (redundancy as usize).saturating_sub(full_holders.len());
        let targets = select_peers(peers, self_node, &full_holders, needed);
        if !targets.is_empty() {
            match replicate_manifest(manifest, store, &targets, &config, client).await {
                Ok(result) => new_holders.extend(result.successful_nodes),
                Err(e) => outcome.errors.push(format!(
                    "replication of {} failed: {e}",
                    manifest.repository
                )),
            }
        }

        if new_holders.is_empty() {
            continue;
        }
        let updates = digests
            .iter()
            .map(|d| {
                let mut holders = catalog.layer_holders(d.as_str());
                holders.extend(new_holders.iter().copied());
                (d.clone(), holders)
            })
            .collect();
        outcome
            .updates
            .push(super::types::UpdateLayerLocations { updates });
    }

    outcome
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

    // -- plan_heal ----------------------------------------------------------

    use super::super::types::{ImageManifest, LayerDescriptor, ManifestCatalog, ManifestCommit};

    /// A digest whose hex encodes the suffix, so each manifest gets
    /// distinct layers.
    fn heal_digest(suffix: u8) -> Digest {
        Digest::new(&format!("sha256:{:064x}", suffix as u128)).unwrap()
    }

    fn heal_manifest(repo: &str, suffix: u8) -> ImageManifest {
        let layer = |s: u8| LayerDescriptor {
            digest: heal_digest(s),
            size: 100,
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
        };
        ImageManifest {
            digest: heal_digest(suffix),
            config: layer(suffix + 1),
            layers: vec![layer(suffix + 2)],
            repository: repo.to_string(),
            tags: BTreeSet::new(),
            total_size: 200,
            pushed_at: std::time::SystemTime::UNIX_EPOCH,
            pushed_by: 1,
            signature: None,
        }
    }

    /// Catalog with three manifests holding 2, 0-ish (1), and 3 full
    /// copies respectively.
    fn heal_catalog() -> ManifestCatalog {
        let mut catalog = ManifestCatalog::default();
        for (repo, suffix, holders) in [
            ("two-copies", 10u8, vec![1u64, 2]),
            ("one-copy", 20, vec![1]),
            ("three-copies", 30, vec![1, 2, 3]),
        ] {
            catalog.apply_manifest_commit(&ManifestCommit {
                manifest: heal_manifest(repo, suffix),
                tag: "v1".to_string(),
                holder_nodes: holders.into_iter().collect(),
            });
        }
        catalog
    }

    #[test]
    fn audit_orders_rarest_first() {
        let catalog = heal_catalog();
        let candidates = plan_heal(&catalog, 3, 10);

        // three-copies is already at redundancy; the other two come
        // back rarest first.
        let repos: Vec<&str> = candidates
            .iter()
            .map(|c| c.manifest.repository.as_str())
            .collect();
        assert_eq!(repos, vec!["one-copy", "two-copies"]);
        assert_eq!(candidates[0].full_holders, BTreeSet::from([1]));
    }

    #[test]
    fn plan_heal_caps_work_per_tick() {
        let catalog = heal_catalog();
        let candidates = plan_heal(&catalog, 3, 1);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].manifest.repository, "one-copy");
    }

    #[test]
    fn plan_heal_empty_when_redundancy_met() {
        let catalog = heal_catalog();
        assert!(plan_heal(&catalog, 1, 10).is_empty());
    }
}

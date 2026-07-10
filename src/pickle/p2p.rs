//! P2P download planning (Phase 12, slice C).
//!
//! A pure planner that decides which layer to fetch from which peer:
//! rarest layers first, load spread across the peers that hold them,
//! duplicates and locally-cached layers skipped. No I/O and no clock —
//! the parallel executor drives the plan; property tests drive the
//! planner across arbitrary topologies.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::replication::Peer;
use super::store::BlobStore;
use super::types::{Digest, ManifestCatalog, PickleError};

/// One planned fetch: this digest, from this peer.
#[derive(Debug, Clone)]
pub struct LayerFetch {
    pub digest: Digest,
    pub peer: Peer,
}

/// The output of [`plan_downloads`].
#[derive(Debug, Default)]
pub struct DownloadPlan {
    /// Planned fetches, rarest layer first.
    pub fetches: Vec<LayerFetch>,
    /// Digests no reachable peer holds — the caller falls back to the
    /// external registry path (or fails the pull honestly).
    pub unavailable: Vec<Digest>,
}

/// Plan which peer serves each needed layer.
///
/// - **Dedup:** a digest appearing twice in `needed` (a config blob
///   that doubles as a layer) is fetched once.
/// - **Skip local:** digests in `local` are excluded — they're already
///   in the blob store.
/// - **Rarest first:** layers with the fewest holding peers are
///   ordered first. When many nodes pull the same image at once, the
///   scarcest blobs spread fastest — the copies whose loss would hurt
///   most gain redundancy soonest.
/// - **Source balancing:** each layer is assigned to the holding peer
///   with the fewest assignments so far (ties broken by node id, so
///   plans are deterministic).
pub fn plan_downloads(
    needed: &[Digest],
    local: &HashSet<Digest>,
    catalog: &ManifestCatalog,
    peers: &[Peer],
    self_node: u64,
) -> DownloadPlan {
    // Dedup while preserving first-seen order, and drop local layers.
    let mut seen: HashSet<&Digest> = HashSet::new();
    let wanted: Vec<&Digest> = needed
        .iter()
        .filter(|d| !local.contains(*d) && seen.insert(*d))
        .collect();

    // For each wanted digest, the peers that hold it (never ourselves).
    let mut candidates: Vec<(&Digest, Vec<&Peer>)> = Vec::with_capacity(wanted.len());
    let mut unavailable = Vec::new();
    for digest in wanted {
        let holders = catalog.layer_holders(digest.as_str());
        let holding_peers: Vec<&Peer> = peers
            .iter()
            .filter(|p| p.node_id != self_node && holders.contains(&p.node_id))
            .collect();
        if holding_peers.is_empty() {
            unavailable.push(digest.clone());
        } else {
            candidates.push((digest, holding_peers));
        }
    }

    // Rarest first: ascending holder count.
    candidates.sort_by_key(|(_, holding)| holding.len());

    // Greedy least-loaded assignment.
    let mut load: HashMap<u64, usize> = HashMap::new();
    let mut fetches = Vec::with_capacity(candidates.len());
    for (digest, holding) in candidates {
        // Peer with the fewest assignments; ties broken by node id so
        // the plan is deterministic regardless of input order.
        let peer = holding
            .into_iter()
            .min_by_key(|p| (load.get(&p.node_id).copied().unwrap_or(0), p.node_id));
        if let Some(peer) = peer {
            *load.entry(peer.node_id).or_insert(0) += 1;
            fetches.push(LayerFetch {
                digest: digest.clone(),
                peer: peer.clone(),
            });
        }
    }

    DownloadPlan {
        fetches,
        unavailable,
    }
}

/// Execute a download plan with bounded concurrency.
///
/// At most `concurrency` fetches run at once in a `JoinSet`; each
/// failed fetch is retried (sequentially, after the parallel pass)
/// against the digest's other holders. A digest that exhausts its
/// holders fails the whole call — the caller decides what a partial
/// image means, not this function.
#[allow(clippy::too_many_arguments)]
pub async fn pull_layers_parallel(
    plan: DownloadPlan,
    repository: &str,
    catalog: &ManifestCatalog,
    peers: &[Peer],
    store: &Arc<BlobStore>,
    client: &reqwest::Client,
    concurrency: usize,
    timeout: Duration,
) -> Result<(), PickleError> {
    let concurrency = concurrency.max(1);
    let mut queue = plan.fetches.into_iter();
    let mut in_flight = tokio::task::JoinSet::new();
    let mut failed: Vec<(Digest, u64)> = Vec::new();

    loop {
        // Keep the window full, then wait for one completion.
        while in_flight.len() < concurrency {
            let Some(fetch) = queue.next() else { break };
            let repository = repository.to_string();
            let store = Arc::clone(store);
            let client = client.clone();
            in_flight.spawn(async move {
                let result = super::pull::pull_layer_from_peer(
                    &fetch.peer,
                    &repository,
                    &fetch.digest,
                    &store,
                    &client,
                    timeout,
                )
                .await;
                (fetch, result)
            });
        }
        let Some(joined) = in_flight.join_next().await else {
            break; // queue drained and nothing in flight
        };
        match joined {
            Ok((_, Ok(()))) => {}
            Ok((fetch, Err(_))) => failed.push((fetch.digest, fetch.peer.node_id)),
            Err(e) => {
                return Err(PickleError::ReplicationFailed(format!(
                    "layer fetch task failed: {e}"
                )));
            }
        }
    }

    // Retry pass: each failure tries the digest's remaining holders.
    for (digest, tried_node) in failed {
        if store.has_blob(&digest) {
            continue;
        }
        let holders = catalog.layer_holders(digest.as_str());
        let mut recovered = false;
        for peer in peers
            .iter()
            .filter(|p| p.node_id != tried_node && holders.contains(&p.node_id))
        {
            if super::pull::pull_layer_from_peer(peer, repository, &digest, store, client, timeout)
                .await
                .is_ok()
            {
                recovered = true;
                break;
            }
        }
        if !recovered {
            return Err(PickleError::ReplicationFailed(format!(
                "layer {digest} unavailable from any holder"
            )));
        }
    }

    Ok(())
}

/// Cluster-backed image source: the Pickle catalog plus P2P layer
/// pulls, installed into the grill's `ImageStore` in cluster mode (and
/// standalone too — it resolves locally-pushed images without peers).
pub struct ClusterSource {
    pub state: super::api::PickleState,
    /// Live gossip membership; `None` standalone (local blobs only).
    pub members:
        Option<tokio::sync::watch::Receiver<Vec<crate::mustard::membership::MembershipSnapshot>>>,
    pub registry_port: u16,
    /// Parallel fetches per image pull (`[images] p2p_concurrency`).
    pub concurrency: usize,
    pub client: reqwest::Client,
    /// Upstream client for the pull-through cache; `None` disables it.
    pub upstream: Option<Arc<dyn super::upstream::UpstreamRegistry>>,
    /// `[images] pull_through` — the cache's master switch.
    pub pull_through: bool,
    /// `[images] cache_recheck_secs`.
    pub cache_recheck_secs: u64,
    /// Serialises cache fills so two instances of the same new image
    /// landing at once don't both download it from upstream. One lock
    /// for all images — the simplest correct thing; per-image locks
    /// are an optimisation nobody has needed yet.
    pub fill_lock: tokio::sync::Mutex<()>,
}

impl ClusterSource {
    /// Resolve `repository:tag` in the catalog and materialise every
    /// blob locally, fetching missing layers from peers in parallel.
    ///
    /// Returns the layer blob paths in manifest order (ready to
    /// unpack), or `None` when the catalog doesn't know the image.
    pub async fn ensure_image_local(
        &self,
        repository: &str,
        tag: &str,
    ) -> Result<Option<Vec<PathBuf>>, PickleError> {
        let peers = match &self.members {
            Some(rx) => crate::cluster::identity::pickle_peers(&rx.borrow(), self.registry_port),
            None => Vec::new(),
        };
        self.ensure_image_local_with_peers(repository, tag, &peers)
            .await
    }

    /// [`Self::ensure_image_local`] with an explicit peer list. Tests
    /// call this directly: in-process registries sit on ephemeral
    /// ports, which the uniform-`registry_port` derivation can't
    /// address.
    pub async fn ensure_image_local_with_peers(
        &self,
        repository: &str,
        tag: &str,
        peers: &[Peer],
    ) -> Result<Option<Vec<PathBuf>>, PickleError> {
        let catalog = self.state.catalog_snapshot().await;
        let Some(manifest) = catalog.get_manifest_by_tag(repository, tag).cloned() else {
            return Ok(None);
        };

        let digests: Vec<Digest> = manifest.all_digests().into_iter().cloned().collect();
        let local: HashSet<Digest> = digests
            .iter()
            .filter(|d| self.state.store.has_blob(d))
            .cloned()
            .collect();

        let plan = plan_downloads(&digests, &local, &catalog, peers, self.state.node_raft_id);
        if !plan.unavailable.is_empty() {
            let missing: Vec<String> = plan.unavailable.iter().map(|d| d.to_string()).collect();
            return Err(PickleError::ReplicationFailed(format!(
                "no reachable holder for layers: {}",
                missing.join(", ")
            )));
        }

        pull_layers_parallel(
            plan,
            repository,
            &catalog,
            peers,
            &self.state.store,
            &self.client,
            self.concurrency,
            Duration::from_secs(30),
        )
        .await?;

        Ok(Some(
            manifest
                .layers
                .iter()
                .map(|layer| self.state.store.blob_path(&layer.digest))
                .collect(),
        ))
    }
}

impl ClusterSource {
    /// Pull-through cache entry point: serve `image` from the cluster
    /// cache, filling it from upstream on a miss or a moved tag.
    ///
    /// `Ok(None)` means the cache is disabled or has no upstream — the
    /// caller falls through to a direct external pull.
    pub async fn ensure_external_image(
        &self,
        image: &crate::grill::image::ImageReference,
    ) -> Result<Option<Vec<PathBuf>>, PickleError> {
        let peers = match &self.members {
            Some(rx) => crate::cluster::identity::pickle_peers(&rx.borrow(), self.registry_port),
            None => Vec::new(),
        };
        self.ensure_external_image_with_peers(image, &peers).await
    }

    /// [`Self::ensure_external_image`] with an explicit peer list
    /// (tests: ephemeral ports).
    pub async fn ensure_external_image_with_peers(
        &self,
        image: &crate::grill::image::ImageReference,
        peers: &[Peer],
    ) -> Result<Option<Vec<PathBuf>>, PickleError> {
        use super::upstream::{CacheDecision, CacheState, decide, refresh_or_refetch};

        if !self.pull_through {
            return Ok(None);
        }
        let Some(upstream) = &self.upstream else {
            return Ok(None);
        };

        let cached_repo = super::upstream::cached_repository(image);
        let recheck = std::time::Duration::from_secs(self.cache_recheck_secs);

        let catalog = self.state.catalog_snapshot().await;
        match decide(
            &catalog,
            &cached_repo,
            &image.tag,
            std::time::SystemTime::now(),
            recheck,
        ) {
            CacheState::Fresh => {
                return self
                    .ensure_image_local_with_peers(&cached_repo, &image.tag, peers)
                    .await;
            }
            CacheState::Stale(cached_digest) => {
                match refresh_or_refetch(upstream.as_ref(), image, &cached_digest).await {
                    Ok(CacheDecision::Hit) | Err(_) => {
                        // Same digest — or upstream unreachable, in
                        // which case a stale cache beats no image:
                        // availability over freshness, stated plainly.
                        return self
                            .ensure_image_local_with_peers(&cached_repo, &image.tag, peers)
                            .await;
                    }
                    Ok(CacheDecision::Refetch) => {}
                }
            }
            CacheState::Miss => {}
        }

        // Fill from upstream, serialised so concurrent misses don't
        // double-download. Re-check after acquiring: another task may
        // have filled while we waited.
        let _guard = self.fill_lock.lock().await;
        let catalog = self.state.catalog_snapshot().await;
        if matches!(
            decide(
                &catalog,
                &cached_repo,
                &image.tag,
                std::time::SystemTime::now(),
                recheck,
            ),
            CacheState::Fresh
        ) {
            return self
                .ensure_image_local_with_peers(&cached_repo, &image.tag, peers)
                .await;
        }

        let manifest = upstream.fetch_manifest(image).await?;
        self.state
            .store
            .write_blob(&manifest.config_bytes, &manifest.config.digest)?;
        for layer in &manifest.layers {
            if self.state.store.has_blob(&layer.digest) {
                continue;
            }
            let bytes = upstream.fetch_blob(image, layer).await?;
            // write_blob verifies the digest — a lying upstream fails here.
            self.state.store.write_blob(&bytes, &layer.digest)?;
        }

        let total_size = manifest.layers.iter().map(|l| l.size).sum();
        let layer_paths = manifest
            .layers
            .iter()
            .map(|layer| self.state.store.blob_path(&layer.digest))
            .collect();
        let image_manifest = super::types::ImageManifest {
            digest: manifest.digest,
            config: manifest.config,
            layers: manifest.layers,
            repository: cached_repo,
            tags: std::iter::once(image.tag.clone()).collect(),
            total_size,
            pushed_at: std::time::SystemTime::now(),
            pushed_by: self.state.node_raft_id,
            // Upstream content was never signable by us; `cache/`
            // repositories are exempt from require_signatures.
            signature: None,
        };
        super::api::record_commit(&self.state, image_manifest, image.tag.clone()).await;

        Ok(Some(layer_paths))
    }
}

impl crate::grill::image::ClusterImageSource for ClusterSource {
    fn fetch_cluster_image<'a>(
        &'a self,
        repository: &'a str,
        tag: &'a str,
    ) -> crate::grill::image::ClusterFetchFuture<'a> {
        Box::pin(async move {
            self.ensure_image_local(repository, tag)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn fetch_pull_through<'a>(
        &'a self,
        image: &'a crate::grill::image::ImageReference,
    ) -> crate::grill::image::ClusterFetchFuture<'a> {
        Box::pin(async move {
            self.ensure_external_image(image)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pickle::types::UpdateLayerLocations;
    use std::collections::BTreeSet;

    fn digest(i: u64) -> Digest {
        Digest::new(&format!("sha256:{i:064x}")).unwrap()
    }

    fn peer(id: u64) -> Peer {
        Peer {
            node_id: id,
            base_url: format!("http://10.0.1.{id}:5000"),
        }
    }

    /// Catalog whose layer i is held by the given node sets.
    fn catalog_with(holders: &[(u64, &[u64])]) -> ManifestCatalog {
        let mut catalog = ManifestCatalog::default();
        catalog.apply_update_locations(&UpdateLayerLocations {
            updates: holders
                .iter()
                .map(|(i, nodes)| (digest(*i), nodes.iter().copied().collect::<BTreeSet<u64>>()))
                .collect(),
        });
        catalog
    }

    #[test]
    fn plan_orders_rarest_first() {
        // Layer 1 has two holders, layer 2 has one.
        let catalog = catalog_with(&[(1, &[2, 3]), (2, &[3])]);
        let peers = vec![peer(2), peer(3)];

        let plan = plan_downloads(
            &[digest(1), digest(2)],
            &HashSet::new(),
            &catalog,
            &peers,
            1,
        );

        assert_eq!(plan.fetches.len(), 2);
        assert_eq!(plan.fetches[0].digest, digest(2), "rarest layer first");
        assert!(plan.unavailable.is_empty());
    }

    #[test]
    fn plan_balances_across_sources() {
        // Six layers, all held by peers 2 and 3: neither gets more
        // than three.
        let layers: Vec<(u64, &[u64])> = (1..=6).map(|i| (i, [2u64, 3].as_slice())).collect();
        let catalog = catalog_with(&layers);
        let peers = vec![peer(2), peer(3)];
        let needed: Vec<Digest> = (1..=6).map(digest).collect();

        let plan = plan_downloads(&needed, &HashSet::new(), &catalog, &peers, 1);

        let mut per_peer: HashMap<u64, usize> = HashMap::new();
        for fetch in &plan.fetches {
            *per_peer.entry(fetch.peer.node_id).or_insert(0) += 1;
        }
        assert_eq!(plan.fetches.len(), 6);
        assert!(
            per_peer.values().all(|&n| n <= 3),
            "unbalanced: {per_peer:?}"
        );
    }

    #[test]
    fn plan_dedups_digests() {
        let catalog = catalog_with(&[(1, &[2])]);
        let peers = vec![peer(2)];

        // The same digest twice (config blob doubling as a layer).
        let plan = plan_downloads(
            &[digest(1), digest(1)],
            &HashSet::new(),
            &catalog,
            &peers,
            1,
        );

        assert_eq!(plan.fetches.len(), 1);
    }

    #[test]
    fn plan_skips_local_layers() {
        let catalog = catalog_with(&[(1, &[2]), (2, &[2])]);
        let peers = vec![peer(2)];
        let local: HashSet<Digest> = [digest(1)].into_iter().collect();

        let plan = plan_downloads(&[digest(1), digest(2)], &local, &catalog, &peers, 1);

        assert_eq!(plan.fetches.len(), 1);
        assert_eq!(plan.fetches[0].digest, digest(2));
    }

    #[test]
    fn plan_reports_unavailable_layers() {
        // Layer 2's only holder is ourselves; layer 3 has no holders.
        let catalog = catalog_with(&[(1, &[2]), (2, &[1]), (3, &[])]);
        let peers = vec![peer(2)];

        let plan = plan_downloads(
            &[digest(1), digest(2), digest(3)],
            &HashSet::new(),
            &catalog,
            &peers,
            1,
        );

        assert_eq!(plan.fetches.len(), 1);
        assert_eq!(plan.unavailable, vec![digest(2), digest(3)]);
    }

    // -- properties -----------------------------------------------------

    use proptest::prelude::*;

    /// An arbitrary topology: for each layer, the subset of peer ids
    /// (1..=n_peers) holding it. May be empty.
    fn arbitrary_topology() -> impl Strategy<Value = (Vec<Vec<u64>>, u64)> {
        (1u64..=8).prop_flat_map(|n_peers| {
            (
                proptest::collection::vec(
                    proptest::collection::btree_set(1u64..=n_peers, 0..=n_peers as usize)
                        .prop_map(|s| s.into_iter().collect::<Vec<u64>>()),
                    1..40,
                ),
                Just(n_peers),
            )
        })
    }

    proptest! {
        /// Every layer with at least one live holder is assigned
        /// exactly once; every layer with none lands in `unavailable`.
        #[test]
        fn complete_coverage((topology, n_peers) in arbitrary_topology()) {
            let holders: Vec<(u64, &[u64])> = topology
                .iter()
                .enumerate()
                .map(|(i, nodes)| (i as u64 + 1, nodes.as_slice()))
                .collect();
            let catalog = catalog_with(&holders);
            let peers: Vec<Peer> = (1..=n_peers).map(peer).collect();
            let needed: Vec<Digest> = (1..=topology.len() as u64).map(digest).collect();

            // self_node = 0: never a peer, so "live holder" == any holder.
            let plan = plan_downloads(&needed, &HashSet::new(), &catalog, &peers, 0);

            prop_assert_eq!(
                plan.fetches.len() + plan.unavailable.len(),
                topology.len()
            );
            let fetched: HashSet<&Digest> = plan.fetches.iter().map(|f| &f.digest).collect();
            prop_assert_eq!(fetched.len(), plan.fetches.len(), "a digest was fetched twice");
            for (i, nodes) in topology.iter().enumerate() {
                let d = digest(i as u64 + 1);
                if nodes.is_empty() {
                    prop_assert!(plan.unavailable.contains(&d));
                } else {
                    prop_assert!(fetched.contains(&d));
                }
            }
        }

        /// No layer is ever assigned to a peer that doesn't hold it.
        #[test]
        fn holders_only((topology, n_peers) in arbitrary_topology()) {
            let holders: Vec<(u64, &[u64])> = topology
                .iter()
                .enumerate()
                .map(|(i, nodes)| (i as u64 + 1, nodes.as_slice()))
                .collect();
            let catalog = catalog_with(&holders);
            let peers: Vec<Peer> = (1..=n_peers).map(peer).collect();
            let needed: Vec<Digest> = (1..=topology.len() as u64).map(digest).collect();

            let plan = plan_downloads(&needed, &HashSet::new(), &catalog, &peers, 0);

            for fetch in &plan.fetches {
                let holders = catalog.layer_holders(fetch.digest.as_str());
                prop_assert!(
                    holders.contains(&fetch.peer.node_id),
                    "{} assigned to non-holder {}",
                    fetch.digest,
                    fetch.peer.node_id
                );
            }
        }

        /// With a uniform topology (every layer held by the same k
        /// peers), greedy least-loaded stays within ceil(n/k) per peer.
        /// (Arbitrary topologies can't promise this: a sole holder of
        /// many layers must take them all.)
        #[test]
        fn balance_bound_uniform(
            n_layers in 1usize..40,
            k in 1u64..=8,
        ) {
            let holder_ids: Vec<u64> = (1..=k).collect();
            let holders: Vec<(u64, &[u64])> = (1..=n_layers as u64)
                .map(|i| (i, holder_ids.as_slice()))
                .collect();
            let catalog = catalog_with(&holders);
            let peers: Vec<Peer> = (1..=k).map(peer).collect();
            let needed: Vec<Digest> = (1..=n_layers as u64).map(digest).collect();

            let plan = plan_downloads(&needed, &HashSet::new(), &catalog, &peers, 0);

            let mut per_peer: HashMap<u64, usize> = HashMap::new();
            for fetch in &plan.fetches {
                *per_peer.entry(fetch.peer.node_id).or_insert(0) += 1;
            }
            let bound = n_layers.div_ceil(k as usize);
            prop_assert!(
                per_peer.values().all(|&n| n <= bound),
                "load {per_peer:?} exceeds ceil({n_layers}/{k}) = {bound}"
            );
        }
    }
}

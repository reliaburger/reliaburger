//! P2P download planning (Phase 12, slice C).
//!
//! A pure planner that decides which layer to fetch from which peer:
//! rarest layers first, load spread across the peers that hold them,
//! duplicates and locally-cached layers skipped. No I/O and no clock —
//! the parallel executor drives the plan; property tests drive the
//! planner across arbitrary topologies.

use std::collections::{HashMap, HashSet};

use super::replication::Peer;
use super::types::{Digest, ManifestCatalog};

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

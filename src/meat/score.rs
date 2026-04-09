/// Phase 2: Score.
///
/// Ranks candidate nodes on a 0–100 scale. Higher is better.
/// The score is a weighted sum of several dimensions:
/// - Bin-packing (50%): prefer fuller nodes to maximise density
/// - Preferred labels (20%): prefer nodes matching soft constraints
/// - Spread (10%): penalise nodes already running the same app
/// - Stability (5%): prefer longer-running nodes (placeholder)
/// - Image locality (15%): prefer nodes with cached images (placeholder)
use std::collections::BTreeMap;

use super::cluster_state::ClusterStateCache;
use super::types::{AppId, NodeId, Resources};

/// Score weights (out of 100).
const WEIGHT_BIN_PACK: u32 = 50;
const WEIGHT_PREFERRED: u32 = 20;
const WEIGHT_IMAGE: u32 = 15;
const WEIGHT_SPREAD: u32 = 10;
const WEIGHT_STABILITY: u32 = 5;

/// Score all candidate nodes and return them sorted by score (descending),
/// then by NodeId (ascending) for deterministic tiebreak.
pub fn score_nodes(
    candidates: &[NodeId],
    app_id: &AppId,
    resources: &Resources,
    preferred_labels: &BTreeMap<String, String>,
    cluster: &ClusterStateCache,
    image: Option<&str>,
) -> Vec<(NodeId, u32)> {
    let mut scored: Vec<(NodeId, u32)> = candidates
        .iter()
        .filter_map(|node_id| {
            cluster.get_node(node_id)?;
            let score = compute_score(node_id, app_id, resources, preferred_labels, cluster, image);
            Some((node_id.clone(), score))
        })
        .collect();

    // Sort by score descending, then node_id ascending for tiebreak
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored
}

/// Compute the weighted score for a single node.
fn compute_score(
    node_id: &NodeId,
    app_id: &AppId,
    resources: &Resources,
    preferred_labels: &BTreeMap<String, String>,
    cluster: &ClusterStateCache,
    image: Option<&str>,
) -> u32 {
    let node = match cluster.get_node(node_id) {
        Some(n) => n,
        None => return 0,
    };

    // Bin-packing: prefer nodes that will be more utilised after placement.
    // Score = utilisation_after / allocatable * 100
    // Nodes with zero allocatable CPU get a neutral score of 50.
    let bin_pack = if node.allocatable.cpu_millicores == 0 {
        50u32
    } else {
        let allocated_after = node.allocated.cpu_millicores + resources.cpu_millicores;
        let utilisation = (allocated_after * 100 / node.allocatable.cpu_millicores).min(100);
        utilisation as u32
    };

    // Preferred labels: proportion of preferred labels that match.
    let preferred = if preferred_labels.is_empty() {
        100
    } else {
        let matches = node.preferred_label_matches(preferred_labels);
        (matches * 100 / preferred_labels.len()) as u32
    };

    // Spread: penalise if other replicas of the same app are on this node.
    let spread = if node.running_apps.contains(app_id) {
        0
    } else {
        100
    };

    // Stability: prefer nodes with longer uptime. Linear ramp from
    // 0 (just joined) to 100 (24+ hours). Freshly joined nodes may
    // still be catching up on state reconstruction or image pulls.
    let stability = ((node.uptime_secs.min(86400) * 100) / 86400) as u32;

    // Image locality: 100 if the node already has the image cached,
    // 0 otherwise. Avoids pulling layers over the network.
    let image_locality = if let Some(image) = image {
        if node.cached_images.contains(image) {
            100
        } else {
            0
        }
    } else {
        0
    };

    // Weighted sum
    let total = bin_pack * WEIGHT_BIN_PACK
        + preferred * WEIGHT_PREFERRED
        + image_locality * WEIGHT_IMAGE
        + spread * WEIGHT_SPREAD
        + stability * WEIGHT_STABILITY;

    total / 100
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::meat::cluster_state::{ClusterStateCache, SchedulerNodeState};

    fn node_state(
        name: &str,
        cpu_alloc: u64,
        cpu_used: u64,
        labels: BTreeMap<String, String>,
    ) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(name),
            allocatable: Resources::new(cpu_alloc, 4096, 0),
            allocated: Resources::new(cpu_used, 0, 0),
            labels,
            ready: true,
            running_apps: HashSet::new(),
            uptime_secs: 86400, // 24h — full stability score
            cached_images: HashSet::new(),
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn bin_packing_prefers_fuller_nodes() {
        let mut cluster = ClusterStateCache::new();
        // "full" has 800/1000 CPU used — will be 90% after placing 100m
        cluster.set_node(node_state("full", 1000, 800, BTreeMap::new()));
        // "empty" has 0/1000 CPU used — will be 10% after placing 100m
        cluster.set_node(node_state("empty", 1000, 0, BTreeMap::new()));

        let candidates = vec![NodeId::new("full"), NodeId::new("empty")];
        let app = AppId::new("web", "prod");
        let res = Resources::new(100, 100, 0);

        let scored = score_nodes(&candidates, &app, &res, &BTreeMap::new(), &cluster, None);

        // "full" should score higher due to bin-packing preference
        assert_eq!(scored[0].0, NodeId::new("full"));
        assert!(scored[0].1 > scored[1].1);
    }

    #[test]
    fn preferred_labels_boost_score() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state(
            "match",
            1000,
            500,
            labels(&[("zone", "us-east")]),
        ));
        cluster.set_node(node_state("no-match", 1000, 500, BTreeMap::new()));

        let candidates = vec![NodeId::new("match"), NodeId::new("no-match")];
        let app = AppId::new("web", "prod");
        let res = Resources::new(100, 100, 0);
        let preferred = labels(&[("zone", "us-east")]);

        let scored = score_nodes(&candidates, &app, &res, &preferred, &cluster, None);

        // "match" should score higher
        assert_eq!(scored[0].0, NodeId::new("match"));
        assert!(scored[0].1 > scored[1].1);
    }

    #[test]
    fn spread_penalises_same_app_on_node() {
        let mut cluster = ClusterStateCache::new();
        let app = AppId::new("web", "prod");

        let mut has_app = node_state("has-app", 1000, 500, BTreeMap::new());
        has_app.running_apps.insert(app.clone());
        cluster.set_node(has_app);

        cluster.set_node(node_state("no-app", 1000, 500, BTreeMap::new()));

        let candidates = vec![NodeId::new("has-app"), NodeId::new("no-app")];
        let res = Resources::new(100, 100, 0);

        let scored = score_nodes(&candidates, &app, &res, &BTreeMap::new(), &cluster, None);

        // "no-app" should score higher (spread bonus)
        assert_eq!(scored[0].0, NodeId::new("no-app"));
    }

    #[test]
    fn deterministic_tiebreak_by_node_id() {
        let mut cluster = ClusterStateCache::new();
        // Identical nodes — same CPU, same labels, no apps
        cluster.set_node(node_state("b-node", 1000, 500, BTreeMap::new()));
        cluster.set_node(node_state("a-node", 1000, 500, BTreeMap::new()));
        cluster.set_node(node_state("c-node", 1000, 500, BTreeMap::new()));

        let candidates = vec![
            NodeId::new("c-node"),
            NodeId::new("a-node"),
            NodeId::new("b-node"),
        ];
        let app = AppId::new("web", "prod");
        let res = Resources::new(100, 100, 0);

        let scored = score_nodes(&candidates, &app, &res, &BTreeMap::new(), &cluster, None);

        // All same score — should be sorted by node ID ascending
        assert_eq!(scored[0].0, NodeId::new("a-node"));
        assert_eq!(scored[1].0, NodeId::new("b-node"));
        assert_eq!(scored[2].0, NodeId::new("c-node"));
    }

    #[test]
    fn stability_prefers_longer_uptime() {
        let mut cluster = ClusterStateCache::new();
        let mut fresh = node_state("fresh", 1000, 500, BTreeMap::new());
        fresh.uptime_secs = 60; // 1 minute
        cluster.set_node(fresh);

        let mut veteran = node_state("veteran", 1000, 500, BTreeMap::new());
        veteran.uptime_secs = 86400; // 24 hours
        cluster.set_node(veteran);

        let candidates = vec![NodeId::new("fresh"), NodeId::new("veteran")];
        let app = AppId::new("web", "prod");
        let res = Resources::new(100, 100, 0);

        let scored = score_nodes(&candidates, &app, &res, &BTreeMap::new(), &cluster, None);

        assert_eq!(scored[0].0, NodeId::new("veteran"));
        assert!(scored[0].1 > scored[1].1);
    }

    #[test]
    fn image_locality_prefers_cached_node() {
        let mut cluster = ClusterStateCache::new();
        let mut has_image = node_state("has-image", 1000, 500, BTreeMap::new());
        has_image.cached_images.insert("myapp:v1".to_string());
        cluster.set_node(has_image);

        cluster.set_node(node_state("no-image", 1000, 500, BTreeMap::new()));

        let candidates = vec![NodeId::new("has-image"), NodeId::new("no-image")];
        let app = AppId::new("web", "prod");
        let res = Resources::new(100, 100, 0);

        let scored = score_nodes(
            &candidates,
            &app,
            &res,
            &BTreeMap::new(),
            &cluster,
            Some("myapp:v1"),
        );

        assert_eq!(scored[0].0, NodeId::new("has-image"));
        assert!(scored[0].1 > scored[1].1);
    }

    #[test]
    fn image_locality_no_effect_without_image() {
        let mut cluster = ClusterStateCache::new();
        let mut has_image = node_state("has-image", 1000, 500, BTreeMap::new());
        has_image.cached_images.insert("myapp:v1".to_string());
        cluster.set_node(has_image);

        cluster.set_node(node_state("no-image", 1000, 500, BTreeMap::new()));

        let candidates = vec![NodeId::new("has-image"), NodeId::new("no-image")];
        let app = AppId::new("web", "prod");
        let res = Resources::new(100, 100, 0);

        // No image specified — image locality should not affect score
        let scored = score_nodes(&candidates, &app, &res, &BTreeMap::new(), &cluster, None);

        // Scores should be equal (tiebreak by node ID)
        assert_eq!(scored[0].1, scored[1].1);
    }
}

/// Phase 2: Score.
///
/// Ranks candidate nodes. Spread comes first: a node running fewer replicas
/// of the app always outranks one running more. Among nodes that run the
/// same number, a weighted score on a 0–90 scale decides (higher is better):
/// - Bin-packing (50): prefer fuller nodes to maximise density
/// - Preferred labels (20): prefer nodes matching soft constraints
/// - Image locality (15): prefer nodes with cached images
/// - Stability (5): prefer longer-running nodes
use std::collections::BTreeMap;

use super::cluster_state::ClusterStateCache;
use super::types::{AppId, NodeId, Resources};

/// Score weights.
///
/// Spread isn't a weight. It used to be one: first 10 against bin-pack's
/// 50, which put every replica of an app on the same node (H8), then 60,
/// scored 0 or 100 on whether the node ran the app at all. That still let
/// bin-packing choose between two nodes that both ran it, so losing a node
/// could stack its replicas on the busier survivor (#346). Replicas exist to
/// survive a node failure, so the replica count is the first sort key and no
/// weighting can outvote it.
const WEIGHT_BIN_PACK: u32 = 50;
const WEIGHT_PREFERRED: u32 = 20;
const WEIGHT_IMAGE: u32 = 15;
const WEIGHT_STABILITY: u32 = 5;

/// Score all candidate nodes and return them best first: fewest replicas of
/// `app_id`, then highest score, then lowest `NodeId` for a deterministic
/// tiebreak.
pub fn score_nodes(
    candidates: &[NodeId],
    app_id: &AppId,
    resources: &Resources,
    preferred_labels: &BTreeMap<String, String>,
    cluster: &ClusterStateCache,
    image: Option<&str>,
) -> Vec<(NodeId, u32)> {
    let mut scored: Vec<(NodeId, u32, u32)> = candidates
        .iter()
        .filter_map(|node_id| {
            let replicas = cluster.get_node(node_id)?.replicas_of(app_id);
            let score = compute_score(node_id, resources, preferred_labels, cluster, image);
            Some((node_id.clone(), replicas, score))
        })
        .collect();

    scored.sort_by(|a, b| a.1.cmp(&b.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0)));
    scored
        .into_iter()
        .map(|(node_id, _, score)| (node_id, score))
        .collect()
}

/// Compute the weighted score for a single node.
fn compute_score(
    node_id: &NodeId,
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
    let bin_pack = {
        let allocated_after = node.allocated.cpu_millicores + resources.cpu_millicores;
        (allocated_after * 100)
            .checked_div(node.allocatable.cpu_millicores)
            .map(|v| v.min(100) as u32)
            .unwrap_or(50u32)
    };

    // Preferred labels: proportion of preferred labels that match.
    let preferred = if preferred_labels.is_empty() {
        100
    } else {
        let matches = node.preferred_label_matches(preferred_labels);
        (matches * 100 / preferred_labels.len()) as u32
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
            capabilities: Default::default(),
            app_replicas: Default::default(),
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
        has_app.app_replicas.insert(app.clone(), 1);
        cluster.set_node(has_app);

        cluster.set_node(node_state("no-app", 1000, 500, BTreeMap::new()));

        let candidates = vec![NodeId::new("has-app"), NodeId::new("no-app")];
        let res = Resources::new(100, 100, 0);

        let scored = score_nodes(&candidates, &app, &res, &BTreeMap::new(), &cluster, None);

        assert_eq!(scored[0].0, NodeId::new("no-app"));
    }

    /// #346: two survivors both run the app. The one with fewer replicas
    /// wins, however much fuller the other is.
    #[test]
    fn fewer_replicas_outrank_a_fuller_node() {
        let mut cluster = ClusterStateCache::new();
        let app = AppId::new("web", "prod");

        let mut busy = node_state("busy", 1000, 890, labels(&[("zone", "a")]));
        busy.app_replicas.insert(app.clone(), 2);
        busy.cached_images.insert("web:v1".to_string());
        cluster.set_node(busy);
        let mut idle = node_state("idle", 1000, 0, BTreeMap::new());
        idle.app_replicas.insert(app.clone(), 1);
        idle.uptime_secs = 0;
        cluster.set_node(idle);

        let candidates = vec![NodeId::new("busy"), NodeId::new("idle")];
        let res = Resources::new(100, 100, 0);
        let scored = score_nodes(
            &candidates,
            &app,
            &res,
            &labels(&[("zone", "a")]),
            &cluster,
            Some("web:v1"),
        );

        assert_eq!(scored[0].0, NodeId::new("idle"));
        assert!(
            scored[0].1 < scored[1].1,
            "the busy node scores higher on every weight: {scored:?}"
        );
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

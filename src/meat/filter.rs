/// Phase 1: Filter.
///
/// Eliminates nodes that cannot run the workload. Checks capacity,
/// required labels, and readiness.
use std::collections::BTreeMap;

use super::cluster_state::ClusterStateCache;
use super::types::{NodeId, Resources};

/// Filter the cluster to nodes eligible for placing this workload.
///
/// A node passes the filter if:
/// 1. It is ready (not unknown, draining, or cordoned).
/// 2. It has sufficient allocatable resources after current allocations.
/// 3. It matches all required placement labels.
pub fn filter_nodes(
    resources: &Resources,
    required_labels: &BTreeMap<String, String>,
    cluster: &ClusterStateCache,
) -> Vec<NodeId> {
    cluster
        .nodes()
        .filter(|node| {
            node.ready && node.can_fit(resources) && node.matches_labels(required_labels)
        })
        .map(|node| node.node_id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::meat::cluster_state::SchedulerNodeState;

    fn node_state(
        name: &str,
        cpu: u64,
        mem: u64,
        labels: BTreeMap<String, String>,
        ready: bool,
    ) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(name),
            allocatable: Resources::new(cpu, mem, 0),
            allocated: Resources::default(),
            labels,
            ready,
            running_apps: HashSet::new(),
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn filters_out_nodes_without_capacity() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("big", 2000, 4096, BTreeMap::new(), true));
        cluster.set_node(node_state("small", 100, 256, BTreeMap::new(), true));

        let result = filter_nodes(&Resources::new(500, 1024, 0), &BTreeMap::new(), &cluster);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], NodeId::new("big"));
    }

    #[test]
    fn filters_out_nodes_missing_required_labels() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state(
            "east",
            2000,
            4096,
            labels(&[("zone", "us-east")]),
            true,
        ));
        cluster.set_node(node_state(
            "west",
            2000,
            4096,
            labels(&[("zone", "us-west")]),
            true,
        ));

        let required = labels(&[("zone", "us-east")]);
        let result = filter_nodes(&Resources::new(100, 100, 0), &required, &cluster);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], NodeId::new("east"));
    }

    #[test]
    fn filters_out_non_ready_nodes() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("ready", 2000, 4096, BTreeMap::new(), true));
        cluster.set_node(node_state("not-ready", 2000, 4096, BTreeMap::new(), false));

        let result = filter_nodes(&Resources::new(100, 100, 0), &BTreeMap::new(), &cluster);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], NodeId::new("ready"));
    }

    #[test]
    fn returns_all_eligible_nodes() {
        let mut cluster = ClusterStateCache::new();
        for i in 0..5 {
            cluster.set_node(node_state(
                &format!("n{i}"),
                2000,
                4096,
                BTreeMap::new(),
                true,
            ));
        }

        let result = filter_nodes(&Resources::new(100, 100, 0), &BTreeMap::new(), &cluster);
        assert_eq!(result.len(), 5);
    }
}

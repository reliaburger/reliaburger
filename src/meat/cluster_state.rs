/// Cluster state cache for the scheduler.
///
/// Tracks per-node capacity, labels, and running apps. Updated from
/// the membership table and aggregated StateReports. The scheduler
/// reads this cache during the Filter and Score phases and updates
/// it after each placement to reflect reserved resources.
use std::collections::{BTreeMap, HashMap, HashSet};

use super::types::{AppId, NodeId, Resources};

/// Per-node state as seen by the scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerNodeState {
    /// Node identifier.
    pub node_id: NodeId,
    /// Total resources available for scheduling (total - reserved).
    pub allocatable: Resources,
    /// Resources currently allocated to running workloads.
    pub allocated: Resources,
    /// Node labels (zone, gpu_model, etc.).
    pub labels: BTreeMap<String, String>,
    /// Whether the node is ready to accept new workloads.
    pub ready: bool,
    /// Apps currently running on this node (for spread scoring).
    pub running_apps: HashSet<AppId>,
}

impl SchedulerNodeState {
    /// Resources remaining after current allocations.
    pub fn available(&self) -> Resources {
        self.allocatable.saturating_sub(&self.allocated)
    }

    /// Whether this node can fit the requested resources.
    pub fn can_fit(&self, required: &Resources) -> bool {
        self.available().fits(required)
    }

    /// Whether this node matches all required labels.
    pub fn matches_labels(&self, required: &BTreeMap<String, String>) -> bool {
        required
            .iter()
            .all(|(k, v)| self.labels.get(k).is_some_and(|lv| lv == v))
    }

    /// Count how many of the preferred labels this node matches.
    pub fn preferred_label_matches(&self, preferred: &BTreeMap<String, String>) -> usize {
        preferred
            .iter()
            .filter(|(k, v)| self.labels.get(*k).is_some_and(|lv| lv == *v))
            .count()
    }
}

/// The scheduler's view of the cluster.
pub struct ClusterStateCache {
    nodes: HashMap<NodeId, SchedulerNodeState>,
}

impl ClusterStateCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
        }
    }

    /// Add or replace a node's state.
    pub fn set_node(&mut self, state: SchedulerNodeState) {
        self.nodes.insert(state.node_id.clone(), state);
    }

    /// Get a node's state.
    pub fn get_node(&self, node_id: &NodeId) -> Option<&SchedulerNodeState> {
        self.nodes.get(node_id)
    }

    /// All node IDs in the cache.
    pub fn node_ids(&self) -> Vec<NodeId> {
        self.nodes.keys().cloned().collect()
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Reserve resources on a node after a placement decision.
    ///
    /// Adds `resources` to the node's `allocated` total and records
    /// the app as running on that node.
    pub fn reserve(&mut self, node_id: &NodeId, app_id: &AppId, resources: &Resources) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.allocated = node.allocated.saturating_add(resources);
            node.running_apps.insert(app_id.clone());
        }
    }

    /// Release resources on a node.
    pub fn release(&mut self, node_id: &NodeId, app_id: &AppId, resources: &Resources) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.allocated = node.allocated.saturating_sub(resources);
            node.running_apps.remove(app_id);
        }
    }

    /// Iterate over all nodes.
    pub fn nodes(&self) -> impl Iterator<Item = &SchedulerNodeState> {
        self.nodes.values()
    }
}

impl Default for ClusterStateCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn node_state(name: &str, cpu: u64, mem: u64) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(name),
            allocatable: Resources::new(cpu, mem, 0),
            allocated: Resources::default(),
            labels: BTreeMap::new(),
            ready: true,
            running_apps: HashSet::new(),
        }
    }

    #[test]
    fn reserve_reduces_available() {
        let mut cache = ClusterStateCache::new();
        cache.set_node(node_state("n1", 1000, 1024));

        let app = AppId::new("web", "prod");
        let res = Resources::new(500, 512, 0);
        cache.reserve(&NodeId::new("n1"), &app, &res);

        let n1 = cache.get_node(&NodeId::new("n1")).unwrap();
        assert_eq!(n1.available().cpu_millicores, 500);
        assert_eq!(n1.available().memory_bytes, 512);
        assert!(n1.running_apps.contains(&app));
    }

    #[test]
    fn release_restores_available() {
        let mut cache = ClusterStateCache::new();
        cache.set_node(node_state("n1", 1000, 1024));

        let app = AppId::new("web", "prod");
        let res = Resources::new(500, 512, 0);
        cache.reserve(&NodeId::new("n1"), &app, &res);
        cache.release(&NodeId::new("n1"), &app, &res);

        let n1 = cache.get_node(&NodeId::new("n1")).unwrap();
        assert_eq!(n1.available().cpu_millicores, 1000);
        assert!(!n1.running_apps.contains(&app));
    }

    #[test]
    fn matches_labels_all_required() {
        let mut state = node_state("n1", 1000, 1024);
        state
            .labels
            .insert("zone".to_string(), "us-east".to_string());
        state.labels.insert("ssd".to_string(), "true".to_string());

        let mut required = BTreeMap::new();
        required.insert("zone".to_string(), "us-east".to_string());
        assert!(state.matches_labels(&required));

        required.insert("ssd".to_string(), "false".to_string());
        assert!(!state.matches_labels(&required));
    }
}

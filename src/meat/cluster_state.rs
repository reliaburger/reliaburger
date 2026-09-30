/// Cluster state cache for the scheduler.
///
/// Tracks per-node capacity, labels, and running apps. Updated from
/// the membership table and aggregated StateReports. The scheduler
/// reads this cache during the Filter and Score phases and updates
/// it after each placement to reflect reserved resources.
use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::types::{AppId, NodeId, Resources};

/// Live node capabilities that affect placement correctness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapabilities {
    /// Kernel hooks and runtime support for pre-start egress allowlists.
    pub egress: crate::sesame::egress::EgressEnforcementCapability,
    /// Ready resolver transports and workload reachability.
    pub dns: crate::onion::dns::DnsCapability,
}

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
    /// Live enforcement capabilities reported by the node.
    pub capabilities: NodeCapabilities,
    /// How many replicas of each app this node runs (for spread).
    pub app_replicas: HashMap<AppId, u32>,
    /// How long this node has been alive, in seconds. Used for stability
    /// scoring — prefer nodes with longer uptime over freshly joined ones.
    pub uptime_secs: u64,
    /// Set of image references cached locally (e.g. "myapp:v1").
    /// Used for image locality scoring — prefer nodes that already have
    /// the required image layers.
    pub cached_images: HashSet<String>,
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

    /// How many replicas of `app_id` this node runs.
    pub fn replicas_of(&self, app_id: &AppId) -> u32 {
        self.app_replicas.get(app_id).copied().unwrap_or(0)
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
#[derive(Clone)]
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
    /// Adds `resources` to the node's `allocated` total and counts one
    /// more replica of the app on that node.
    pub fn reserve(&mut self, node_id: &NodeId, app_id: &AppId, resources: &Resources) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.allocated = node.allocated.saturating_add(resources);
            *node.app_replicas.entry(app_id.clone()).or_default() += 1;
        }
    }

    /// Release resources on a node, and one replica of the app.
    pub fn release(&mut self, node_id: &NodeId, app_id: &AppId, resources: &Resources) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.allocated = node.allocated.saturating_sub(resources);
            if let Some(count) = node.app_replicas.get_mut(app_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    node.app_replicas.remove(app_id);
                }
            }
        }
    }

    /// Say how many replicas of `app_id` a node runs, whatever its report
    /// said. Resources are untouched: a report already counts what runs.
    pub fn set_replicas(&mut self, node_id: &NodeId, app_id: &AppId, count: u32) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            if count == 0 {
                node.app_replicas.remove(app_id);
            } else {
                node.app_replicas.insert(app_id.clone(), count);
            }
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
            capabilities: NodeCapabilities::default(),
            app_replicas: HashMap::new(),
            uptime_secs: 3600,
            cached_images: HashSet::new(),
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
        assert_eq!(n1.replicas_of(&app), 1);
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
        assert_eq!(n1.replicas_of(&app), 0);
    }

    #[test]
    fn reserve_and_release_count_replicas_of_an_app() {
        let mut cache = ClusterStateCache::new();
        cache.set_node(node_state("n1", 1000, 1024));
        let node = NodeId::new("n1");
        let app = AppId::new("web", "prod");
        let res = Resources::new(100, 0, 0);

        cache.reserve(&node, &app, &res);
        cache.reserve(&node, &app, &res);
        assert_eq!(cache.get_node(&node).unwrap().replicas_of(&app), 2);

        cache.release(&node, &app, &res);
        assert_eq!(cache.get_node(&node).unwrap().replicas_of(&app), 1);

        cache.set_replicas(&node, &app, 3);
        assert_eq!(cache.get_node(&node).unwrap().replicas_of(&app), 3);
        assert_eq!(cache.get_node(&node).unwrap().allocated.cpu_millicores, 100);
        cache.set_replicas(&node, &app, 0);
        assert!(cache.get_node(&node).unwrap().app_replicas.is_empty());
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

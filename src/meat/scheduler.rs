/// Meat scheduler: the four-phase placement pipeline.
///
/// Filter → Score → Select → Commit. For each replica, the pipeline
/// runs iteratively: after placing one replica, the cluster state
/// cache is updated to reflect the reserved resources before placing
/// the next. This prevents over-committing a single node.
use std::collections::BTreeMap;

use crate::config::app::AppSpec;
use crate::config::types::Replicas;

use super::cluster_state::ClusterStateCache;
use super::filter::filter_nodes;
use super::score::score_nodes;
use super::types::{AppId, Placement, Resources, SchedulingDecision};

/// Scheduling errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("no eligible nodes for app {app_id:?}")]
    NoEligibleNodes { app_id: AppId },

    #[error("quota exceeded for namespace {namespace:?}: {detail}")]
    QuotaExceeded { namespace: String, detail: String },

    #[error("invalid spec: {reason}")]
    InvalidSpec { reason: String },
}

/// The Meat scheduler.
pub struct Scheduler {
    /// Mutable cluster state, updated after each placement.
    pub cluster: ClusterStateCache,
}

impl Scheduler {
    /// Create a new scheduler with the given cluster state.
    pub fn new(cluster: ClusterStateCache) -> Self {
        Self { cluster }
    }

    /// Schedule an app according to its spec.
    ///
    /// Returns a `SchedulingDecision` with one `Placement` per replica.
    /// For daemon mode, places one replica on every eligible node.
    pub fn schedule_app(
        &mut self,
        app_id: &AppId,
        spec: &AppSpec,
    ) -> Result<SchedulingDecision, ScheduleError> {
        let resources = self.extract_resources(spec);
        let (required, preferred) = self.parse_labels(spec);
        let image = spec.image.as_deref();

        match spec.replicas {
            Replicas::Fixed(n) => {
                self.schedule_fixed(app_id, n, &resources, &required, &preferred, image)
            }
            Replicas::DaemonSet => self.schedule_daemon(app_id, &resources, &required),
        }
    }

    /// Schedule N fixed replicas.
    fn schedule_fixed(
        &mut self,
        app_id: &AppId,
        replica_count: u32,
        resources: &Resources,
        required_labels: &BTreeMap<String, String>,
        preferred_labels: &BTreeMap<String, String>,
        image: Option<&str>,
    ) -> Result<SchedulingDecision, ScheduleError> {
        let mut placements = Vec::with_capacity(replica_count as usize);

        for _ in 0..replica_count {
            // Phase 1: Filter
            let candidates = filter_nodes(resources, required_labels, &self.cluster);
            if candidates.is_empty() {
                return Err(ScheduleError::NoEligibleNodes {
                    app_id: app_id.clone(),
                });
            }

            // Phase 2: Score
            let scored = score_nodes(
                &candidates,
                app_id,
                resources,
                preferred_labels,
                &self.cluster,
                image,
            );

            // Phase 3: Select (highest score, deterministic tiebreak)
            let (selected_node, _score) =
                scored
                    .first()
                    .ok_or_else(|| ScheduleError::NoEligibleNodes {
                        app_id: app_id.clone(),
                    })?;

            // Phase 4: Commit (reserve in the cluster state cache)
            self.cluster.reserve(selected_node, app_id, resources);

            placements.push(Placement {
                node_id: selected_node.clone(),
                resources: *resources,
            });
        }

        Ok(SchedulingDecision {
            app_id: app_id.clone(),
            placements,
        })
    }

    /// Schedule daemon mode: one replica on every eligible node.
    fn schedule_daemon(
        &mut self,
        app_id: &AppId,
        resources: &Resources,
        required_labels: &BTreeMap<String, String>,
    ) -> Result<SchedulingDecision, ScheduleError> {
        let candidates = filter_nodes(resources, required_labels, &self.cluster);
        if candidates.is_empty() {
            return Err(ScheduleError::NoEligibleNodes {
                app_id: app_id.clone(),
            });
        }

        let mut placements = Vec::with_capacity(candidates.len());
        for node_id in &candidates {
            self.cluster.reserve(node_id, app_id, resources);
            placements.push(Placement {
                node_id: node_id.clone(),
                resources: *resources,
            });
        }

        Ok(SchedulingDecision {
            app_id: app_id.clone(),
            placements,
        })
    }

    /// Extract resource requirements from an AppSpec.
    ///
    /// Uses the `request` (minimum) values from cpu/memory ranges.
    /// Falls back to zero if not specified.
    fn extract_resources(&self, spec: &AppSpec) -> Resources {
        let cpu = spec.cpu.as_ref().map(|r| r.request).unwrap_or(0);
        let memory = spec.memory.as_ref().map(|r| r.request).unwrap_or(0);
        let gpus = spec.gpu.unwrap_or(0);
        Resources::new(cpu, memory, gpus)
    }

    /// Parse placement labels from the spec.
    ///
    /// Labels are stored as `["key=value", ...]` in the config.
    /// Returns (required, preferred) as BTreeMaps.
    fn parse_labels(&self, spec: &AppSpec) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
        let required = spec
            .placement
            .as_ref()
            .map(|p| parse_label_list(&p.required))
            .unwrap_or_default();
        let preferred = spec
            .placement
            .as_ref()
            .map(|p| parse_label_list(&p.preferred))
            .unwrap_or_default();
        (required, preferred)
    }
}

/// Parse a list of "key=value" strings into a BTreeMap.
fn parse_label_list(labels: &[String]) -> BTreeMap<String, String> {
    labels
        .iter()
        .filter_map(|s| {
            let (k, v) = s.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::config::app::PlacementSpec;
    use crate::config::types::ResourceRange;
    use crate::meat::cluster_state::SchedulerNodeState;
    use crate::meat::types::NodeId;

    fn default_spec() -> AppSpec {
        toml::from_str(r#"image = "test:v1""#).unwrap()
    }

    fn node_state(
        name: &str,
        cpu: u64,
        mem: u64,
        labels: BTreeMap<String, String>,
    ) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(name),
            allocatable: Resources::new(cpu, mem, 0),
            allocated: Resources::default(),
            labels,
            ready: true,
            running_apps: HashSet::new(),
            uptime_secs: 86400,
            cached_images: HashSet::new(),
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn cluster_3_nodes() -> ClusterStateCache {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("n1", 2000, 4096, BTreeMap::new()));
        cluster.set_node(node_state("n2", 2000, 4096, BTreeMap::new()));
        cluster.set_node(node_state("n3", 2000, 4096, BTreeMap::new()));
        cluster
    }

    #[test]
    fn schedule_fixed_replicas_places_all() {
        let mut scheduler = Scheduler::new(cluster_3_nodes());

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(3);
        spec.cpu = Some(ResourceRange {
            request: 500,
            limit: 1000,
        });
        spec.memory = Some(ResourceRange {
            request: 1024,
            limit: 2048,
        });

        let app = AppId::new("web", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        assert_eq!(decision.placements.len(), 3);
        // Resources are reserved correctly
        for p in &decision.placements {
            assert_eq!(p.resources.cpu_millicores, 500);
        }
    }

    #[test]
    fn schedule_spreads_when_resources_tight() {
        // When nodes are already near capacity, spread scoring forces
        // distribution because bin-packing can't differentiate further.
        let mut cluster = ClusterStateCache::new();
        for i in 0..3 {
            let mut n = node_state(&format!("n{i}"), 1000, 4096, BTreeMap::new());
            n.allocated = Resources::new(800, 0, 0); // 80% utilised
            cluster.set_node(n);
        }

        let mut scheduler = Scheduler::new(cluster);
        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(3);
        spec.cpu = Some(ResourceRange {
            request: 100,
            limit: 200,
        });

        let app = AppId::new("web", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        assert_eq!(decision.placements.len(), 3);
        let nodes: HashSet<_> = decision.placements.iter().map(|p| &p.node_id).collect();
        assert_eq!(nodes.len(), 3, "tight resources should spread across nodes");
    }

    #[test]
    fn schedule_daemon_mode_on_all_nodes() {
        let mut scheduler = Scheduler::new(cluster_3_nodes());

        let mut spec = default_spec();
        spec.replicas = Replicas::DaemonSet;
        spec.cpu = Some(ResourceRange {
            request: 100,
            limit: 200,
        });

        let app = AppId::new("monitor", "system");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        assert_eq!(decision.placements.len(), 3, "daemon on all 3 nodes");
    }

    #[test]
    fn schedule_with_required_labels() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("gpu-1", 2000, 4096, labels(&[("gpu", "a100")])));
        cluster.set_node(node_state("gpu-2", 2000, 4096, labels(&[("gpu", "a100")])));
        cluster.set_node(node_state("cpu-1", 2000, 4096, BTreeMap::new()));

        let mut scheduler = Scheduler::new(cluster);

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(2);
        spec.cpu = Some(ResourceRange {
            request: 100,
            limit: 200,
        });
        spec.placement = Some(PlacementSpec {
            required: vec!["gpu=a100".to_string()],
            preferred: vec![],
        });

        let app = AppId::new("ml", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        assert_eq!(decision.placements.len(), 2);
        for p in &decision.placements {
            assert!(
                p.node_id == NodeId::new("gpu-1") || p.node_id == NodeId::new("gpu-2"),
                "should only place on gpu nodes, got {:?}",
                p.node_id
            );
        }
    }

    #[test]
    fn schedule_with_preferred_labels_fallback() {
        let mut cluster = ClusterStateCache::new();
        // Only one node has the preferred label
        cluster.set_node(node_state(
            "east",
            2000,
            4096,
            labels(&[("zone", "us-east")]),
        ));
        cluster.set_node(node_state(
            "west",
            2000,
            4096,
            labels(&[("zone", "us-west")]),
        ));

        let mut scheduler = Scheduler::new(cluster);

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(2);
        spec.cpu = Some(ResourceRange {
            request: 100,
            limit: 200,
        });
        spec.placement = Some(PlacementSpec {
            required: vec![],
            preferred: vec!["zone=us-east".to_string()],
        });

        let app = AppId::new("web", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        // Both should be placed (preferred is soft, not hard)
        assert_eq!(decision.placements.len(), 2);

        // First placement should prefer "east"
        assert_eq!(decision.placements[0].node_id, NodeId::new("east"));
    }

    #[test]
    fn schedule_fails_no_eligible_nodes() {
        let mut cluster = ClusterStateCache::new();
        cluster.set_node(node_state("tiny", 100, 256, BTreeMap::new()));

        let mut scheduler = Scheduler::new(cluster);

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(1);
        spec.cpu = Some(ResourceRange {
            request: 9999,
            limit: 9999,
        });

        let app = AppId::new("huge", "prod");
        let result = scheduler.schedule_app(&app, &spec);
        assert!(matches!(result, Err(ScheduleError::NoEligibleNodes { .. })));
    }

    #[test]
    fn schedule_bin_packs_onto_fuller_node() {
        let mut cluster = ClusterStateCache::new();
        // "full" already has 800m allocated — will be preferred for packing
        let mut full = node_state("full", 2000, 4096, BTreeMap::new());
        full.allocated = Resources::new(800, 0, 0);
        cluster.set_node(full);
        cluster.set_node(node_state("empty", 2000, 4096, BTreeMap::new()));

        let mut scheduler = Scheduler::new(cluster);

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(1);
        spec.cpu = Some(ResourceRange {
            request: 200,
            limit: 400,
        });

        let app = AppId::new("web", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        assert_eq!(
            decision.placements[0].node_id,
            NodeId::new("full"),
            "should bin-pack onto the fuller node"
        );
    }

    #[test]
    fn schedule_gpu_workload() {
        let mut cluster = ClusterStateCache::new();
        let mut gpu_node = node_state("gpu-1", 2000, 4096, BTreeMap::new());
        gpu_node.allocatable = Resources::new(2000, 4096, 2); // 2 GPUs
        cluster.set_node(gpu_node);
        cluster.set_node(node_state("cpu-1", 2000, 4096, BTreeMap::new())); // 0 GPUs

        let mut scheduler = Scheduler::new(cluster);

        let mut spec = default_spec();
        spec.replicas = Replicas::Fixed(1);
        spec.gpu = Some(1);

        let app = AppId::new("ml", "prod");
        let decision = scheduler.schedule_app(&app, &spec).unwrap();

        assert_eq!(
            decision.placements[0].node_id,
            NodeId::new("gpu-1"),
            "should place on the GPU node"
        );
    }

    #[test]
    fn parse_label_list_works() {
        let labels = vec![
            "zone=us-east".to_string(),
            "ssd=true".to_string(),
            "malformed".to_string(), // no = sign, skipped
        ];
        let map = parse_label_list(&labels);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("zone").unwrap(), "us-east");
        assert_eq!(map.get("ssd").unwrap(), "true");
    }
}

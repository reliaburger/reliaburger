/// Shared cluster types used by Mustard, Raft, and Patty.
///
/// These newtypes prevent accidentally mixing up identifiers
/// (a `NodeId` is not an `AppId` is not an `InstanceId`) and
/// give the scheduler a concrete vocabulary for resource tracking
/// and placement decisions.
use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// NodeId
// ---------------------------------------------------------------------------

/// Unique identifier for a node in the cluster.
///
/// Typically the hostname or a user-configured name. Distinct from
/// `InstanceId`, which identifies a container instance *within* a node.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl NodeId {
    /// Create a new `NodeId` from a string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

// ---------------------------------------------------------------------------
// AppId
// ---------------------------------------------------------------------------

/// Unique identifier for an application within a namespace.
///
/// Combines the app name and namespace so the scheduler can distinguish
/// between apps with the same name in different namespaces.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct AppId {
    pub name: String,
    pub namespace: String,
}

impl fmt::Display for AppId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.namespace, self.name)
    }
}

impl AppId {
    /// Create a new `AppId`.
    pub fn new(name: impl Into<String>, namespace: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            namespace: namespace.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

/// Resource quantities in base units.
///
/// CPU is in millicores (1000m = 1 core), memory in bytes, GPUs as
/// whole device count. All arithmetic is saturating to prevent overflow
/// panics in scheduling calculations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Resources {
    /// CPU in millicores (e.g. 500 = 0.5 cores).
    pub cpu_millicores: u64,
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// Number of GPUs.
    pub gpus: u32,
}

impl Resources {
    /// Create a new `Resources` with the given values.
    pub fn new(cpu_millicores: u64, memory_bytes: u64, gpus: u32) -> Self {
        Self {
            cpu_millicores,
            memory_bytes,
            gpus,
        }
    }

    /// Returns `true` if `self` can satisfy `required`.
    ///
    /// Every field of `self` must be >= the corresponding field in `required`.
    pub fn fits(&self, required: &Resources) -> bool {
        self.cpu_millicores >= required.cpu_millicores
            && self.memory_bytes >= required.memory_bytes
            && self.gpus >= required.gpus
    }

    /// Subtract `other` from `self`, saturating at zero.
    ///
    /// Used when allocating resources: the remaining capacity is
    /// `total.saturating_sub(allocated)`.
    pub fn saturating_sub(&self, other: &Resources) -> Resources {
        Resources {
            cpu_millicores: self.cpu_millicores.saturating_sub(other.cpu_millicores),
            memory_bytes: self.memory_bytes.saturating_sub(other.memory_bytes),
            gpus: self.gpus.saturating_sub(other.gpus),
        }
    }

    /// Add `other` to `self`, saturating at the maximum value.
    pub fn saturating_add(&self, other: &Resources) -> Resources {
        Resources {
            cpu_millicores: self.cpu_millicores.saturating_add(other.cpu_millicores),
            memory_bytes: self.memory_bytes.saturating_add(other.memory_bytes),
            gpus: self.gpus.saturating_add(other.gpus),
        }
    }

    /// Returns `true` if all fields are zero.
    pub fn is_zero(&self) -> bool {
        self.cpu_millicores == 0 && self.memory_bytes == 0 && self.gpus == 0
    }
}

impl fmt::Display for Resources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cpu={}m mem={}Mi gpus={}",
            self.cpu_millicores,
            self.memory_bytes / (1024 * 1024),
            self.gpus
        )
    }
}

// ---------------------------------------------------------------------------
// NodeCapacity
// ---------------------------------------------------------------------------

/// A node's total and reserved resources, plus its labels.
///
/// The scheduler uses this to compute allocatable capacity and
/// evaluate placement constraints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapacity {
    /// Node identity.
    pub node_id: NodeId,
    /// Address for cluster communication.
    pub address: SocketAddr,
    /// Total hardware resources on this node.
    pub total: Resources,
    /// Resources reserved for the OS and Bun agent.
    pub reserved: Resources,
    /// Resources currently allocated to running workloads.
    pub allocated: Resources,
    /// Node labels for placement constraints.
    pub labels: BTreeMap<String, String>,
}

impl NodeCapacity {
    /// Resources available for scheduling new workloads.
    ///
    /// `allocatable = total - reserved - allocated`
    pub fn allocatable(&self) -> Resources {
        self.total
            .saturating_sub(&self.reserved)
            .saturating_sub(&self.allocated)
    }

    /// Returns `true` if this node can accommodate the given resource request.
    pub fn can_fit(&self, required: &Resources) -> bool {
        self.allocatable().fits(required)
    }
}

// ---------------------------------------------------------------------------
// SchedulingDecision
// ---------------------------------------------------------------------------

/// A placement decision made by the Patty scheduler.
///
/// Records where each replica of an app should run, so the
/// decision can be committed to the Raft log and replicated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulingDecision {
    /// The app being scheduled.
    pub app_id: AppId,
    /// Ordered list of node assignments (one per replica).
    pub placements: Vec<Placement>,
}

/// A single replica placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    /// Which node this replica is assigned to.
    pub node_id: NodeId,
    /// Resources reserved for this replica.
    pub resources: Resources,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- NodeId ---------------------------------------------------------------

    #[test]
    fn node_id_equality() {
        let a = NodeId::new("node-1");
        let b = NodeId::new("node-1");
        let c = NodeId::new("node-2");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn node_id_hashing() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(NodeId::new("node-1"));
        set.insert(NodeId::new("node-1"));
        set.insert(NodeId::new("node-2"));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn node_id_display() {
        let id = NodeId::new("worker-42");
        assert_eq!(id.to_string(), "worker-42");
    }

    #[test]
    fn node_id_ordering() {
        let a = NodeId::new("alpha");
        let b = NodeId::new("beta");
        assert!(a < b);
    }

    #[test]
    fn node_id_serialisation_round_trip() {
        let id = NodeId::new("node-1");
        let json = serde_json::to_string(&id).unwrap();
        let decoded: NodeId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, decoded);
    }

    // -- AppId ----------------------------------------------------------------

    #[test]
    fn app_id_equality() {
        let a = AppId::new("web", "production");
        let b = AppId::new("web", "production");
        let c = AppId::new("web", "staging");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn app_id_display() {
        let id = AppId::new("redis", "default");
        assert_eq!(id.to_string(), "default/redis");
    }

    #[test]
    fn app_id_same_name_different_namespace() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(AppId::new("web", "prod"));
        set.insert(AppId::new("web", "staging"));
        assert_eq!(set.len(), 2);
    }

    // -- Resources ------------------------------------------------------------

    #[test]
    fn resources_fits_when_sufficient() {
        let available = Resources::new(4000, 8 * 1024 * 1024 * 1024, 2);
        let required = Resources::new(2000, 4 * 1024 * 1024 * 1024, 1);
        assert!(available.fits(&required));
    }

    #[test]
    fn resources_does_not_fit_when_cpu_insufficient() {
        let available = Resources::new(1000, 8 * 1024 * 1024 * 1024, 2);
        let required = Resources::new(2000, 4 * 1024 * 1024 * 1024, 1);
        assert!(!available.fits(&required));
    }

    #[test]
    fn resources_does_not_fit_when_memory_insufficient() {
        let available = Resources::new(4000, 2 * 1024 * 1024 * 1024, 2);
        let required = Resources::new(2000, 4 * 1024 * 1024 * 1024, 1);
        assert!(!available.fits(&required));
    }

    #[test]
    fn resources_does_not_fit_when_gpu_insufficient() {
        let available = Resources::new(4000, 8 * 1024 * 1024 * 1024, 0);
        let required = Resources::new(2000, 4 * 1024 * 1024 * 1024, 1);
        assert!(!available.fits(&required));
    }

    #[test]
    fn resources_fits_exact_match() {
        let r = Resources::new(1000, 512 * 1024 * 1024, 1);
        assert!(r.fits(&r));
    }

    #[test]
    fn resources_saturating_sub() {
        let total = Resources::new(4000, 8 * 1024 * 1024 * 1024, 2);
        let used = Resources::new(3000, 6 * 1024 * 1024 * 1024, 1);
        let remaining = total.saturating_sub(&used);
        assert_eq!(remaining.cpu_millicores, 1000);
        assert_eq!(remaining.memory_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(remaining.gpus, 1);
    }

    #[test]
    fn resources_saturating_sub_clamps_at_zero() {
        let small = Resources::new(1000, 1024, 0);
        let large = Resources::new(2000, 2048, 1);
        let result = small.saturating_sub(&large);
        assert_eq!(result.cpu_millicores, 0);
        assert_eq!(result.memory_bytes, 0);
        assert_eq!(result.gpus, 0);
    }

    #[test]
    fn resources_saturating_add() {
        let a = Resources::new(1000, 512 * 1024 * 1024, 1);
        let b = Resources::new(2000, 256 * 1024 * 1024, 1);
        let sum = a.saturating_add(&b);
        assert_eq!(sum.cpu_millicores, 3000);
        assert_eq!(sum.memory_bytes, 768 * 1024 * 1024);
        assert_eq!(sum.gpus, 2);
    }

    #[test]
    fn resources_is_zero() {
        assert!(Resources::default().is_zero());
        assert!(!Resources::new(1, 0, 0).is_zero());
    }

    #[test]
    fn resources_display() {
        let r = Resources::new(2000, 4 * 1024 * 1024 * 1024, 1);
        assert_eq!(r.to_string(), "cpu=2000m mem=4096Mi gpus=1");
    }

    #[test]
    fn resources_serialisation_round_trip() {
        let r = Resources::new(500, 1024 * 1024, 0);
        let json = serde_json::to_string(&r).unwrap();
        let decoded: Resources = serde_json::from_str(&json).unwrap();
        assert_eq!(r, decoded);
    }

    // -- NodeCapacity ---------------------------------------------------------

    #[test]
    fn node_capacity_allocatable() {
        let cap = NodeCapacity {
            node_id: NodeId::new("node-1"),
            address: "127.0.0.1:9444".parse().unwrap(),
            total: Resources::new(8000, 16 * 1024 * 1024 * 1024, 4),
            reserved: Resources::new(500, 512 * 1024 * 1024, 0),
            allocated: Resources::new(3000, 8 * 1024 * 1024 * 1024, 2),
            labels: BTreeMap::new(),
        };
        let alloc = cap.allocatable();
        assert_eq!(alloc.cpu_millicores, 4500);
        assert_eq!(
            alloc.memory_bytes,
            (16 * 1024 - 512 - 8 * 1024) * 1024 * 1024
        );
        assert_eq!(alloc.gpus, 2);
    }

    #[test]
    fn node_capacity_allocatable_saturates() {
        // allocated exceeds total-reserved: allocatable should be 0, not underflow
        let cap = NodeCapacity {
            node_id: NodeId::new("overloaded"),
            address: "127.0.0.1:9444".parse().unwrap(),
            total: Resources::new(4000, 4 * 1024 * 1024 * 1024, 1),
            reserved: Resources::new(500, 512 * 1024 * 1024, 0),
            allocated: Resources::new(4000, 4 * 1024 * 1024 * 1024, 2),
            labels: BTreeMap::new(),
        };
        let alloc = cap.allocatable();
        assert_eq!(alloc.cpu_millicores, 0);
        assert_eq!(alloc.memory_bytes, 0);
        assert_eq!(alloc.gpus, 0);
    }

    #[test]
    fn node_capacity_can_fit() {
        let cap = NodeCapacity {
            node_id: NodeId::new("node-1"),
            address: "127.0.0.1:9444".parse().unwrap(),
            total: Resources::new(8000, 16 * 1024 * 1024 * 1024, 2),
            reserved: Resources::new(500, 512 * 1024 * 1024, 0),
            allocated: Resources::new(2000, 4 * 1024 * 1024 * 1024, 0),
            labels: BTreeMap::new(),
        };
        let small = Resources::new(1000, 1024 * 1024 * 1024, 1);
        let large = Resources::new(8000, 16 * 1024 * 1024 * 1024, 4);
        assert!(cap.can_fit(&small));
        assert!(!cap.can_fit(&large));
    }

    // -- SchedulingDecision ---------------------------------------------------

    #[test]
    fn scheduling_decision_serialisation_round_trip() {
        let decision = SchedulingDecision {
            app_id: AppId::new("web", "production"),
            placements: vec![
                Placement {
                    node_id: NodeId::new("node-1"),
                    resources: Resources::new(500, 256 * 1024 * 1024, 0),
                },
                Placement {
                    node_id: NodeId::new("node-2"),
                    resources: Resources::new(500, 256 * 1024 * 1024, 0),
                },
            ],
        };
        let json = serde_json::to_string(&decision).unwrap();
        let decoded: SchedulingDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(decision, decoded);
    }

    #[test]
    fn scheduling_decision_distinct_nodes() {
        use std::collections::HashSet;
        let decision = SchedulingDecision {
            app_id: AppId::new("web", "default"),
            placements: vec![
                Placement {
                    node_id: NodeId::new("node-1"),
                    resources: Resources::default(),
                },
                Placement {
                    node_id: NodeId::new("node-2"),
                    resources: Resources::default(),
                },
                Placement {
                    node_id: NodeId::new("node-3"),
                    resources: Resources::default(),
                },
            ],
        };
        let unique_nodes: HashSet<&NodeId> =
            decision.placements.iter().map(|p| &p.node_id).collect();
        assert_eq!(unique_nodes.len(), 3);
    }
}

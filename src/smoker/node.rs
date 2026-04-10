//! Node-level fault simulation.
//!
//! `NodeDrain` simulates a graceful departure — the node stops
//! accepting work and evicts running containers. `NodeKill` simulates
//! an abrupt crash — gossip and reporting stop immediately.
//!
//! Both faults operate through the agent's existing subsystems rather
//! than directly manipulating the network, so they exercise the same
//! code paths as real failures.

/// Instructions for the agent to execute a node drain.
///
/// The agent handles the actual implementation: marking itself as
/// draining, stopping scheduling, initiating graceful container
/// eviction, and restoring after the duration expires.
#[derive(Debug, Clone)]
pub struct DrainPlan {
    /// How long to keep the node drained before recovery.
    pub duration_secs: u64,
    /// Whether to wait for containers to finish gracefully.
    pub graceful: bool,
}

impl DrainPlan {
    pub fn new(duration_secs: u64) -> Self {
        Self {
            duration_secs,
            graceful: true,
        }
    }
}

/// Instructions for the agent to execute a node kill.
///
/// The agent pauses gossip heartbeats, disconnects from the reporting
/// tree, and stops responding to health checks. From the cluster's
/// perspective, the node has vanished. After the duration, the agent
/// resumes normal operation and rejoins via gossip.
#[derive(Debug, Clone)]
pub struct KillPlan {
    /// How long to keep the node "dead" before recovery.
    pub duration_secs: u64,
    /// Whether to also stop all containers on the node.
    pub kill_containers: bool,
}

impl KillPlan {
    pub fn new(duration_secs: u64, kill_containers: bool) -> Self {
        Self {
            duration_secs,
            kill_containers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_plan_defaults_to_graceful() {
        let plan = DrainPlan::new(30);
        assert_eq!(plan.duration_secs, 30);
        assert!(plan.graceful);
    }

    #[test]
    fn kill_plan_stores_parameters() {
        let plan = KillPlan::new(60, true);
        assert_eq!(plan.duration_secs, 60);
        assert!(plan.kill_containers);

        let plan = KillPlan::new(10, false);
        assert!(!plan.kill_containers);
    }
}

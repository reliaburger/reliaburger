//! High-level council node wrapper.
//!
//! Owns an `openraft::Raft<TypeConfig>` instance and provides a clean
//! API for the rest of Reliaburger to write desired state, query
//! leadership, and manage council membership.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use openraft::error::ClientWriteError;
use openraft::{ChangeMembers, Raft, RaftNetworkFactory};
use tokio::sync::watch;

use super::CouncilError;
use super::log_store::MemLogStore;
use super::state_machine::CouncilStateMachine;
use super::types::{
    CouncilConfig, CouncilNodeInfo, CouncilResponse, DesiredState, RaftRequest, TypeConfig,
};

// ---------------------------------------------------------------------------
// CouncilNode
// ---------------------------------------------------------------------------

/// High-level wrapper around an openraft Raft instance.
///
/// Provides a clean API for writing desired state, querying
/// leadership, and managing council membership.
pub struct CouncilNode {
    raft: Raft<TypeConfig>,
    #[allow(dead_code)]
    raft_id: u64,
    state_machine: CouncilStateMachine,
    /// Master secret for unwrapping CA private keys (in-memory only).
    wrapping_ikm: Option<[u8; 32]>,
}

impl CouncilNode {
    /// Create a new council node.
    ///
    /// This starts the Raft engine but does not initialise the
    /// cluster. Call `initialize()` on the first node, then use
    /// `add_learner()` + `change_membership()` from the leader to
    /// grow the council.
    pub async fn new<N: RaftNetworkFactory<TypeConfig>>(
        raft_id: u64,
        config: CouncilConfig,
        network: N,
        log_store: MemLogStore,
        state_machine: CouncilStateMachine,
        wrapping_ikm: Option<[u8; 32]>,
    ) -> Result<Self, CouncilError> {
        let raft_config = Arc::new(
            config
                .to_openraft_config()
                .validate()
                .map_err(|e| CouncilError::InitError(e.to_string()))?,
        );

        let raft = Raft::new(
            raft_id,
            raft_config,
            network,
            log_store,
            state_machine.clone(),
        )
        .await
        .map_err(|e| CouncilError::Fatal(e.to_string()))?;

        Ok(Self {
            raft,
            raft_id,
            state_machine,
            wrapping_ikm,
        })
    }

    /// Initialise the cluster with an initial set of members.
    ///
    /// Call this once on the very first node (with itself as the only
    /// member). It becomes leader immediately (quorum of 1 = itself).
    pub async fn initialize(
        &self,
        members: BTreeMap<u64, CouncilNodeInfo>,
    ) -> Result<(), CouncilError> {
        self.raft
            .initialize(members)
            .await
            .map_err(|e| CouncilError::InitError(e.to_string()))?;
        Ok(())
    }

    /// Write a request to the Raft log.
    ///
    /// Returns `ForwardToLeader` if this node is not the leader.
    pub async fn write(&self, request: RaftRequest) -> Result<CouncilResponse, CouncilError> {
        let result = self.raft.client_write(request).await;
        match result {
            Ok(resp) => Ok(resp.data),
            Err(e) => match e {
                openraft::error::RaftError::APIError(ClientWriteError::ForwardToLeader(fwd)) => {
                    Err(CouncilError::ForwardToLeader {
                        leader: fwd.leader_id,
                    })
                }
                other => Err(CouncilError::WriteFailed(other.to_string())),
            },
        }
    }

    /// Return the current leader's node ID, if known.
    pub async fn current_leader(&self) -> Option<u64> {
        self.raft.current_leader().await
    }

    /// Return `true` if this node is the current leader.
    pub async fn is_leader(&self) -> bool {
        self.raft.ensure_linearizable().await.is_ok()
    }

    /// Subscribe to Raft metrics changes.
    pub fn metrics(&self) -> watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>> {
        self.raft.metrics()
    }

    /// Read the current desired state from the state machine.
    pub async fn desired_state(&self) -> DesiredState {
        self.state_machine.desired_state().await
    }

    /// Add a learner node to the cluster.
    ///
    /// The learner receives log replication but does not vote.
    /// Use `change_membership()` to promote learners to voters.
    pub async fn add_learner(&self, id: u64, info: CouncilNodeInfo) -> Result<(), CouncilError> {
        self.raft
            .add_learner(id, info, true)
            .await
            .map_err(|e| CouncilError::WriteFailed(e.to_string()))?;
        Ok(())
    }

    /// Change the voter set of the cluster.
    ///
    /// All nodes in `members` must already be learners (added via
    /// `add_learner()`). Nodes not in `members` are demoted to
    /// learners (retained, not removed).
    pub async fn change_membership(&self, members: BTreeSet<u64>) -> Result<(), CouncilError> {
        self.raft
            .change_membership(ChangeMembers::ReplaceAllVoters(members), true)
            .await
            .map_err(|e| CouncilError::WriteFailed(e.to_string()))?;
        Ok(())
    }

    /// Shut down this Raft node.
    pub async fn shutdown(&self) -> Result<(), CouncilError> {
        self.raft
            .shutdown()
            .await
            .map_err(|e| CouncilError::Fatal(e.to_string()))?;
        Ok(())
    }

    /// Access the underlying openraft handle (for tests).
    pub fn raft(&self) -> &Raft<TypeConfig> {
        &self.raft
    }

    /// Returns the wrapping IKM for unwrapping CA private keys.
    pub fn wrapping_ikm(&self) -> Option<&[u8; 32]> {
        self.wrapping_ikm.as_ref()
    }

    /// Read the current security state from the state machine.
    pub async fn security_state(&self) -> crate::sesame::types::SecurityState {
        self.state_machine.desired_state().await.security_state
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::config::app::AppSpec;
    use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
    use crate::meat::types::{AppId, NodeId, Placement, Resources, SchedulingDecision};

    use super::*;

    fn default_spec() -> AppSpec {
        toml::from_str(r#"image = "test:v1""#).unwrap()
    }

    fn node_info(id: u64) -> CouncilNodeInfo {
        CouncilNodeInfo::new(
            format!("127.0.0.1:{}", 9000 + id).parse().unwrap(),
            format!("node-{}", id),
        )
    }

    fn fast_config() -> CouncilConfig {
        CouncilConfig {
            heartbeat_interval_ms: 50,
            election_timeout_min_ms: 200,
            election_timeout_max_ms: 400,
            snapshot_threshold: 100,
            max_in_snapshot_log_to_keep: 50,
        }
    }

    /// Helper: spin up N council nodes connected via an in-memory router.
    async fn create_cluster(n: u64) -> (Vec<CouncilNode>, InMemoryRaftRouter) {
        let router = InMemoryRaftRouter::new();
        let mut nodes = Vec::new();

        for id in 1..=n {
            let network = InMemoryRaftNetworkFactory::new(id, router.clone());
            let log_store = MemLogStore::new();
            let sm = CouncilStateMachine::new();
            let node = CouncilNode::new(id, fast_config(), network, log_store, sm, None)
                .await
                .unwrap();
            router.register(id, node.raft().clone()).await;
            nodes.push(node);
        }

        (nodes, router)
    }

    /// Helper: initialise a cluster and wait for a leader.
    async fn init_cluster(nodes: &[CouncilNode]) {
        let mut members = BTreeMap::new();
        for (i, _) in nodes.iter().enumerate() {
            let id = (i + 1) as u64;
            members.insert(id, node_info(id));
        }
        // Initialise on node 1.
        nodes[0].initialize(members).await.unwrap();
    }

    /// Wait for a leader to be elected (up to timeout).
    async fn wait_for_leader(nodes: &[CouncilNode], timeout: Duration) -> Option<u64> {
        let start = tokio::time::Instant::now();
        loop {
            for node in nodes {
                if let Some(leader) = node.current_leader().await {
                    return Some(leader);
                }
            }
            if start.elapsed() > timeout {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait for a leader from a set of node references, optionally
    /// excluding a specific node ID (e.g., a shut-down leader).
    async fn wait_for_leader_refs(
        nodes: &[&CouncilNode],
        timeout: Duration,
        exclude: Option<u64>,
    ) -> Option<u64> {
        let start = tokio::time::Instant::now();
        loop {
            for node in nodes {
                if let Some(leader) = node.current_leader().await {
                    if exclude.is_none_or(|ex| leader != ex) {
                        return Some(leader);
                    }
                }
            }
            if start.elapsed() > timeout {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Find the leader node from the cluster.
    async fn find_leader(nodes: &[CouncilNode]) -> &CouncilNode {
        let leader_id = wait_for_leader(nodes, Duration::from_secs(5))
            .await
            .expect("no leader elected within timeout");
        &nodes[(leader_id - 1) as usize]
    }

    // -----------------------------------------------------------------------
    // Bootstrap tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn single_node_becomes_leader() {
        let (nodes, _router) = create_cluster(1).await;
        init_cluster(&nodes).await;

        let leader = wait_for_leader(&nodes, Duration::from_secs(5)).await;
        assert_eq!(leader, Some(1));

        // Can write immediately.
        let resp = nodes[0].write(RaftRequest::Noop).await.unwrap();
        assert!(matches!(resp, CouncilResponse::Applied { .. }));
    }

    #[tokio::test]
    async fn single_node_grows_to_three() {
        let (mut nodes, router) = create_cluster(1).await;
        init_cluster(&nodes[..1]).await;
        wait_for_leader(&nodes[..1], Duration::from_secs(5)).await;

        // Write an entry before growth.
        nodes[0]
            .write(RaftRequest::AppSpec {
                app_id: AppId::new("web", "prod"),
                spec: Box::new(default_spec()),
            })
            .await
            .unwrap();

        // Create and add nodes 2 and 3.
        for id in 2..=3u64 {
            let network = InMemoryRaftNetworkFactory::new(id, router.clone());
            let node = CouncilNode::new(
                id,
                fast_config(),
                network,
                MemLogStore::new(),
                CouncilStateMachine::new(),
                None,
            )
            .await
            .unwrap();
            router.register(id, node.raft().clone()).await;
            nodes.push(node);
        }

        // Add as learners, then change membership.
        let leader = &nodes[0];
        leader.add_learner(2, node_info(2)).await.unwrap();
        leader.add_learner(3, node_info(3)).await.unwrap();
        leader
            .change_membership(BTreeSet::from([1, 2, 3]))
            .await
            .unwrap();

        // Wait for replication.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // All three nodes should have the pre-growth entry.
        for node in &nodes {
            let state = node.desired_state().await;
            assert!(
                state.apps.contains_key(&AppId::new("web", "prod")),
                "node missing pre-growth entry"
            );
        }
    }

    #[tokio::test]
    async fn single_node_write_before_growth() {
        let (mut nodes, router) = create_cluster(1).await;
        init_cluster(&nodes[..1]).await;
        wait_for_leader(&nodes[..1], Duration::from_secs(5)).await;

        // Write config before any other node exists.
        nodes[0]
            .write(RaftRequest::ConfigSet {
                key: "region".to_string(),
                value: "us-east".to_string(),
            })
            .await
            .unwrap();

        // Grow to 3.
        for id in 2..=3u64 {
            let network = InMemoryRaftNetworkFactory::new(id, router.clone());
            let node = CouncilNode::new(
                id,
                fast_config(),
                network,
                MemLogStore::new(),
                CouncilStateMachine::new(),
                None,
            )
            .await
            .unwrap();
            router.register(id, node.raft().clone()).await;
            nodes.push(node);
        }
        nodes[0].add_learner(2, node_info(2)).await.unwrap();
        nodes[0].add_learner(3, node_info(3)).await.unwrap();
        nodes[0]
            .change_membership(BTreeSet::from([1, 2, 3]))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        for node in &nodes {
            let state = node.desired_state().await;
            assert_eq!(
                state.config.get("region").map(String::as_str),
                Some("us-east")
            );
        }
    }

    // -----------------------------------------------------------------------
    // is_leader and metrics tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn is_leader_returns_true_for_leader() {
        let (nodes, _router) = create_cluster(3).await;
        init_cluster(&nodes).await;

        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        let leader = &nodes[(leader_id - 1) as usize];
        assert!(leader.is_leader().await);

        // A follower should not be leader.
        let follower_idx = if leader_id == 1 { 1 } else { 0 };
        assert!(!nodes[follower_idx].is_leader().await);
    }

    #[tokio::test]
    async fn metrics_reflects_leader_state() {
        let (nodes, _router) = create_cluster(3).await;
        init_cluster(&nodes).await;

        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        let leader = &nodes[(leader_id - 1) as usize];

        let metrics = leader.metrics().borrow().clone();
        assert_eq!(metrics.current_leader, Some(leader_id));
        assert_eq!(metrics.id, leader_id);
    }

    // -----------------------------------------------------------------------
    // Leader election tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn leader_election_three_node_cluster() {
        let (nodes, _router) = create_cluster(3).await;
        init_cluster(&nodes).await;

        let leader = wait_for_leader(&nodes, Duration::from_secs(5)).await;
        assert!(leader.is_some(), "expected a leader within 5s");
    }

    #[tokio::test]
    async fn leader_election_five_node_cluster() {
        let (nodes, _router) = create_cluster(5).await;
        init_cluster(&nodes).await;

        let leader = wait_for_leader(&nodes, Duration::from_secs(5)).await;
        assert!(leader.is_some(), "expected a leader within 5s");
    }

    // -----------------------------------------------------------------------
    // Replication tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn log_replication_app_spec() {
        let (nodes, _router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader = find_leader(&nodes).await;

        let app_id = AppId::new("api", "staging");
        leader
            .write(RaftRequest::AppSpec {
                app_id: app_id.clone(),
                spec: Box::new(default_spec()),
            })
            .await
            .unwrap();

        // Wait for replication.
        tokio::time::sleep(Duration::from_millis(500)).await;

        for node in &nodes {
            let state = node.desired_state().await;
            assert!(
                state.apps.contains_key(&app_id),
                "app not replicated to all nodes"
            );
        }
    }

    #[tokio::test]
    async fn log_replication_scheduling_decision() {
        let (nodes, _router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader = find_leader(&nodes).await;

        let app_id = AppId::new("web", "prod");
        let decision = SchedulingDecision {
            app_id: app_id.clone(),
            placements: vec![Placement {
                node_id: NodeId::new("node-1"),
                resources: Resources::new(500, 256 * 1024 * 1024, 0),
            }],
        };
        leader
            .write(RaftRequest::SchedulingDecision(decision))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(500)).await;

        for node in &nodes {
            let state = node.desired_state().await;
            assert!(state.scheduling.contains_key(&app_id));
        }
    }

    // -----------------------------------------------------------------------
    // Failover tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn leader_failover() {
        let (nodes, _router) = create_cluster(5).await;
        init_cluster(&nodes).await;

        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();

        // Shut down the leader.
        nodes[(leader_id - 1) as usize].shutdown().await.unwrap();

        // Wait for a new leader, excluding the shut-down node.
        let remaining: Vec<&CouncilNode> = nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| (*i as u64 + 1) != leader_id)
            .map(|(_, n)| n)
            .collect();

        let new_leader =
            wait_for_leader_refs(&remaining, Duration::from_secs(5), Some(leader_id)).await;
        assert!(new_leader.is_some(), "no new leader after failover");
        assert_ne!(new_leader.unwrap(), leader_id);
    }

    #[tokio::test]
    async fn leader_failover_preserves_state() {
        let (nodes, _router) = create_cluster(5).await;
        init_cluster(&nodes).await;

        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        let leader = &nodes[(leader_id - 1) as usize];

        // Write entries.
        leader
            .write(RaftRequest::AppSpec {
                app_id: AppId::new("web", "prod"),
                spec: Box::new(default_spec()),
            })
            .await
            .unwrap();
        leader
            .write(RaftRequest::ConfigSet {
                key: "env".to_string(),
                value: "production".to_string(),
            })
            .await
            .unwrap();

        // Wait for replication.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Kill the leader.
        leader.shutdown().await.unwrap();

        // Wait for a new leader.
        let remaining: Vec<&CouncilNode> = nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| (*i as u64 + 1) != leader_id)
            .map(|(_, n)| n)
            .collect();

        let new_leader_id =
            wait_for_leader_refs(&remaining, Duration::from_secs(5), Some(leader_id))
                .await
                .expect("no new leader");
        let new_leader = &nodes[(new_leader_id - 1) as usize];

        let state = new_leader.desired_state().await;
        assert!(state.apps.contains_key(&AppId::new("web", "prod")));
        assert_eq!(
            state.config.get("env").map(String::as_str),
            Some("production")
        );
    }

    #[tokio::test]
    async fn write_on_follower_returns_forward_error() {
        let (nodes, _router) = create_cluster(3).await;
        init_cluster(&nodes).await;

        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();

        // Pick a follower.
        let follower_idx = if leader_id == 1 { 1 } else { 0 };
        let follower = &nodes[follower_idx];

        let result = follower.write(RaftRequest::Noop).await;
        assert!(
            matches!(result, Err(CouncilError::ForwardToLeader { .. })),
            "expected ForwardToLeader, got {:?}",
            result
        );
    }

    // -----------------------------------------------------------------------
    // Partition test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn partition_majority_continues_minority_cannot_write() {
        let (nodes, router) = create_cluster(5).await;
        init_cluster(&nodes).await;

        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();

        // Write before partition.
        let leader = &nodes[(leader_id - 1) as usize];
        leader
            .write(RaftRequest::ConfigSet {
                key: "before".to_string(),
                value: "partition".to_string(),
            })
            .await
            .unwrap();

        // Wait for replication.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Partition: isolate nodes 4 and 5 from nodes 1, 2, 3.
        // Also partition minority nodes from each other for cleaner test.
        let minority = [4u64, 5];
        let majority = [1u64, 2, 3];
        for &m in &minority {
            for &j in &majority {
                router.partition(m, j).await;
            }
        }

        // Wait for the majority to stabilise and elect a leader.
        // The majority leader must be one of {1, 2, 3}.
        let majority_nodes: Vec<&CouncilNode> = majority
            .iter()
            .map(|&id| &nodes[(id - 1) as usize])
            .collect();

        // If the old leader was in the minority, the majority needs
        // to elect a new one; exclude minority IDs.
        let majority_leader = wait_for_leader_refs(&majority_nodes, Duration::from_secs(10), None)
            .await
            .expect("majority should elect a leader");
        assert!(
            majority.contains(&majority_leader),
            "leader should be in the majority partition"
        );

        let ml = &nodes[(majority_leader - 1) as usize];
        ml.write(RaftRequest::ConfigSet {
            key: "after".to_string(),
            value: "partition".to_string(),
        })
        .await
        .unwrap();

        // The minority should not have the majority's post-partition write.
        let minority_state = nodes[3].desired_state().await;
        assert!(
            !minority_state.config.contains_key("after"),
            "minority should not have post-partition writes"
        );

        // Heal and verify convergence.
        router.heal().await;
        // Give enough time for the minority to catch up.
        tokio::time::sleep(Duration::from_millis(2000)).await;

        for (i, node) in nodes.iter().enumerate() {
            let state = node.desired_state().await;
            assert_eq!(
                state.config.get("before").map(String::as_str),
                Some("partition"),
                "node {} missing 'before' key after heal",
                i + 1
            );
            assert_eq!(
                state.config.get("after").map(String::as_str),
                Some("partition"),
                "node {} missing 'after' key after heal",
                i + 1
            );
        }
    }
}

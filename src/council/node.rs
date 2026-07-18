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
#[cfg(test)]
use super::log_store::MemLogStore;
use super::state_machine::CouncilStateMachine;
use super::types::{
    CouncilConfig, CouncilNodeInfo, CouncilResponse, DesiredState, RaftRequest, TypeConfig,
};
use crate::sesame::types::{CaRole, SerialNumber, SpiffeUri};

/// Result of signing a workload CSR.
#[derive(Debug, Clone)]
pub struct CsrSignResult {
    /// DER-encoded signed workload certificate.
    pub cert_der: Vec<u8>,
    /// DER-encoded Workload CA certificate.
    pub workload_ca_cert_der: Vec<u8>,
    /// DER-encoded Root CA certificate.
    pub root_ca_cert_der: Vec<u8>,
    /// OIDC JWT token (if OIDC config is present).
    pub jwt_token: Option<String>,
    /// Allocated serial number.
    pub serial: SerialNumber,
}

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
    pub async fn new<N, LS>(
        raft_id: u64,
        config: CouncilConfig,
        network: N,
        log_store: LS,
        state_machine: CouncilStateMachine,
        wrapping_ikm: Option<[u8; 32]>,
    ) -> Result<Self, CouncilError>
    where
        N: RaftNetworkFactory<TypeConfig>,
        LS: openraft::storage::RaftLogStorage<TypeConfig>,
    {
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

    /// Change the voter set, dropping demoted voters from the membership
    /// entirely rather than retaining them as learners.
    ///
    /// The self-healing reconciler uses this to evict dead voters: the
    /// joint-consensus change commits with a majority of both the old and
    /// new configurations, so it never waits on the evicted node's vote.
    pub async fn change_membership_evicting(
        &self,
        members: BTreeSet<u64>,
    ) -> Result<(), CouncilError> {
        self.raft
            .change_membership(ChangeMembers::ReplaceAllVoters(members), false)
            .await
            .map_err(|e| CouncilError::WriteFailed(e.to_string()))?;
        Ok(())
    }

    /// Remove a learner from the membership entirely. The node must not be
    /// a voter; demote it first via `change_membership`.
    pub async fn remove_learner(&self, id: u64) -> Result<(), CouncilError> {
        self.raft
            .change_membership(ChangeMembers::RemoveNodes(BTreeSet::from([id])), false)
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

    /// Read the current Pickle manifest catalogue from the state machine.
    pub async fn manifest_catalog(&self) -> crate::pickle::types::ManifestCatalog {
        self.state_machine.desired_state().await.manifest_catalog
    }

    /// Sign a workload CSR with the Workload CA.
    ///
    /// This is a synchronous crypto operation — only `AllocateSerial`
    /// goes through Raft. The council unwraps the CA key in memory,
    /// validates the CSR, signs it, and optionally mints an OIDC JWT.
    pub async fn sign_workload_csr(
        &self,
        csr_der: &[u8],
        spiffe_uri: &SpiffeUri,
        usage: crate::sesame::identity::CertUsage,
        cluster_name: &str,
        node_id: &str,
        instance_id: &str,
    ) -> Result<CsrSignResult, CouncilError> {
        let ikm = self
            .wrapping_ikm
            .as_ref()
            .ok_or_else(|| CouncilError::SecurityError("no wrapping IKM available".to_string()))?;

        let security_state = self.security_state().await;

        // Get Workload CA
        let workload_ca = security_state
            .get_ca(CaRole::Workload)
            .ok_or_else(|| CouncilError::SecurityError("no Workload CA in state".to_string()))?;
        let root_ca = security_state
            .get_ca(CaRole::Root)
            .ok_or_else(|| CouncilError::SecurityError("no Root CA in state".to_string()))?;

        let workload_ca_cert_der = workload_ca.certificate_der.clone();
        let root_ca_cert_der = root_ca.certificate_der.clone();

        // Unwrap CA private key
        let wrapped = workload_ca.private_key_wrapped.as_ref().ok_or_else(|| {
            CouncilError::SecurityError("Workload CA has no private key".to_string())
        })?;
        let ca_key_der = crate::sesame::crypto::unwrap_key(ikm, wrapped)
            .map_err(|e| CouncilError::SecurityError(format!("failed to unwrap CA key: {e}")))?;

        // Reconstruct CA keypair
        let ca_key_pki = rustls::pki_types::PrivateKeyDer::try_from(ca_key_der)
            .map_err(|e| CouncilError::SecurityError(format!("invalid CA key DER: {e}")))?;
        let ca_keypair =
            rcgen::KeyPair::from_der_and_sign_algo(&ca_key_pki, &rcgen::PKCS_ECDSA_P256_SHA256)
                .map_err(|e| CouncilError::SecurityError(format!("invalid CA keypair: {e}")))?;

        // Rebuild the CA params from the REAL CA certificate, not from a
        // reconstructed DN string. The issuer distinguished name the signed
        // leaf carries must match the actual Workload CA's subject exactly —
        // image verification now checks issuer binding (IMG3), so a DN derived
        // from a caller-supplied `cluster_name` that differs from the CA's own
        // name would produce a leaf that fails the binding. `from_ca_cert_der`
        // reads the subject straight off the certificate.
        let ca_cert_der_pki = rustls::pki_types::CertificateDer::from(workload_ca_cert_der.clone());
        let ca_params = rcgen::CertificateParams::from_ca_cert_der(&ca_cert_der_pki)
            .map_err(|e| CouncilError::SecurityError(format!("invalid CA cert DER: {e}")))?;

        // Allocate serial via Raft, reading the assigned value straight from the
        // write response so concurrent signings never derive the same serial.
        let serial = match self.write(RaftRequest::AllocateSerial).await? {
            crate::council::types::CouncilResponse::SerialAllocated { serial } => {
                SerialNumber(serial)
            }
            other => {
                return Err(CouncilError::SecurityError(format!(
                    "AllocateSerial returned unexpected response: {other:?}"
                )));
            }
        };
        let state_after = self.security_state().await;

        // Sign the CSR
        let cert_der = crate::sesame::identity::validate_and_sign_csr(
            csr_der,
            spiffe_uri,
            serial,
            usage,
            &ca_keypair,
            &ca_params,
            std::time::SystemTime::now(),
        )
        .map_err(|e| CouncilError::SecurityError(format!("CSR signing failed: {e}")))?;

        // Mint OIDC JWT if config is present
        let jwt_token = if let Some(ref oidc_config) = state_after.oidc_signing_config {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let claims = crate::sesame::types::WorkloadJwtClaims {
                iss: oidc_config.issuer.clone(),
                sub: spiffe_uri.to_uri(),
                aud: vec![format!("spiffe://{cluster_name}")],
                exp: now + 3600,
                iat: now,
                namespace: spiffe_uri.namespace.clone(),
                app: spiffe_uri.name.clone(),
                cluster: cluster_name.to_string(),
                node: node_id.to_string(),
                instance: instance_id.to_string(),
            };
            let token = crate::sesame::oidc::mint_jwt(&claims, oidc_config, ikm)
                .map_err(|e| CouncilError::SecurityError(format!("JWT minting failed: {e}")))?;
            Some(token)
        } else {
            None
        };

        Ok(CsrSignResult {
            cert_der,
            workload_ca_cert_der,
            root_ca_cert_der,
            jwt_token,
            serial,
        })
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

    #[tokio::test]
    async fn manifest_catalog_returns_the_replicated_catalog() {
        let (nodes, _router) = create_cluster(1).await;
        init_cluster(&nodes).await;
        // A fresh council has an empty catalogue; the accessor reads it from the
        // same replicated desired state as security_state().
        assert!(nodes[0].manifest_catalog().await.manifests.is_empty());
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
                if let Some(leader) = node.current_leader().await
                    && exclude.is_none_or(|ex| leader != ex)
                {
                    return Some(leader);
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

    async fn wait_for_all_states(
        nodes: &[CouncilNode],
        timeout: Duration,
        predicate: impl Fn(&DesiredState) -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let mut all_match = true;
            for node in nodes {
                if !predicate(&node.desired_state().await) {
                    all_match = false;
                    break;
                }
            }
            if all_match {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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

        assert!(
            wait_for_all_states(&nodes, Duration::from_secs(3), |state| state
                .apps
                .contains_key(&AppId::new("web", "prod")))
            .await,
            "pre-growth entry did not replicate"
        );

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
        assert!(
            wait_for_all_states(&nodes, Duration::from_secs(3), |state| {
                state.config.get("region").map(String::as_str) == Some("us-east")
            })
            .await,
            "configuration did not replicate after cluster growth"
        );

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

        assert!(
            wait_for_all_states(&nodes, Duration::from_secs(3), |state| {
                state.apps.contains_key(&app_id)
            })
            .await,
            "app did not replicate"
        );

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

        assert!(
            wait_for_all_states(&nodes, Duration::from_secs(3), |state| {
                state.scheduling.contains_key(&app_id)
            })
            .await,
            "scheduling decision did not replicate"
        );

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

        assert!(
            wait_for_all_states(&nodes, Duration::from_secs(3), |state| {
                state.apps.contains_key(&AppId::new("web", "prod"))
                    && state.config.get("env").map(String::as_str) == Some("production")
            })
            .await,
            "state did not replicate before failover"
        );

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
    async fn join_token_issuance_fails_closed_on_a_follower_and_survives_failover() {
        let (nodes, _router) = create_cluster(3).await;
        init_cluster(&nodes).await;

        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        let leader = &nodes[(leader_id - 1) as usize];
        let (_, before_failover) =
            crate::sesame::join::create_join_token(crate::sesame::join::DEFAULT_JOIN_TOKEN_TTL)
                .unwrap();
        let before_hash = before_failover.token_hash;
        leader
            .write(RaftRequest::CreateJoinToken(before_failover))
            .await
            .unwrap();
        assert!(
            wait_for_all_states(&nodes, Duration::from_secs(3), |state| {
                state
                    .security_state
                    .join_tokens
                    .iter()
                    .any(|token| token.token_hash == before_hash)
            })
            .await
        );

        let follower_idx = nodes
            .iter()
            .enumerate()
            .find(|(index, _)| *index as u64 + 1 != leader_id)
            .map(|(index, _)| index)
            .unwrap();
        let (_, refused) =
            crate::sesame::join::create_join_token(crate::sesame::join::DEFAULT_JOIN_TOKEN_TTL)
                .unwrap();
        let refused_hash = refused.token_hash;
        assert!(matches!(
            nodes[follower_idx]
                .write(RaftRequest::CreateJoinToken(refused))
                .await,
            Err(CouncilError::ForwardToLeader { .. })
        ));
        for node in &nodes {
            assert!(
                node.security_state()
                    .await
                    .join_tokens
                    .iter()
                    .all(|token| token.token_hash != refused_hash),
                "a follower must not commit the token it refused"
            );
        }

        leader.shutdown().await.unwrap();
        let remaining: Vec<&CouncilNode> = nodes
            .iter()
            .enumerate()
            .filter(|(index, _)| *index as u64 + 1 != leader_id)
            .map(|(_, node)| node)
            .collect();
        let new_leader_id =
            wait_for_leader_refs(&remaining, Duration::from_secs(5), Some(leader_id))
                .await
                .expect("no new leader");
        let new_leader = &nodes[(new_leader_id - 1) as usize];

        let (_, after_failover) =
            crate::sesame::join::create_join_token(crate::sesame::join::DEFAULT_JOIN_TOKEN_TTL)
                .unwrap();
        let after_hash = after_failover.token_hash;
        new_leader
            .write(RaftRequest::CreateJoinToken(after_failover))
            .await
            .unwrap();
        let state = new_leader.security_state().await;
        assert!(
            state
                .join_tokens
                .iter()
                .any(|token| token.token_hash == before_hash)
        );
        assert!(
            state
                .join_tokens
                .iter()
                .any(|token| token.token_hash == after_hash)
        );
        assert!(
            state
                .join_tokens
                .iter()
                .all(|token| token.token_hash != refused_hash)
        );
    }

    #[tokio::test]
    async fn membership_changes_compose_add_then_evict() {
        // The self-healing reconciler's sequence: add a learner, promote it
        // via joint consensus, then evict a voter without leaving it behind
        // as a learner. Each step must compose with the previous one.
        let (mut nodes, router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();

        // Bring up node 4 and walk it in: learner first, then voter.
        let network = InMemoryRaftNetworkFactory::new(4, router.clone());
        let node4 = CouncilNode::new(
            4,
            fast_config(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap();
        router.register(4, node4.raft().clone()).await;
        nodes.push(node4);

        let leader = &nodes[(leader_id - 1) as usize];
        leader.add_learner(4, node_info(4)).await.unwrap();
        leader
            .change_membership(BTreeSet::from([1, 2, 3, 4]))
            .await
            .unwrap();

        // Evict a non-leader voter entirely.
        let victim = if leader_id == 3 { 2 } else { 3 };
        let mut remaining = BTreeSet::from([1, 2, 3, 4]);
        remaining.remove(&victim);
        leader
            .change_membership_evicting(remaining.clone())
            .await
            .unwrap();

        let metrics = leader.metrics().borrow().clone();
        let membership = metrics.membership_config.membership().clone();
        let voters: BTreeSet<u64> = membership.voter_ids().collect();
        let all: BTreeSet<u64> = membership.nodes().map(|(id, _)| *id).collect();
        assert_eq!(voters, remaining);
        assert!(
            !all.contains(&victim),
            "evicted voter must not linger as a learner: {all:?}"
        );

        // The council still commits after the whole dance.
        let resp = leader.write(RaftRequest::Noop).await.unwrap();
        assert!(matches!(resp, CouncilResponse::Applied { .. }));
    }

    #[tokio::test]
    async fn remove_learner_drops_it_from_membership() {
        let (mut nodes, router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();

        let network = InMemoryRaftNetworkFactory::new(4, router.clone());
        let node4 = CouncilNode::new(
            4,
            fast_config(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap();
        router.register(4, node4.raft().clone()).await;
        nodes.push(node4);

        let leader = &nodes[(leader_id - 1) as usize];
        leader.add_learner(4, node_info(4)).await.unwrap();
        leader.remove_learner(4).await.unwrap();

        let metrics = leader.metrics().borrow().clone();
        let membership = metrics.membership_config.membership().clone();
        let all: BTreeSet<u64> = membership.nodes().map(|(id, _)| *id).collect();
        assert!(!all.contains(&4), "removed learner still present: {all:?}");
        // Voters are untouched.
        let voters: BTreeSet<u64> = membership.voter_ids().collect();
        assert_eq!(voters, BTreeSet::from([1, 2, 3]));
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

        assert!(
            wait_for_all_states(&nodes, Duration::from_secs(3), |state| {
                state.config.get("before").map(String::as_str) == Some("partition")
            })
            .await,
            "pre-partition write did not replicate"
        );

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
        assert!(
            wait_for_all_states(&nodes, Duration::from_secs(5), |state| {
                state.config.get("before").map(String::as_str) == Some("partition")
                    && state.config.get("after").map(String::as_str) == Some("partition")
            })
            .await,
            "minority did not catch up after the partition healed"
        );

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

    // -----------------------------------------------------------------------
    // CSR signing test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn sign_workload_csr_on_leader() {
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
        use crate::sesame::types::{SpiffeUri, WorkloadType};

        // Create a single-node council WITH a wrapping IKM
        let wrapping_ikm = b"test-wrapping-material-32bytes!!";
        let router = InMemoryRaftRouter::new();
        let network = InMemoryRaftNetworkFactory::new(1, router.clone());
        let node = CouncilNode::new(
            1,
            fast_config(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            Some(*wrapping_ikm),
        )
        .await
        .unwrap();
        router.register(1, node.raft().clone()).await;

        let members = BTreeMap::from([(1, node_info(1))]);
        node.initialize(members).await.unwrap();
        assert_eq!(
            wait_for_leader(std::slice::from_ref(&node), Duration::from_secs(5)).await,
            Some(1),
            "single-node council did not elect itself"
        );

        // Bootstrap SecurityState with a CA hierarchy
        let hierarchy =
            crate::sesame::ca::generate_ca_hierarchy("test-cluster", wrapping_ikm).unwrap();
        let oidc_config = crate::sesame::oidc::generate_oidc_keypair(
            "https://test.reliaburger.dev",
            wrapping_ikm,
        )
        .unwrap();
        let security_state = crate::sesame::types::SecurityState {
            certificate_authorities: vec![
                crate::sesame::types::CertificateAuthority {
                    private_key_wrapped: None,
                    ..hierarchy.root.ca
                },
                hierarchy.node.ca,
                hierarchy.workload.ca,
                hierarchy.ingress.ca,
            ],
            age_keypairs: vec![],
            api_tokens: vec![],
            join_tokens: vec![],
            next_serial: 10,
            oidc_signing_config: Some(oidc_config),
            crl: crate::sesame::types::Crl::default(),
            secret_seals: std::collections::BTreeMap::new(),
        };
        node.write(RaftRequest::SecurityStateInit(Box::new(security_state)))
            .await
            .unwrap();

        // Generate a CSR
        let spiffe_uri = SpiffeUri {
            trust_domain: "test-cluster".to_string(),
            namespace: "default".to_string(),
            workload_type: WorkloadType::App,
            name: "api".to_string(),
        };
        let (csr_der, _private_key) =
            crate::sesame::identity::create_workload_csr(&spiffe_uri).unwrap();

        // Sign it
        let result = node
            .sign_workload_csr(
                &csr_der,
                &spiffe_uri,
                crate::sesame::identity::CertUsage::Mtls,
                "test-cluster",
                "node-01",
                "api-0",
            )
            .await
            .unwrap();

        assert!(!result.cert_der.is_empty());
        assert!(!result.workload_ca_cert_der.is_empty());
        assert!(!result.root_ca_cert_der.is_empty());
        assert!(result.jwt_token.is_some());

        // Verify the signed cert chains to the Workload CA
        crate::sesame::cert::verify_signature(&result.cert_der, &result.workload_ca_cert_der)
            .unwrap();

        // Verify the JWT is valid
        let state = node.security_state().await;
        let oidc = state.oidc_signing_config.as_ref().unwrap();
        let claims =
            crate::sesame::oidc::verify_jwt(result.jwt_token.as_ref().unwrap(), oidc).unwrap();
        assert_eq!(claims.sub, "spiffe://test-cluster/ns/default/app/api");
    }
}

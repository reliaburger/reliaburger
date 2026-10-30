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
use super::fence::{FenceSnapshot, RecoveryFence};
use super::handover::{ElectionHold, campaign_round, leader_after_handover};
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
    /// Every trusted Workload CA and root, for the workload's `ca.pem`
    /// (F04 R2).
    pub ca_bundle_der: Vec<Vec<u8>>,
    /// OIDC JWT token (if OIDC config is present).
    pub jwt_token: Option<String>,
    /// Allocated serial number.
    pub serial: SerialNumber,
}

/// The Raft metrics type every council consumer watches.
pub type RaftMetrics = openraft::RaftMetrics<u64, CouncilNodeInfo>;

/// How the rest of the node should see `metrics` under `fence`: a fenced
/// node's council was replaced, so it names no leader and is no leader.
fn fenced_view(mut metrics: RaftMetrics, fence: &FenceSnapshot) -> RaftMetrics {
    if fence.fenced_by().is_some() {
        metrics.current_leader = None;
        metrics.state = openraft::ServerState::Follower;
    }
    metrics
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
    /// Order grant proposals against membership transitions on this leader.
    node_fault_membership: tokio::sync::Mutex<()>,
    /// When this node, as leader, last served each endpoint consumer.
    consumer_contacts: tokio::sync::Mutex<crate::onion::lease::ConsumerContacts>,
    /// Master secret for unwrapping CA private keys (in-memory only).
    wrapping_ikm: Option<[u8; 32]>,
    /// The recovery fence (#424) and the Raft metrics as the rest of the node
    /// should see them: a fenced node never claims a leader.
    fence: Option<(RecoveryFence, watch::Receiver<RaftMetrics>)>,
    /// Whether this voter is holding its elections while leadership moves
    /// off it (#755).
    election_hold: ElectionHold,
    /// The leadership proof callers share, so a burst of requests doesn't
    /// send a burst of heartbeat rounds (#1441).
    leadership_proof: super::proof::SharedProof,
    /// Test hook: writes and linearizable reads never return, as they don't
    /// on a leader that has lost its quorum and not yet stepped down.
    #[cfg(test)]
    writes_hang: std::sync::atomic::AtomicBool,
    /// Test hook: every write that hung, with the moment it arrived.
    #[cfg(test)]
    hung_writes: std::sync::Mutex<Vec<(tokio::time::Instant, RaftRequest)>>,
    /// Test hook: how many times [`Self::desired_state`] cloned the whole
    /// desired state, so a loop can be shown not to (#1030).
    #[cfg(test)]
    full_desired_reads: std::sync::atomic::AtomicU64,
    /// Test hook: how many leadership proofs this node has run (#1441).
    #[cfg(test)]
    leadership_proofs: std::sync::atomic::AtomicU64,
    /// Test hook: leadership proofs running now, and the most ever at once.
    #[cfg(test)]
    proofs_in_flight: (std::sync::atomic::AtomicU64, std::sync::atomic::AtomicU64),
}

/// Test hook: counts a leadership proof while it runs, including one a
/// caller's timeout cancels (#1441).
#[cfg(test)]
struct ProofInFlight<'a>(&'a std::sync::atomic::AtomicU64);

#[cfg(test)]
impl<'a> ProofInFlight<'a> {
    fn start(node: &'a CouncilNode) -> Self {
        use std::sync::atomic::Ordering;
        node.leadership_proofs.fetch_add(1, Ordering::SeqCst);
        let (now, most) = &node.proofs_in_flight;
        most.fetch_max(now.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        Self(now)
    }
}

#[cfg(test)]
impl Drop for ProofInFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
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

        let election_hold = ElectionHold::new(raft.clone(), raft_id);
        Ok(Self {
            raft,
            raft_id,
            election_hold,
            leadership_proof: super::proof::SharedProof::new(),
            state_machine,
            node_fault_membership: tokio::sync::Mutex::new(()),
            consumer_contacts: tokio::sync::Mutex::new(crate::onion::lease::ConsumerContacts::new(
                std::time::Instant::now(),
            )),
            wrapping_ikm,
            fence: None,
            #[cfg(test)]
            writes_hang: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            hung_writes: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            full_desired_reads: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            leadership_proofs: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            proofs_in_flight: Default::default(),
        })
    }

    /// Put this node behind a recovery fence (#424), shared with its Raft
    /// transport. Once the fence closes, writes and linearizable reads are
    /// refused, and [`metrics`](Self::metrics) stops naming any leader, so
    /// the leader hint, placement and every leader-only loop stand down.
    ///
    /// Spawns the task that filters the metrics, so call it on the runtime.
    pub fn with_recovery_fence(mut self, fence: RecoveryFence) -> Self {
        let mut raw = self.raft.metrics();
        let mut fence_rx = fence.subscribe();
        let (filtered_tx, filtered_rx) =
            watch::channel(fenced_view(raw.borrow().clone(), &fence_rx.borrow()));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = raw.changed() => if changed.is_err() { break },
                    changed = fence_rx.changed() => if changed.is_err() { break },
                }
                let view = fenced_view(raw.borrow().clone(), &fence_rx.borrow());
                if filtered_tx.send(view).is_err() {
                    break;
                }
            }
        });
        self.fence = Some((fence, filtered_rx));
        self
    }

    /// This node's recovery fence, when it runs behind one.
    pub fn recovery_fence(&self) -> Option<&RecoveryFence> {
        self.fence.as_ref().map(|(fence, _)| fence)
    }

    /// The fence error, when this node is fenced.
    fn fenced_error(&self) -> Option<CouncilError> {
        let snapshot = self.recovery_fence()?.snapshot();
        if snapshot.split_with().is_some() {
            return Some(CouncilError::Split {
                epoch: snapshot.epoch,
            });
        }
        snapshot
            .fenced_by()
            .map(|newer_epoch| CouncilError::Fenced {
                epoch: snapshot.epoch,
                newer_epoch,
            })
    }

    /// Test hook: from now on every write and linearizable read hangs.
    #[cfg(test)]
    pub(crate) fn hang_writes(&self) {
        self.writes_hang
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test hook: the writes that have hung since [`Self::hang_writes`], in
    /// arrival order.
    #[cfg(test)]
    pub(crate) fn hung_writes(&self) -> Vec<(tokio::time::Instant, RaftRequest)> {
        self.hung_writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[cfg(test)]
    async fn hang_write_if_asked(&self, request: &RaftRequest) {
        if self.writes_hang.load(std::sync::atomic::Ordering::SeqCst) {
            // A plain mutex in a test hook: the guard is a temporary,
            // dropped before the await below.
            self.hung_writes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((tokio::time::Instant::now(), request.clone()));
            std::future::pending::<()>().await;
        }
    }

    #[cfg(test)]
    async fn hang_if_asked(&self) {
        if self.writes_hang.load(std::sync::atomic::Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
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
        #[cfg(test)]
        self.hang_write_if_asked(&request).await;
        if let Some(error) = self.fenced_error() {
            return Err(error);
        }
        let _membership = if matches!(
            &request,
            RaftRequest::ReserveNodeFault { .. } | RaftRequest::DecommissionNode { .. }
        ) {
            Some(self.node_fault_membership.lock().await)
        } else {
            None
        };
        let result = self.raft.client_write(request).await;
        match result {
            Ok(resp) => Ok(resp.data),
            Err(e) => match e {
                openraft::error::RaftError::APIError(ClientWriteError::ForwardToLeader(fwd)) => {
                    Err(CouncilError::ForwardToLeader {
                        leader: fwd.leader_node.map(|node| node.name),
                    })
                }
                other => Err(CouncilError::WriteFailed(other.to_string())),
            },
        }
    }

    /// Return the current leader's node ID, if known.
    pub async fn current_leader(&self) -> Option<u64> {
        if self.fenced_error().is_some() {
            return None;
        }
        self.raft.current_leader().await
    }

    /// Return `true` if this node is the current leader, proved by a quorum
    /// answering a heartbeat sent after this call.
    ///
    /// Concurrent callers share one proof at a time (#1441): a caller that
    /// arrives while one runs waits for the next, which every caller that
    /// arrived meanwhile shares.
    pub async fn is_leader(&self) -> bool {
        if self.fenced_error().is_some() {
            return false;
        }
        self.leadership_proof
            .run(|| async {
                #[cfg(test)]
                let _in_flight = ProofInFlight::start(self);
                self.raft.ensure_linearizable().await.is_ok()
            })
            .await
    }

    /// Return `true` once this node proves it leads, trying until `within`
    /// passes while it still believes it leads.
    ///
    /// One [`is_leader`](Self::is_leader) round gives each follower a single
    /// heartbeat interval to answer. On a loaded host both can miss it, and
    /// a settled leader then reads as "not leader" (#1009). A node that
    /// names another voter, or no leader at all, returns `false` straight
    /// away: it has nothing to confirm.
    pub async fn confirm_leadership(&self, within: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        let retry_pause = std::time::Duration::from_millis(self.raft.config().heartbeat_interval);
        loop {
            match tokio::time::timeout_at(deadline, self.is_leader()).await {
                Ok(true) => return true,
                Ok(false) => {}
                Err(_) => return false,
            }
            let believes_it_leads = {
                let metrics = self.metrics();
                let metrics = metrics.borrow();
                metrics.current_leader == Some(metrics.id)
            };
            if !believes_it_leads || tokio::time::Instant::now() + retry_pause >= deadline {
                return false;
            }
            tokio::time::sleep(retry_pause).await;
        }
    }

    /// Stop this voter campaigning until another voter leads or `until`
    /// passes (#755).
    ///
    /// openraft 0.9 has no leadership transfer: another voter's campaign
    /// deposes the leader, but the election that follows favours the longest
    /// log, which is usually the old leader's. A leader that must give way
    /// holds its elections first, so it can't win that election. It still
    /// votes, and it still leads until a higher term deposes it. Calling this
    /// again pushes the deadline out.
    pub fn hold_elections_until(&self, until: std::time::Instant) {
        self.election_hold.hold_until(until);
    }

    /// Whether this voter is holding its elections
    /// ([`hold_elections_until`](Self::hold_elections_until)).
    pub fn elections_held(&self) -> bool {
        self.election_hold.is_held()
    }

    /// Campaign until a voter other than `previous` leads, and return it.
    ///
    /// The first campaign deposes `previous` at a higher term. Followers
    /// refuse every candidate while the leader lease runs, so this campaigns
    /// again each [`campaign_round`] until leadership moves. The winner may
    /// be another voter with a longer log than this one; Raft's vote rule
    /// picks it, and that's still a handover. `previous` should be holding
    /// its elections, or it may win the election its deposition starts.
    pub async fn campaign_until_leadership_moves(
        &self,
        previous: Option<u64>,
        deadline: std::time::Instant,
    ) -> Result<u64, CouncilError> {
        if let Some(error) = self.fenced_error() {
            return Err(error);
        }
        let started = std::time::Instant::now();
        let round = campaign_round(std::time::Duration::from_millis(
            self.raft.config().election_timeout_max,
        ));
        let mut metrics = self.raft.metrics();
        loop {
            let leader = metrics.borrow().current_leader;
            if let Some(leader) = leader_after_handover(previous, leader) {
                return Ok(leader);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                let previous = previous
                    .and_then(|id| {
                        let metrics = metrics.borrow();
                        let membership = metrics.membership_config.membership();
                        membership.get_node(&id).map(|node| node.name.clone())
                    })
                    .unwrap_or_else(|| "no known leader".to_string());
                return Err(CouncilError::HandoverTimedOut {
                    previous,
                    waited: now.duration_since(started),
                });
            }
            self.raft
                .trigger()
                .elect()
                .await
                .map_err(|e| CouncilError::Fatal(e.to_string()))?;
            let round_end = tokio::time::Instant::from_std((now + round).min(deadline));
            let moved =
                metrics.wait_for(|m| leader_after_handover(previous, m.current_leader).is_some());
            // A timeout ends the round; a closed watch means Raft stopped, and
            // the next campaign reports that.
            let _ = tokio::time::timeout_at(round_end, moved).await;
        }
    }

    /// The leader's volatile record of when it last served each endpoint
    /// consumer. Serving a consumer and discharging one both take this lock,
    /// so a discharge can never race a poll that renews the same lease.
    pub fn consumer_contacts(&self) -> &tokio::sync::Mutex<crate::onion::lease::ConsumerContacts> {
        &self.consumer_contacts
    }

    /// The Raft term this node currently knows.
    pub fn current_term(&self) -> u64 {
        self.raft.metrics().borrow().current_term
    }

    /// Subscribe to Raft metrics changes.
    ///
    /// Behind a recovery fence, a fenced node's metrics name no leader.
    pub fn metrics(&self) -> watch::Receiver<RaftMetrics> {
        match &self.fence {
            Some((_, filtered)) => filtered.clone(),
            None => self.raft.metrics(),
        }
    }

    /// Observe committed requested and completed GitOps trigger generations.
    pub fn gitops_trigger_updates(&self) -> watch::Receiver<(u64, u64)> {
        self.state_machine.gitops_trigger_updates()
    }

    /// Read the current desired state from the state machine.
    pub async fn desired_state(&self) -> DesiredState {
        #[cfg(test)]
        self.full_desired_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.state_machine.desired_state().await
    }

    /// Test hook: how many whole-state clones [`Self::desired_state`] has
    /// made on this node.
    #[cfg(test)]
    pub(crate) fn full_desired_reads(&self) -> u64 {
        self.full_desired_reads
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Borrow the local desired state for `read`, without cloning all of
    /// it. Follower-local, like [`Self::desired_state`].
    pub async fn read_desired<T>(&self, read: impl FnOnce(&DesiredState) -> T) -> T {
        self.state_machine.read_desired(read).await
    }

    /// Establish an applied log barrier before inspecting fault ownership.
    /// A timed-out grant write may still commit. A following no-op must apply
    /// first, so dropping the caller's future cannot hide that reservation.
    async fn guard_membership_change(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, ()>, CouncilError> {
        let guard = self.node_fault_membership.lock().await;
        self.write(RaftRequest::Noop).await?;
        if self
            .desired_state()
            .await
            .node_fault_reservations
            .active
            .is_some()
        {
            return Err(CouncilError::WriteFailed(
                "membership change waits for node fault reversal".into(),
            ));
        }
        Ok(guard)
    }

    /// Add a learner node to the cluster.
    ///
    /// The learner receives log replication but does not vote. This returns
    /// once the membership entry that adds it has committed, which takes the
    /// existing voters a commit or two. It does not wait for the learner to
    /// catch up: on a slow disk that takes as long as the learner's log gap
    /// needs (#779). Replication carries on in the background; promote the
    /// learner with `change_membership()` once its replication metrics show
    /// it close behind the leader, as the council planner does.
    pub async fn add_learner(&self, id: u64, info: CouncilNodeInfo) -> Result<(), CouncilError> {
        let _guard = self.guard_membership_change().await?;
        if self
            .security_state()
            .await
            .crl
            .retired_nodes
            .contains_key(&info.name)
        {
            return Err(CouncilError::WriteFailed("node identity is retired".into()));
        }
        self.raft
            .add_learner(id, info, false)
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
        let _guard = self.guard_membership_change().await?;
        self.refuse_retired_voters(&members).await?;
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
        let _guard = self.guard_membership_change().await?;
        self.refuse_retired_voters(&members).await?;
        self.raft
            .change_membership(ChangeMembers::ReplaceAllVoters(members), false)
            .await
            .map_err(|e| CouncilError::WriteFailed(e.to_string()))?;
        Ok(())
    }

    /// The voter set a committed but unfinished membership change is heading
    /// for, or `None` when the membership is uniform or its joint step has
    /// not committed yet.
    ///
    /// openraft changes voters in two steps, and the second (uniform) step is
    /// proposed by the caller's future once the joint step commits. A caller
    /// that gives up in between, such as the council reconciler's bounded
    /// operation, leaves the council in the joint config, and nothing in
    /// openraft finishes it (#673).
    pub fn unfinished_membership_change(&self) -> Option<BTreeSet<u64>> {
        let metrics = self.metrics().borrow().clone();
        let configs = metrics.membership_config.membership().get_joint_config();
        if configs.len() < 2 {
            return None;
        }
        let membership_index = metrics.membership_config.log_id().map(|id| id.index);
        let applied_index = metrics.last_applied.map(|id| id.index);
        if applied_index < membership_index {
            return None;
        }
        configs.last().cloned()
    }

    /// Propose the uniform step of an unfinished membership change, so the
    /// council reaches the voter set the original change asked for. Returns
    /// whether a change was proposed. Demoted voters are kept as learners: the
    /// reconciler removes any that are dead once the council is whole again.
    pub async fn finish_membership_change(&self) -> Result<bool, CouncilError> {
        let Some(target) = self.unfinished_membership_change() else {
            return Ok(false);
        };
        self.change_membership(target).await?;
        Ok(true)
    }

    async fn refuse_retired_voters(&self, members: &BTreeSet<u64>) -> Result<(), CouncilError> {
        let security = self.security_state().await;
        let metrics = self.metrics().borrow().clone();
        if members.iter().any(|id| {
            metrics
                .membership_config
                .membership()
                .get_node(id)
                .is_some_and(|node| security.crl.retired_nodes.contains_key(&node.name))
        }) {
            return Err(CouncilError::WriteFailed("node identity is retired".into()));
        }
        Ok(())
    }

    /// Remove a learner from the membership entirely. The node must not be
    /// a voter; demote it first via `change_membership`.
    pub async fn remove_learner(&self, id: u64) -> Result<(), CouncilError> {
        let _guard = self.guard_membership_change().await?;
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

    /// Read the current security state from the **local** state machine.
    ///
    /// This is a follower-local read: it reflects everything this node has
    /// applied, which during a leadership transition can lag the committed
    /// state by a replication round. Fine for advisory uses (the CRL refresh
    /// ticker, fast-fail pre-checks whose authoritative decision goes through
    /// Raft anyway). For a read that *is* the security decision, use
    /// [`Self::security_state_linearizable`].
    pub async fn security_state(&self) -> crate::sesame::types::SecurityState {
        self.state_machine.desired_state().await.security_state
    }

    /// How many voters the council grows to, if the cluster was created
    /// with a size. Borrows the state rather than cloning it, so the
    /// reconciler can ask on every tick.
    pub async fn council_size(&self) -> Option<crate::council::CouncilSize> {
        self.state_machine
            .read_desired(|state| state.council_size)
            .await
    }

    /// Whether an operator has permanently retired this node identity.
    /// Cheap enough to call on every request: it borrows, never clones.
    pub async fn is_node_retired(&self, node_id: &str) -> bool {
        self.state_machine
            .read_desired(|state| state.security_state.crl.retired_nodes.contains_key(node_id))
            .await
    }

    /// What a repeated producer retirement of `execution` would answer,
    /// read from this node's applied state instead of committing a no-op
    /// entry (#1440). See [`DesiredState::committed_producer_release`].
    /// Current only after a linearizable read, such as
    /// [`Self::security_state_linearizable`], on the leader.
    pub async fn committed_producer_release(
        &self,
        node_id: &str,
        execution: &crate::grill::RuntimeExecution,
    ) -> Option<bool> {
        self.state_machine
            .read_desired(|state| state.committed_producer_release(node_id, execution))
            .await
    }

    /// Read the security state only if this node can prove the read is current.
    ///
    /// `ensure_linearizable` confirms with a quorum that this node is still
    /// the leader and has applied everything committed, so a `RevokeCertificate`
    /// or token change that committed elsewhere can't be missed. It fails on a
    /// follower (and on a leader that has been deposed and doesn't know it yet),
    /// which is the point: a node that cannot prove freshness returns an error
    /// rather than a stale answer it presents as authoritative (O7).
    pub async fn security_state_linearizable(
        &self,
    ) -> Result<crate::sesame::types::SecurityState, CouncilError> {
        #[cfg(test)]
        self.hang_if_asked().await;
        if let Some(error) = self.fenced_error() {
            return Err(error);
        }
        self.raft
            .ensure_linearizable()
            .await
            .map_err(|e| CouncilError::ReadFailed(e.to_string()))?;
        Ok(self.state_machine.desired_state().await.security_state)
    }

    /// [`Self::security_state_linearizable`], but a leader gets `within` to
    /// prove it leads, as [`Self::confirm_leadership`] does.
    ///
    /// One leadership round gives each follower a single heartbeat interval
    /// to answer, so on a loaded host a settled leader can fail it and refuse
    /// a read it should serve (#1009). A follower, or a node that names no
    /// leader, fails at once.
    pub async fn security_state_confirmed(
        &self,
        within: std::time::Duration,
    ) -> Result<crate::sesame::types::SecurityState, CouncilError> {
        #[cfg(test)]
        self.hang_if_asked().await;
        if let Some(error) = self.fenced_error() {
            return Err(error);
        }
        if !self.confirm_leadership(within).await {
            return Err(CouncilError::ReadFailed(
                "this node can't prove it leads the council".into(),
            ));
        }
        Ok(self.state_machine.desired_state().await.security_state)
    }

    /// The desired state, confirmed fresh with a quorum first, as
    /// [`Self::security_state_linearizable`] does. Fails on a follower and
    /// on a leader that has been deposed and doesn't know it yet. The
    /// council backup exports this, so a stale leader never ships old state.
    pub async fn desired_state_linearizable(&self) -> Result<DesiredState, CouncilError> {
        if let Some(error) = self.fenced_error() {
            return Err(error);
        }
        self.raft
            .ensure_linearizable()
            .await
            .map_err(|e| CouncilError::ReadFailed(e.to_string()))?;
        Ok(self.state_machine.desired_state().await)
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

        // O7: signing a workload certificate *is* the security decision, and
        // it reads CA material and the revocation list. A deposed leader
        // serving this from stale local state would mint certificates against
        // a CA or CRL the cluster has already moved past, so the read has to
        // prove it's current or fail.
        let security_state = self.security_state_linearizable().await?;

        // Get Workload CA
        let workload_ca = security_state
            .active_ca(CaRole::Workload)
            .ok_or_else(|| CouncilError::SecurityError("no Workload CA in state".to_string()))?;
        let root_ca = security_state
            .active_ca(CaRole::Root)
            .ok_or_else(|| CouncilError::SecurityError("no Root CA in state".to_string()))?;

        let workload_ca_cert_der = workload_ca.certificate_der.clone();
        let root_ca_cert_der = root_ca.certificate_der.clone();
        let ca_bundle_der = crate::sesame::trust::workload_ca_bundle(&security_state);

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
        // name would produce a leaf that fails the binding.
        // `issuer_params_from_der` reads the subject straight off the
        // certificate.
        let ca_params = crate::sesame::ca::issuer_params_from_der(&workload_ca_cert_der)
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
            ca_bundle_der,
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
    async fn membership_changes_wait_for_node_fault_reversal() {
        use crate::smoker::{
            reservation::NodeFaultReservation,
            types::{FaultRequest, FaultType},
        };
        let (nodes, _router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader = find_leader(&nodes).await;
        let membership_log_id = *leader.metrics().borrow().membership_config.log_id();
        let reservation = NodeFaultReservation {
            sequence: 1,
            boot_id: "test-boot".into(),
            cleanup_after_unix_ms: 1,
            request: FaultRequest {
                workload: Default::default(),
                task_index: None,
                fault_type: FaultType::NodeKill {
                    kill_containers: false,
                },
                target_service: String::new(),
                namespace: None,
                target_instance: None,
                target_node: Some("node-a".into()),
                duration: Duration::from_secs(30),
                injected_by: "operator".into(),
                reason: None,
                include_leader: true,
                override_safety: true,
                acknowledged: true,
            },
        };
        let admitted = leader
            .write(RaftRequest::ReserveNodeFault {
                reservation: Box::new(reservation),
                membership_log_id,
                unavailable_voters: Default::default(),
            })
            .await
            .unwrap();
        assert!(matches!(admitted, CouncilResponse::Applied { .. }));
        let error = leader
            .change_membership([leader.raft_id].into())
            .await
            .expect_err("a reservation must fence membership changes too");
        assert!(error.to_string().contains("node fault"));
        assert_eq!(
            leader
                .metrics()
                .borrow()
                .membership_config
                .membership()
                .voter_ids()
                .count(),
            3
        );
        leader
            .write(RaftRequest::ReleaseNodeFault { sequence: 1 })
            .await
            .unwrap();
        leader
            .change_membership([leader.raft_id].into())
            .await
            .unwrap();
        for node in &nodes {
            node.shutdown().await.unwrap();
        }
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

    async fn write_admission_fixture(
        council: &crate::council::CouncilNode,
        mut request: crate::council::RaftRequest,
    ) -> Result<crate::council::CouncilResponse, crate::council::CouncilError> {
        let previous = council.desired_state().await.last_applied_log;
        if let crate::council::RaftRequest::SchedulingDecision(decision) = &request {
            request = crate::council::RaftRequest::SchedulingDecisions {
                expected_log_id: previous,
                decisions: vec![decision.clone()],
            }
        }
        council.write(request).await
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
    async fn settled_leader_confirms_after_its_quorum_misses_a_heartbeat_round() {
        let (nodes, router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        // The followers must not depose the leader while it can't reach
        // them: this models slow heartbeats, not a lost leader (#1009).
        for node in &nodes {
            node.hold_elections_until(std::time::Instant::now() + Duration::from_secs(30));
        }
        let leader = &nodes[(leader_id - 1) as usize];
        for follower in (1..=3).filter(|id| *id != leader_id) {
            router.partition(leader_id, follower).await;
        }
        // One confirmation round can't reach a quorum.
        assert!(!leader.is_leader().await);

        let healer = router.clone();
        let heal = async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            healer.heal().await;
        };
        let (confirmed, ()) = tokio::join!(leader.confirm_leadership(Duration::from_secs(5)), heal);
        assert!(confirmed, "a settled leader gave up after one missed round");
    }

    #[tokio::test]
    async fn settled_leader_serves_security_state_after_its_quorum_misses_a_heartbeat_round() {
        let (nodes, router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        // Slow heartbeats, not a lost leader: the followers must not depose
        // it while it can't reach them (#1009).
        for node in &nodes {
            node.hold_elections_until(std::time::Instant::now() + Duration::from_secs(30));
        }
        let leader = &nodes[(leader_id - 1) as usize];
        for follower in (1..=3).filter(|id| *id != leader_id) {
            router.partition(leader_id, follower).await;
        }
        // One confirmation round can't reach a quorum, so the single-round
        // read the registry routes used to make answers 503.
        assert!(leader.security_state_linearizable().await.is_err());

        let healer = router.clone();
        let heal = async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            healer.heal().await;
        };
        let (read, ()) = tokio::join!(
            leader.security_state_confirmed(Duration::from_secs(5)),
            heal
        );
        assert!(
            read.is_ok(),
            "a settled leader refused a security read after one missed round"
        );

        let follower = &nodes[if leader_id == 1 { 1 } else { 0 }];
        let started = std::time::Instant::now();
        assert!(
            follower
                .security_state_confirmed(Duration::from_secs(5))
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    // #1441: every request a loaded leader serves asks it to confirm it
    // leads. Each used to run its own proof, a heartbeat to every follower,
    // in parallel and every heartbeat interval, multiplying heartbeat traffic
    // just when the followers were slow to answer it.
    #[tokio::test]
    async fn concurrent_confirmations_share_one_proof_at_a_time() {
        use std::sync::atomic::Ordering;

        let (nodes, router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        for node in &nodes {
            node.hold_elections_until(std::time::Instant::now() + Duration::from_secs(30));
        }
        let leader = &nodes[(leader_id - 1) as usize];
        // Requests that share a proof all get its answer.
        let answers = futures_util::future::join_all(
            (0..50).map(|_| leader.confirm_leadership(Duration::from_secs(5))),
        )
        .await;
        assert!(answers.iter().all(|confirmed| *confirmed));

        for follower in (1..=3).filter(|id| *id != leader_id) {
            router.partition(leader_id, follower).await;
        }
        let window = Duration::from_millis(600);

        // How many proofs one request runs while the followers can't answer.
        let before = leader.leadership_proofs.load(Ordering::SeqCst);
        assert!(!leader.confirm_leadership(window).await);
        let alone = leader.leadership_proofs.load(Ordering::SeqCst) - before;

        // Fifty requests at once.
        let before = leader.leadership_proofs.load(Ordering::SeqCst);
        let confirmations = (0..50).map(|_| leader.confirm_leadership(window));
        let answers = futures_util::future::join_all(confirmations).await;
        let together = leader.leadership_proofs.load(Ordering::SeqCst) - before;
        assert!(answers.iter().all(|confirmed| !confirmed));
        assert_eq!(
            leader.proofs_in_flight.1.load(Ordering::SeqCst),
            1,
            "two leadership proofs ran at once"
        );
        // A failed proof ends at once and each request sleeps its own retry
        // pause, so requests that wake just after a round start the next:
        // fifty run a few times as many as one. Unshared, they ran fifty
        // times as many.
        assert!(
            together <= 10 * alone,
            "fifty requests ran {together} proofs; one alone ran {alone}"
        );
    }

    #[tokio::test]
    async fn follower_does_not_wait_to_confirm_leadership() {
        let (nodes, _router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        let follower = &nodes[if leader_id == 1 { 1 } else { 0 }];
        let started = std::time::Instant::now();
        assert!(!follower.confirm_leadership(Duration::from_secs(5)).await);
        assert!(started.elapsed() < Duration::from_secs(1));
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
                ordinal: 0,
            }],
        };
        write_admission_fixture(leader, RaftRequest::SchedulingDecision(decision))
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
        let (_, before_failover) = crate::sesame::join::create_join_token(
            crate::sesame::join::DEFAULT_JOIN_TOKEN_TTL,
            "node-99",
        )
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
        let (_, refused) = crate::sesame::join::create_join_token(
            crate::sesame::join::DEFAULT_JOIN_TOKEN_TTL,
            "node-99",
        )
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

        let (_, after_failover) = crate::sesame::join::create_join_token(
            crate::sesame::join::DEFAULT_JOIN_TOKEN_TTL,
            "node-99",
        )
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

    /// A one-voter council whose promotion of learner 2 committed its joint
    /// step after the caller had given up: the shape #673 left behind when the
    /// reconciler's five-second bound dropped `change_membership` mid-flight.
    async fn abandoned_promotion() -> (Vec<CouncilNode>, InMemoryRaftRouter) {
        let (mut nodes, router) = create_cluster(1).await;
        init_cluster(&nodes).await;
        wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        let network = InMemoryRaftNetworkFactory::new(2, router.clone());
        let node2 = CouncilNode::new(
            2,
            fast_config(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap();
        router.register(2, node2.raft().clone()).await;
        nodes.push(node2);
        nodes[0].add_learner(2, node_info(2)).await.unwrap();

        // The joint config {[1], [1, 2]} needs node 2's acknowledgement, so
        // a partition holds it uncommitted past the caller's bound.
        router.partition(1, 2).await;
        let abandoned = tokio::time::timeout(
            Duration::from_millis(500),
            nodes[0].change_membership(BTreeSet::from([1, 2])),
        )
        .await;
        assert!(abandoned.is_err(), "the promotion should still be pending");
        router.heal().await;

        let committed_joint = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if nodes[0].unfinished_membership_change().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(committed_joint.is_ok(), "the joint step never committed");
        (nodes, router)
    }

    fn voter_configs(node: &CouncilNode) -> Vec<BTreeSet<u64>> {
        node.metrics()
            .borrow()
            .membership_config
            .membership()
            .get_joint_config()
            .clone()
    }

    #[tokio::test]
    async fn an_abandoned_promotion_stays_joint_until_finished() {
        let (nodes, _router) = abandoned_promotion().await;
        // Nothing in openraft proposes the uniform step on its own: the
        // council would sit in the joint config, and the reconciler's
        // change-in-flight gate would hold every later change, forever.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            voter_configs(&nodes[0]),
            vec![BTreeSet::from([1]), BTreeSet::from([1, 2])]
        );

        assert!(nodes[0].finish_membership_change().await.unwrap());
        assert_eq!(voter_configs(&nodes[0]), vec![BTreeSet::from([1, 2])]);
        assert_eq!(nodes[0].unfinished_membership_change(), None);
    }

    #[tokio::test]
    async fn finishing_a_uniform_membership_proposes_nothing() {
        let (nodes, _router) = create_cluster(1).await;
        init_cluster(&nodes).await;
        wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        let before = nodes[0].metrics().borrow().last_log_index;
        assert_eq!(nodes[0].unfinished_membership_change(), None);
        assert!(!nodes[0].finish_membership_change().await.unwrap());
        assert_eq!(nodes[0].metrics().borrow().last_log_index, before);
    }

    #[tokio::test]
    async fn adding_a_learner_does_not_wait_for_a_slow_learner_to_catch_up() {
        // #779: on a slow disk, catching a new learner up takes as long as
        // its log gap needs, and the reconciler's bounded operation dropped
        // the wait. Adding the learner is one commit by the existing voters;
        // the catch-up carries on afterwards, and promotion waits for it.
        // No snapshot: the learner must replay the whole log gap. openraft's
        // blocking add waits until the learner trails by at most its
        // `replication_lag_threshold` (5,000 entries), so a gap of 8,000 on
        // a disk that takes half a millisecond per entry holds it for 1.5 s.
        let config = CouncilConfig {
            snapshot_threshold: 10_000,
            ..fast_config()
        };
        let router = InMemoryRaftRouter::new();
        let mut nodes = Vec::new();
        for id in 1..=2 {
            let log_store = MemLogStore::new();
            if id == 2 {
                log_store.slow_entry_writes(Duration::from_micros(500));
            }
            let node = CouncilNode::new(
                id,
                config.clone(),
                InMemoryRaftNetworkFactory::new(id, router.clone()),
                log_store,
                CouncilStateMachine::new(),
                None,
            )
            .await
            .unwrap();
            router.register(id, node.raft().clone()).await;
            nodes.push(node);
        }
        init_cluster(&nodes[..1]).await;
        wait_for_leader(&nodes[..1], Duration::from_secs(5))
            .await
            .unwrap();
        for _ in 0..8_000 {
            nodes[0].write(RaftRequest::Noop).await.unwrap();
        }

        tokio::time::timeout(
            Duration::from_millis(500),
            nodes[0].add_learner(2, node_info(2)),
        )
        .await
        .expect("adding the learner waited for its catch-up")
        .unwrap();
        let membership = nodes[0].metrics().borrow().membership_config.clone();
        assert!(membership.membership().get_node(&2).is_some());
        assert!(!membership.membership().voter_ids().any(|id| id == 2));

        // Replication carries on in the background until the learner holds
        // the leader's log.
        let mut metrics = nodes[0].metrics();
        let last = nodes[0].metrics().borrow().last_log_index;
        tokio::time::timeout(
            Duration::from_secs(30),
            metrics.wait_for(|m| {
                m.replication
                    .as_ref()
                    .and_then(|r| r.get(&2).copied().flatten())
                    .map(|log_id| log_id.index)
                    >= last
            }),
        )
        .await
        .expect("the learner never caught up")
        .unwrap();
        for node in &nodes {
            node.shutdown().await.unwrap();
        }
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
        // The message names the leader as operators know it, not by its
        // internal Raft id ("leader is node Some(5870141345109727948)").
        assert_eq!(
            result.unwrap_err().to_string(),
            format!("not the leader (the leader is node-{leader_id})")
        );
    }

    #[test]
    fn forward_error_without_a_known_leader_says_so() {
        let error = CouncilError::ForwardToLeader { leader: None };
        assert_eq!(error.to_string(), "not the leader, and no leader is known");
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

    #[tokio::test]
    async fn isolated_member_misses_writes_until_healed() {
        let (nodes, router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .expect("leader should be elected");

        // Cut node 3 off from nodes 1 and 2 in both directions.
        for majority in [1u64, 2] {
            router.partition(3, majority).await;
        }

        // The majority keeps, or re-elects, a leader of its own.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let majority_leader = loop {
            let mut found = None;
            for node in &nodes[..2] {
                if let Some(leader) = node.current_leader().await
                    && leader != 3
                {
                    found = Some(leader);
                }
            }
            if let Some(leader) = found
                && nodes[(leader - 1) as usize]
                    .write(RaftRequest::ConfigSet {
                        key: "during_isolation".to_string(),
                        value: "committed".to_string(),
                    })
                    .await
                    .is_ok()
            {
                break leader;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the majority never accepted a write"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_ne!(majority_leader, 3);

        assert!(
            wait_for_all_states(&nodes[..2], Duration::from_secs(5), |state| {
                state.config.contains_key("during_isolation")
            })
            .await,
            "the majority did not apply its own write"
        );
        assert!(
            !nodes[2]
                .desired_state()
                .await
                .config
                .contains_key("during_isolation"),
            "the isolated node received a write through the partition"
        );

        router.heal().await;
        assert!(
            wait_for_all_states(&nodes, Duration::from_secs(10), |state| {
                state.config.get("during_isolation").map(String::as_str) == Some("committed")
            })
            .await,
            "the isolated node did not catch up after the partition healed"
        );
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
            node_leaves: std::collections::BTreeMap::new(),
            pending_intermediates: Vec::new(),
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

    // -----------------------------------------------------------------------
    // Leadership handover (#755)
    // -----------------------------------------------------------------------

    /// The highest term at which `id` was seen leading, sampled from `nodes`.
    fn leading_term(nodes: &[CouncilNode], id: u64) -> Option<u64> {
        nodes
            .iter()
            .map(|node| node.raft().metrics().borrow().clone())
            .filter(|metrics| metrics.current_leader == Some(id))
            .map(|metrics| metrics.current_term)
            .max()
    }

    /// The scenario #646 hit: the leader holds an entry one follower lacks,
    /// that follower's campaign deposes it, and the election that follows
    /// favours the longest log, which is the old leader's. Holding the old
    /// leader's elections leaves the other up-to-date follower as the only
    /// voter that can win.
    #[tokio::test]
    async fn a_deposed_leader_holding_its_elections_does_not_win_back_leadership() {
        let (nodes, router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        let leader = &nodes[(leader_id - 1) as usize];
        let lagging = (1..=3).find(|id| *id != leader_id).unwrap();
        let successor = (1..=3)
            .find(|id| *id != leader_id && *id != lagging)
            .unwrap();
        leader.write(RaftRequest::Noop).await.unwrap();

        // The lagging follower misses the next entry and campaigns while cut
        // off from the leader, so it carries a higher term and a shorter log.
        router.partition(leader_id, lagging).await;
        leader.write(RaftRequest::Noop).await.unwrap();
        let term_before = leader.raft().metrics().borrow().current_term;
        leader.hold_elections_until(std::time::Instant::now() + Duration::from_secs(30));
        assert!(leader.elections_held());
        nodes[(lagging - 1) as usize]
            .raft()
            .trigger()
            .elect()
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Healing lets the leader meet the higher term, which deposes it.
        router.heal().await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let regained = leading_term(&nodes, leader_id).is_some_and(|term| term > term_before);
            assert!(!regained, "the deposed leader won leadership back");
            if nodes
                .iter()
                .all(|node| node.raft().metrics().borrow().current_leader == Some(successor))
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "leadership never reached the up-to-date follower"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Someone else leads, so the old leader may campaign again.
        tokio::time::timeout(Duration::from_secs(5), async {
            while leader.elections_held() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hold outlived the handover");
        for node in &nodes {
            node.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn an_election_hold_ends_at_its_deadline_when_nobody_takes_over() {
        let (nodes, _router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        let leader = &nodes[(leader_id - 1) as usize];
        leader.hold_elections_until(std::time::Instant::now() + Duration::from_millis(300));
        assert!(leader.elections_held());
        // A later hold extends the deadline; an earlier one doesn't cut it.
        leader.hold_elections_until(std::time::Instant::now() + Duration::from_millis(100));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            leader.elections_held(),
            "an earlier deadline cut the hold short"
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while leader.elections_held() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hold outlived its deadline");
        for node in &nodes {
            node.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn campaigning_against_a_held_leader_moves_leadership_off_it() {
        let (nodes, _router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        let leader = &nodes[(leader_id - 1) as usize];
        leader.write(RaftRequest::Noop).await.unwrap();
        let term_before = leader.raft().metrics().borrow().current_term;
        let candidate_id = (1..=3).find(|id| *id != leader_id).unwrap();
        let candidate = &nodes[(candidate_id - 1) as usize];

        leader.hold_elections_until(std::time::Instant::now() + Duration::from_secs(30));
        let new_leader = candidate
            .campaign_until_leadership_moves(
                Some(leader_id),
                std::time::Instant::now() + Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_ne!(new_leader, leader_id);
        assert!(
            leading_term(&nodes, leader_id).is_none_or(|term| term <= term_before),
            "the old leader won leadership back"
        );
        for node in &nodes {
            node.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_campaign_that_cannot_move_leadership_gives_up_at_its_deadline() {
        let (nodes, router) = create_cluster(3).await;
        init_cluster(&nodes).await;
        let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
            .await
            .unwrap();
        let candidate_id = (1..=3).find(|id| *id != leader_id).unwrap();
        // Cut off from both peers, the candidate can never win.
        for peer in (1..=3).filter(|id| *id != candidate_id) {
            router.partition(candidate_id, peer).await;
        }
        let error = nodes[(candidate_id - 1) as usize]
            .campaign_until_leadership_moves(
                Some(leader_id),
                std::time::Instant::now() + Duration::from_millis(800),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, CouncilError::HandoverTimedOut { .. }),
            "{error:?}"
        );
        for node in &nodes {
            node.shutdown().await.unwrap();
        }
    }
}

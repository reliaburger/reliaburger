//! Leader-side rolling upgrade orchestration.
//!
//! The leader walks the fleet: workers (in batches of `parallel`), then
//! non-leader council members one at a time (quorum-gated), then hands
//! leadership to an upgraded member and lets the *new* leader upgrade the
//! former one. All state lives in Raft (`DesiredState.active_upgrade`), so
//! a leader change mid-run is a resume, not a restart.
//!
//! The core is [`step`]: given the current state and a [`NodeControl`]
//! (real HTTP or a test mock), poll reality, take at most one round of
//! actions, and return the updated state. Every step starts by polling
//! `/v1/version` — idempotency by observation is the whole resume story.
//! Directives are idempotent by `upgrade_id`, so re-sending on a later
//! tick (after a crash between send and Raft write) is harmless.

use std::time::{Duration, SystemTime};

use super::types::{
    BinarySource, ClusterUpgradePhase, ClusterUpgradeState, NodeRole, NodeUpgradePhase,
    NodeUpgradeRecord, UpgradeDirection, UpgradeDirective,
};
use super::version::BinaryVersion;

/// A node stuck in Directed/Verifying longer than this is marked failed
/// (covers directives lost to nodes that died mid-swap).
pub const NODE_TIMEOUT: Duration = Duration::from_secs(300);

/// What a node reports when polled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeProbe {
    pub version: BinaryVersion,
    pub healthy: bool,
    pub upgrade_in_flight: bool,
    /// Upgrade ids the node attempted and reverted.
    pub failed_upgrade_ids: Vec<String>,
}

/// Effects the orchestrator performs on nodes. Mocked in unit tests; the
/// real implementation speaks HTTP to each node's API.
pub trait NodeControl: Send + Sync {
    /// Poll `/v1/version` + `/v1/health`. `None` if unreachable.
    fn probe(&self, address: &str) -> impl std::future::Future<Output = Option<NodeProbe>> + Send;

    /// POST an upgrade directive to the node.
    fn direct_upgrade(
        &self,
        address: &str,
        directive: &UpgradeDirective,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;

    /// POST a rollback directive to the node.
    fn direct_rollback(
        &self,
        address: &str,
        version: &BinaryVersion,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// Inputs the driver supplies each tick.
#[derive(Debug, Clone)]
pub struct StepContext {
    /// This (leader) node's id — used to recognise "the leader" records
    /// and to know when leadership has already transferred.
    pub self_node_id: String,
    /// Whether the council can lose one more member without losing quorum.
    /// Computed by the driver from Raft metrics; council upgrades refuse
    /// to proceed while false.
    pub quorum_ok: bool,
    /// Node ids gossip currently reports `Alive`. A node only reaches
    /// [`NodeUpgradePhase::Healthy`] once it is BOTH HTTP-healthy at the
    /// target version AND back in this set — an upgraded process that came
    /// up but never rejoined the mesh is not "done" (UPG2). The leader
    /// (`self_node_id`) is always treated as alive; it is running this code.
    pub gossip_alive_ids: std::collections::BTreeSet<String>,
    /// Wall-clock now (injected so tests control timeouts).
    pub now: SystemTime,
}

impl StepContext {
    /// Is this node back in the gossip mesh? The leader always is.
    fn is_gossip_alive(&self, node_id: &str) -> bool {
        node_id == self.self_node_id || self.gossip_alive_ids.contains(node_id)
    }
}

/// Advance the upgrade by one tick.
///
/// Polls in-flight nodes, sends any due directives, and returns the
/// updated state. The driver persists it to Raft when it changed. Pure
/// with respect to everything except `control`.
pub async fn step<C: NodeControl>(
    mut state: ClusterUpgradeState,
    control: &C,
    context: &StepContext,
) -> ClusterUpgradeState {
    match state.phase.clone() {
        ClusterUpgradePhase::Completed | ClusterUpgradePhase::Paused { .. } => state,

        ClusterUpgradePhase::Preparing => {
            // The binary was verified and pushed to Pickle by whoever
            // started the upgrade (the start handler); nothing to prepare
            // per-node. Move straight into the rolling walk.
            state.phase = ClusterUpgradePhase::UpgradingWorkers;
            state
        }

        ClusterUpgradePhase::UpgradingWorkers => {
            poll_and_drive_group(&mut state, control, context, NodeRole::Worker, true).await;
            if let Some(reason) = first_failure(&state) {
                state.phase = ClusterUpgradePhase::Paused { reason };
            } else if group_done(&state, NodeRole::Worker) {
                state.phase = ClusterUpgradePhase::UpgradingCouncil;
            }
            state
        }

        ClusterUpgradePhase::UpgradingCouncil => {
            // Always poll and detect failures; only START a new council
            // upgrade while quorum has headroom. Gating the whole call on
            // quorum (as an earlier version did) stalled recovery: a dead
            // voter flips `quorum_ok` false, and with no council node yet in
            // flight the failing node was never polled, so the run never
            // paused. Separating the two fixes that.
            poll_and_drive_group(
                &mut state,
                control,
                context,
                NodeRole::Council,
                context.quorum_ok,
            )
            .await;
            if let Some(reason) = first_failure(&state) {
                state.phase = ClusterUpgradePhase::Paused { reason };
            } else if group_done(&state, NodeRole::Council) {
                state.phase = if state.nodes.iter().any(|n| n.role == NodeRole::Leader) {
                    ClusterUpgradePhase::TransferringLeadership
                } else {
                    ClusterUpgradePhase::Completed
                };
            }
            state
        }

        ClusterUpgradePhase::TransferringLeadership => {
            // openraft 0.9 has no graceful leadership transfer, and a naive
            // `trigger().elect()` on a follower cannot win against a live
            // leader's heartbeats (anti-disruption/leader-stickiness). So
            // the leader upgrades IN PLACE: exec is sub-second, a council of
            // >=3 keeps quorum through the bounce, and the returning node
            // re-establishes leadership and finishes the run via the
            // poll-first idempotency below. See the Phase 14 notes in
            // docs/design/agent-bun.md 5.5.
            state.phase = ClusterUpgradePhase::UpgradingLeader;
            state
        }

        ClusterUpgradePhase::UpgradingLeader => {
            poll_and_drive_group(&mut state, control, context, NodeRole::Leader, true).await;
            if let Some(reason) = first_failure(&state) {
                state.phase = ClusterUpgradePhase::Paused { reason };
            } else if group_done(&state, NodeRole::Leader) {
                state.phase = ClusterUpgradePhase::Completed;
            }
            state
        }
    }
}

/// Poll every non-terminal node of `role`; direct new ones up to the
/// group's concurrency budget (workers: `parallel`; everyone else: 1).
///
/// `may_direct_new` gates ONLY the "send a directive to the next Pending
/// node" step (pass 2). Polling in-flight nodes and detecting failures
/// (pass 1) and re-sending to already-Directed nodes (pass 3) always run,
/// whatever `may_direct_new` says — so the quorum gate that stops a council
/// upgrade from *starting* another node cannot also stall failure detection
/// or block recovery of an already-troubled cluster.
async fn poll_and_drive_group<C: NodeControl>(
    state: &mut ClusterUpgradeState,
    control: &C,
    context: &StepContext,
    role: NodeRole,
    may_direct_new: bool,
) {
    let target = state.target_version.clone();
    let direction = state.direction;
    let state_upgrade_id = state.upgrade_id.clone();
    let directive = build_directive(state);
    let budget = if role == NodeRole::Worker {
        state.parallel.max(1) as usize
    } else {
        1
    };

    // Pass 1: poll everything not already in a terminal *failure* state.
    // Healthy nodes are re-polled too: a crash-looping node can look
    // momentarily Healthy at the target between boot attempts (it did exec
    // into the new binary before the fail-boot hook crashed it), and if we
    // latched that and moved on, the next node would upgrade PAST a failure
    // the reverting node is about to report. Re-checking Healthy catches the
    // subsequent revert and pauses the run instead.
    for record in state.nodes.iter_mut().filter(|n| n.role == role) {
        match record.phase {
            NodeUpgradePhase::Failed { .. } | NodeUpgradePhase::RolledBack => continue,
            _ => {}
        }

        let Some(probe) = control.probe(&record.address).await else {
            // Unreachable is normal mid-swap (the exec blip); the timeout
            // below catches nodes that never come back. A Healthy node that
            // briefly stops answering is left Healthy (its check_timeout is a
            // no-op), so a transient blip doesn't undo a completed node.
            check_timeout(record, context.now);
            continue;
        };

        if record.from_version.is_none() {
            record.from_version = Some(probe.version.clone());
        }

        // The node itself says it attempted and reverted this run: failed,
        // regardless of which phase we thought it was in — INCLUDING a node
        // we already marked Healthy on a lucky mid-crash-loop poll. This is
        // the guard that stops the walk advancing past a fail-boot node.
        if probe.failed_upgrade_ids.contains(&state_upgrade_id)
            && matches!(
                record.phase,
                NodeUpgradePhase::Directed
                    | NodeUpgradePhase::Verifying
                    | NodeUpgradePhase::Healthy
            )
        {
            set_phase(
                record,
                NodeUpgradePhase::Failed {
                    reason: format!(
                        "node {} reverted the upgrade (came back on {})",
                        record.node_id, probe.version
                    ),
                },
                context.now,
            );
            continue;
        }

        if probe.version == target && probe.healthy && !probe.upgrade_in_flight {
            // HTTP-healthy at the target is necessary but not sufficient: a
            // node that came back up but never rejoined the gossip mesh is
            // isolated (its workloads take no traffic, its votes don't
            // count). Hold it in Verifying until gossip sees it Alive
            // again; the stuck-node timeout catches one that never does.
            if context.is_gossip_alive(&record.node_id) {
                set_phase(record, NodeUpgradePhase::Healthy, context.now);
            } else if record.phase == NodeUpgradePhase::Directed {
                set_phase(record, NodeUpgradePhase::Verifying, context.now);
            } else {
                check_timeout(record, context.now);
            }
            continue;
        }

        match record.phase {
            NodeUpgradePhase::Directed if probe.upgrade_in_flight => {
                set_phase(record, NodeUpgradePhase::Verifying, context.now);
            }
            NodeUpgradePhase::Verifying if !probe.upgrade_in_flight && probe.version != target => {
                // It went through the swap and came back on the old
                // version: the node reverted itself.
                set_phase(
                    record,
                    NodeUpgradePhase::Failed {
                        reason: format!(
                            "node {} reverted to {} (upgrade to {target} failed)",
                            record.node_id, probe.version
                        ),
                    },
                    context.now,
                );
            }
            NodeUpgradePhase::Healthy if probe.version != target && !probe.upgrade_in_flight => {
                // A node we already marked Healthy is now stably back on the
                // old version: it reverted after a lucky mid-crash-loop poll
                // latched it. Re-fail it so the walk pauses rather than
                // advancing past a node that didn't really upgrade.
                set_phase(
                    record,
                    NodeUpgradePhase::Failed {
                        reason: format!(
                            "node {} reverted to {} after reporting healthy (upgrade to {target} failed)",
                            record.node_id, probe.version
                        ),
                    },
                    context.now,
                );
            }
            _ => check_timeout(record, context.now),
        }
    }

    // A failure detected in pass 1 pauses the run (the caller checks
    // `first_failure` right after this returns). We must NOT direct a new
    // node in the same tick: a node that just failed frees its in-flight
    // slot, and directing the next Pending node here would upgrade it PAST
    // the failure before the pause takes effect — exactly the "only the
    // failing node should have been attempted" invariant the cluster
    // rollback test pins. Any Failed node in the group stops pass 2.
    let group_has_failure = state
        .nodes
        .iter()
        .any(|n| n.role == role && matches!(n.phase, NodeUpgradePhase::Failed { .. }));

    // Pass 2: fill the concurrency budget with Pending nodes — but only
    // when allowed to start a new one (the council quorum gate) and no node
    // in this group has failed this run. When `may_direct_new` is false or a
    // failure is present we leave Pending nodes untouched; pass 1 above has
    // already polled and failed-detected everything in flight.
    let in_flight = state
        .nodes
        .iter()
        .filter(|n| {
            n.role == role
                && matches!(
                    n.phase,
                    NodeUpgradePhase::Directed | NodeUpgradePhase::Verifying
                )
        })
        .count();
    let mut slots = if may_direct_new && !group_has_failure {
        budget.saturating_sub(in_flight)
    } else {
        0
    };
    let mut directed_this_tick = Vec::new();

    for record in state.nodes.iter_mut().filter(|n| n.role == role) {
        if slots == 0 {
            break;
        }
        if record.phase != NodeUpgradePhase::Pending {
            continue;
        }
        let sent = match direction {
            UpgradeDirection::Upgrade => control.direct_upgrade(&record.address, &directive).await,
            UpgradeDirection::Rollback => control.direct_rollback(&record.address, &target).await,
        };
        match sent {
            Ok(()) => {
                set_phase(record, NodeUpgradePhase::Directed, context.now);
                directed_this_tick.push(record.node_id.clone());
                slots -= 1;
            }
            Err(reason) => {
                set_phase(
                    record,
                    NodeUpgradePhase::Failed {
                        reason: format!("directive to {} refused: {reason}", record.node_id),
                    },
                    context.now,
                );
            }
        }
    }

    // Pass 3: re-send to Directed nodes from PREVIOUS ticks that still
    // don't show the upgrade in flight (lost directive — e.g. leader
    // crashed between the Raft write and the send). Idempotent by
    // upgrade_id, so the worst case is a polite no-op.
    if direction == UpgradeDirection::Upgrade {
        for record in state.nodes.iter().filter(|n| n.role == role) {
            if record.phase == NodeUpgradePhase::Directed
                && !directed_this_tick.contains(&record.node_id)
            {
                let _ = control.direct_upgrade(&record.address, &directive).await;
            }
        }
    }
}

fn build_directive(state: &ClusterUpgradeState) -> UpgradeDirective {
    UpgradeDirective {
        upgrade_id: state.upgrade_id.clone(),
        target_version: state.target_version.clone(),
        binary_sha256: state.binary_sha256.clone(),
        embedded_signature: state.embedded_signature.clone(),
        external_signature: state.external_signature.clone(),
        source: BinarySource::Pickle {
            registry_address: state.registry_address.clone(),
        },
    }
}

fn set_phase(record: &mut NodeUpgradeRecord, phase: NodeUpgradePhase, now: SystemTime) {
    record.phase = phase;
    record.since = Some(now);
}

fn check_timeout(record: &mut NodeUpgradeRecord, now: SystemTime) {
    if !matches!(
        record.phase,
        NodeUpgradePhase::Directed | NodeUpgradePhase::Verifying
    ) {
        return;
    }
    let expired = record
        .since
        .and_then(|since| now.duration_since(since).ok())
        .map(|elapsed| elapsed > NODE_TIMEOUT)
        .unwrap_or(false);
    if expired {
        record.phase = NodeUpgradePhase::Failed {
            reason: format!(
                "node {} did not reach the target version within {}s",
                record.node_id,
                NODE_TIMEOUT.as_secs()
            ),
        };
        record.since = Some(now);
    }
}

fn first_failure(state: &ClusterUpgradeState) -> Option<String> {
    state.nodes.iter().find_map(|n| match &n.phase {
        NodeUpgradePhase::Failed { reason } => Some(reason.clone()),
        _ => None,
    })
}

fn group_done(state: &ClusterUpgradeState, role: NodeRole) -> bool {
    state
        .nodes
        .iter()
        .filter(|n| n.role == role)
        .all(|n| n.phase == NodeUpgradePhase::Healthy)
}

/// Un-pause a paused upgrade: failed nodes go back to Pending (they are
/// re-polled and re-directed; a node already at target skips straight to
/// Healthy), and the phase re-enters the earliest unfinished group.
///
/// The run gets a FRESH upgrade id: nodes refuse to re-attempt an id they
/// already reverted (the crash-loop-forever guard), so a retry must look
/// like a new run to them.
pub fn resume(mut state: ClusterUpgradeState) -> ClusterUpgradeState {
    if !matches!(state.phase, ClusterUpgradePhase::Paused { .. }) {
        return state;
    }
    state.upgrade_id = format!("{}-retry", state.upgrade_id);
    for record in &mut state.nodes {
        if matches!(record.phase, NodeUpgradePhase::Failed { .. }) {
            record.phase = NodeUpgradePhase::Pending;
            record.since = None;
        }
    }
    state.phase = if !group_done(&state, NodeRole::Worker) {
        ClusterUpgradePhase::UpgradingWorkers
    } else if !group_done(&state, NodeRole::Council) {
        ClusterUpgradePhase::UpgradingCouncil
    } else {
        ClusterUpgradePhase::TransferringLeadership
    };
    state
}

// ---------------------------------------------------------------------------
// The real NodeControl (HTTP) and the driver loop
// ---------------------------------------------------------------------------

/// HTTP implementation of [`NodeControl`], talking to each node's bun API.
#[derive(Clone)]
pub struct HttpNodeControl {
    /// Scheme + client for peer API calls (HTTPS + CA trust under mTLS).
    http: crate::cluster::ClusterHttp,
    /// Cluster service token, presented so peers accept us as the system
    /// principal. `None` when the cluster runs without API auth.
    service_token: Option<String>,
}

impl HttpNodeControl {
    /// Plaintext control plane (mTLS off).
    pub fn new(service_token: Option<String>) -> Self {
        Self::with_http(service_token, crate::cluster::ClusterHttp::plaintext())
    }

    /// Control plane over the given scheme/client (HTTPS under mTLS).
    pub fn with_http(service_token: Option<String>, http: crate::cluster::ClusterHttp) -> Self {
        Self {
            http,
            service_token,
        }
    }

    fn with_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.service_token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }
}

impl NodeControl for HttpNodeControl {
    async fn probe(&self, address: &str) -> Option<NodeProbe> {
        let version_response = self
            .http
            .client()
            .get(self.http.url(address, "/v1/version"))
            .send()
            .await
            .ok()?;
        let value: serde_json::Value = version_response.json().await.ok()?;
        let version: BinaryVersion = value["version"].as_str()?.parse().ok()?;
        let upgrade_in_flight = value["upgrade_in_flight"].as_bool().unwrap_or(false);
        let failed_upgrade_ids = value["failed_upgrade_ids"]
            .as_array()
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| id.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let healthy = matches!(
            self.http
                .client()
                .get(self.http.url(address, "/v1/health"))
                .send()
                .await,
            Ok(response) if response.status().is_success()
        );

        Some(NodeProbe {
            version,
            healthy,
            upgrade_in_flight,
            failed_upgrade_ids,
        })
    }

    async fn direct_upgrade(
        &self,
        address: &str,
        directive: &UpgradeDirective,
    ) -> Result<(), String> {
        let response = self
            .with_auth(
                self.http
                    .client()
                    .post(self.http.url(address, "/v1/upgrade/apply"))
                    .json(directive),
            )
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(format!("{status}: {body}"))
        }
    }

    async fn direct_rollback(&self, address: &str, version: &BinaryVersion) -> Result<(), String> {
        let response = self
            .with_auth(
                self.http
                    .client()
                    .post(self.http.url(address, "/v1/upgrade/rollback"))
                    .json(&serde_json::json!({ "version": version })),
            )
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(format!("{status}: {body}"))
        }
    }
}

/// Can the council afford to have one voter mid-restart?
///
/// Quorum protection is about *live* voters, not configured ones. With `v`
/// voters configured and `live` of them currently reachable, taking one
/// more down during an upgrade leaves `live - 1` up, which must still form
/// a quorum of the configured set: `live - 1 >= v/2 + 1`.
///
/// The old check counted configured voters and assumed all were live. That
/// let an upgrade proceed on a 3-voter council with one voter *already*
/// dead: upgrading a second one drops live voters to 1, below the quorum of
/// 2, and the cluster loses its leader for the duration of the swap (or
/// forever, if the swap fails). Counting live voters refuses that.
///
/// A single-voter council (`v == 1`) is a special case: the leader
/// upgrading itself is the whole cluster restarting, there is no quorum to
/// protect, so it always proceeds.
///
/// `live` counts voters that gossip reports `Alive`. A voter absent from
/// the alive set — dead, suspect or left — does not count. The leader
/// itself is always counted live: it is running this very code.
pub fn live_quorum_headroom_ok(configured_voters: usize, live_voters: usize) -> bool {
    if configured_voters <= 1 {
        return true;
    }
    // Quorum of the configured set. Integer `v/2 + 1` is the majority.
    let quorum = configured_voters / 2 + 1;
    // After taking one more voter down for its swap, `live_voters - 1` remain.
    live_voters.saturating_sub(1) >= quorum
}

/// Count configured voters, and how many of them gossip reports `Alive`.
///
/// Voter ids are Raft `u64`s; gossip identifies nodes by name. We bridge
/// them with the same stable `raft_id_from_name` hash the whole cluster
/// uses, so a voter counts as live iff some alive gossip member hashes to
/// its id. `self_id` (the leader running this) is always counted: it need
/// not appear in its own gossip snapshot to be alive.
///
/// Returns `(configured_voters, live_voters)`.
fn count_live_voters(
    council: &crate::council::CouncilNode,
    membership: &[crate::mustard::membership::MembershipSnapshot],
    self_id: u64,
) -> (usize, usize) {
    use crate::cluster::identity::raft_id_from_name;
    use crate::mustard::state::NodeState;

    let metrics = council.metrics().borrow().clone();
    let voters: std::collections::BTreeSet<u64> =
        metrics.membership_config.membership().voter_ids().collect();

    let alive_ids: std::collections::BTreeSet<u64> = membership
        .iter()
        .filter(|m| m.state == NodeState::Alive)
        .map(|m| raft_id_from_name(&m.node_id.0))
        .collect();

    let live = voters
        .iter()
        .filter(|id| **id == self_id || alive_ids.contains(id))
        .count();
    (voters.len(), live)
}

/// The long-lived orchestration loop, spawned once per cluster-mode bun.
///
/// Dormant unless this node is the Raft leader AND an upgrade is active.
/// Each tick: run [`step`], persist the state to Raft if it changed, and
/// clear it (archiving to history) on completion. Because the state lives
/// in Raft, a leadership change mid-run just moves which node's copy of
/// this loop does the work.
pub async fn run_orchestrator(
    council: std::sync::Arc<crate::council::CouncilNode>,
    control: HttpNodeControl,
    self_node_id: String,
    membership_rx: tokio::sync::watch::Receiver<
        Vec<crate::mustard::membership::MembershipSnapshot>,
    >,
    cancel: tokio_util::sync::CancellationToken,
) {
    use crate::cluster::identity::raft_id_from_name;
    use crate::council::types::RaftRequest;

    let self_raft_id = raft_id_from_name(&self_node_id);
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        if !council.is_leader().await {
            continue;
        }
        let Some(upgrade) = council.desired_state().await.active_upgrade else {
            continue;
        };
        if matches!(
            upgrade.phase,
            ClusterUpgradePhase::Paused { .. } | ClusterUpgradePhase::Completed
        ) {
            // Completed states are normally cleared below; this handles a
            // leader that crashed between phases.
            if upgrade.phase == ClusterUpgradePhase::Completed {
                let _ = council
                    .write(RaftRequest::UpgradeClear {
                        upgrade_id: upgrade.upgrade_id.clone(),
                    })
                    .await;
            }
            continue;
        }

        // Count live voters against the current gossip snapshot: an upgrade
        // that would drop the vote keeping quorum is refused (UPG1).
        let (configured_voters, live_voters, gossip_alive_ids) = {
            let membership = membership_rx.borrow();
            let (configured, live) = count_live_voters(&council, &membership, self_raft_id);
            let alive: std::collections::BTreeSet<String> = membership
                .iter()
                .filter(|m| m.state == crate::mustard::state::NodeState::Alive)
                .map(|m| m.node_id.0.clone())
                .collect();
            (configured, live, alive)
        };
        let context = StepContext {
            self_node_id: self_node_id.clone(),
            quorum_ok: live_quorum_headroom_ok(configured_voters, live_voters),
            gossip_alive_ids,
            now: SystemTime::now(),
        };
        let next = step(upgrade.clone(), &control, &context).await;

        if next.phase == ClusterUpgradePhase::Completed {
            println!(
                "bun: cluster upgrade {} to {} complete",
                next.upgrade_id, next.target_version
            );
            // Persist the FINAL state before clearing, so the archived
            // history entry shows the completed walk (every node Healthy),
            // not whatever mid-walk snapshot Raft last saw.
            let _ = council
                .write(RaftRequest::UpgradeUpdate {
                    state: Box::new(next.clone()),
                })
                .await;
            let _ = council
                .write(RaftRequest::UpgradeClear {
                    upgrade_id: next.upgrade_id.clone(),
                })
                .await;
        } else if next != upgrade {
            if let ClusterUpgradePhase::Paused { reason } = &next.phase {
                eprintln!("bun: cluster upgrade {} paused: {reason}", next.upgrade_id);
            }
            let _ = council
                .write(RaftRequest::UpgradeUpdate {
                    state: Box::new(next),
                })
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    fn v(s: &str) -> BinaryVersion {
        s.parse().unwrap()
    }

    /// Mock cluster: per-address node states + recorded actions.
    #[derive(Default)]
    struct MockControl {
        nodes: Mutex<HashMap<String, NodeProbe>>,
        directives: Mutex<Vec<String>>,
        rollbacks: Mutex<Vec<String>>,
    }

    impl MockControl {
        fn set(&self, address: &str, version: &str, healthy: bool, in_flight: bool) {
            self.nodes.lock().unwrap().insert(
                address.to_string(),
                NodeProbe {
                    version: v(version),
                    healthy,
                    upgrade_in_flight: in_flight,
                    failed_upgrade_ids: Vec::new(),
                },
            );
        }

        fn set_failed(&self, address: &str, version: &str, failed_id: &str) {
            self.nodes.lock().unwrap().insert(
                address.to_string(),
                NodeProbe {
                    version: v(version),
                    healthy: true,
                    upgrade_in_flight: false,
                    failed_upgrade_ids: vec![failed_id.to_string()],
                },
            );
        }

        fn directed(&self) -> Vec<String> {
            self.directives.lock().unwrap().clone()
        }
    }

    impl NodeControl for MockControl {
        async fn probe(&self, address: &str) -> Option<NodeProbe> {
            self.nodes.lock().unwrap().get(address).cloned()
        }

        async fn direct_upgrade(
            &self,
            address: &str,
            _directive: &UpgradeDirective,
        ) -> Result<(), String> {
            self.directives.lock().unwrap().push(address.to_string());
            Ok(())
        }

        async fn direct_rollback(
            &self,
            address: &str,
            _version: &BinaryVersion,
        ) -> Result<(), String> {
            self.rollbacks.lock().unwrap().push(address.to_string());
            Ok(())
        }
    }

    fn record(id: &str, role: NodeRole, phase: NodeUpgradePhase) -> NodeUpgradeRecord {
        NodeUpgradeRecord {
            node_id: id.to_string(),
            address: format!("addr-{id}"),
            role,
            from_version: None,
            phase,
            since: None,
        }
    }

    fn cluster_state(nodes: Vec<NodeUpgradeRecord>, parallel: u32) -> ClusterUpgradeState {
        ClusterUpgradeState {
            upgrade_id: "up-1".to_string(),
            target_version: v("0.2.0"),
            binary_sha256: "abc".to_string(),
            embedded_signature: "sig".to_string(),
            external_signature: Some("ext".to_string()),
            parallel,
            direction: UpgradeDirection::Upgrade,
            phase: ClusterUpgradePhase::UpgradingWorkers,
            registry_address: "leader:5050".to_string(),
            nodes,
        }
    }

    /// A context in which every node the tests use is gossip-alive, so the
    /// gossip-rejoin gate (UPG2) never blocks the transitions those tests
    /// are about. The dedicated rejoin tests build their own contexts.
    fn context() -> StepContext {
        context_alive(["w1", "w2", "w3", "c1", "c2", "leader"])
    }

    fn context_alive<const N: usize>(alive: [&str; N]) -> StepContext {
        StepContext {
            self_node_id: "leader".to_string(),
            quorum_ok: true,
            gossip_alive_ids: alive.iter().map(|s| s.to_string()).collect(),
            now: SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn workers_upgrade_in_batches_of_parallel() {
        let control = MockControl::default();
        for worker in ["w1", "w2", "w3"] {
            control.set(&format!("addr-{worker}"), "0.1.0", true, false);
        }
        let state = cluster_state(
            vec![
                record("w1", NodeRole::Worker, NodeUpgradePhase::Pending),
                record("w2", NodeRole::Worker, NodeUpgradePhase::Pending),
                record("w3", NodeRole::Worker, NodeUpgradePhase::Pending),
            ],
            2,
        );

        let state = step(state, &control, &context()).await;

        // Exactly `parallel` nodes directed; the third waits its turn.
        let directed: Vec<_> = state
            .nodes
            .iter()
            .filter(|n| n.phase == NodeUpgradePhase::Directed)
            .map(|n| n.node_id.clone())
            .collect();
        assert_eq!(directed, vec!["w1", "w2"]);
        assert_eq!(state.nodes[2].phase, NodeUpgradePhase::Pending);
    }

    #[tokio::test]
    async fn workers_advance_to_council_when_all_healthy() {
        let control = MockControl::default();
        control.set("addr-w1", "0.2.0", true, false);
        control.set("addr-c1", "0.1.0", true, false);
        let state = cluster_state(
            vec![
                record("w1", NodeRole::Worker, NodeUpgradePhase::Verifying),
                record("c1", NodeRole::Council, NodeUpgradePhase::Pending),
            ],
            1,
        );

        let state = step(state, &control, &context()).await;

        assert_eq!(state.nodes[0].phase, NodeUpgradePhase::Healthy);
        assert_eq!(state.phase, ClusterUpgradePhase::UpgradingCouncil);
    }

    #[tokio::test]
    async fn node_reverting_pauses_the_upgrade() {
        let control = MockControl::default();
        // w1 went through the swap and came back on the OLD version.
        control.set("addr-w1", "0.1.0", true, false);
        let mut state = cluster_state(
            vec![record("w1", NodeRole::Worker, NodeUpgradePhase::Verifying)],
            1,
        );
        state.nodes[0].from_version = Some(v("0.1.0"));

        let state = step(state, &control, &context()).await;

        assert!(matches!(
            state.nodes[0].phase,
            NodeUpgradePhase::Failed { .. }
        ));
        assert!(matches!(state.phase, ClusterUpgradePhase::Paused { .. }));
    }

    #[tokio::test]
    async fn stuck_node_times_out_and_pauses() {
        let control = MockControl::default();
        // Unreachable node (no probe response).
        let mut state = cluster_state(
            vec![record("w1", NodeRole::Worker, NodeUpgradePhase::Directed)],
            1,
        );
        state.nodes[0].since = Some(SystemTime::now() - (NODE_TIMEOUT + Duration::from_secs(1)));

        let state = step(state, &control, &context()).await;

        assert!(matches!(
            state.nodes[0].phase,
            NodeUpgradePhase::Failed { .. }
        ));
        assert!(matches!(state.phase, ClusterUpgradePhase::Paused { .. }));
    }

    #[tokio::test]
    async fn council_members_upgrade_one_at_a_time() {
        let control = MockControl::default();
        control.set("addr-c1", "0.1.0", true, false);
        control.set("addr-c2", "0.1.0", true, false);
        let mut state = cluster_state(
            vec![
                record("c1", NodeRole::Council, NodeUpgradePhase::Pending),
                record("c2", NodeRole::Council, NodeUpgradePhase::Pending),
            ],
            4, // parallel applies to workers only
        );
        state.phase = ClusterUpgradePhase::UpgradingCouncil;

        let state = step(state, &control, &context()).await;

        assert_eq!(state.nodes[0].phase, NodeUpgradePhase::Directed);
        assert_eq!(state.nodes[1].phase, NodeUpgradePhase::Pending);
    }

    #[tokio::test]
    async fn quorum_risk_refuses_council_upgrade() {
        let control = MockControl::default();
        control.set("addr-c1", "0.1.0", true, false);
        let mut state = cluster_state(
            vec![record("c1", NodeRole::Council, NodeUpgradePhase::Pending)],
            1,
        );
        state.phase = ClusterUpgradePhase::UpgradingCouncil;

        let mut ctx = context();
        ctx.quorum_ok = false;
        let state = step(state, &control, &ctx).await;

        assert_eq!(state.nodes[0].phase, NodeUpgradePhase::Pending);
        assert!(control.directed().is_empty());
    }

    #[tokio::test]
    async fn a_failed_node_pauses_before_a_sibling_upgrades() {
        // The cluster-test scenario at the unit level: the first council
        // node is a fail-boot node. It must pause the run BEFORE the second
        // council node is ever directed, so "only the failing node was
        // attempted" holds — no node upgrades past a failure.
        let control = MockControl::default();
        let mut state = cluster_state(
            vec![
                record("c1", NodeRole::Council, NodeUpgradePhase::Pending),
                record("c2", NodeRole::Council, NodeUpgradePhase::Pending),
            ],
            1,
        );
        state.phase = ClusterUpgradePhase::UpgradingCouncil;

        // Tick 1: c1 is on the old version, healthy. It gets directed; c2
        // must stay Pending (one at a time).
        control.set("addr-c1", "0.1.0", true, false);
        control.set("addr-c2", "0.1.0", true, false);
        let state = step(state, &control, &context()).await;
        assert_eq!(state.nodes[0].phase, NodeUpgradePhase::Directed);
        assert_eq!(state.nodes[1].phase, NodeUpgradePhase::Pending);

        // Tick 2: c1 reports the upgrade in flight (crash-looping); c2 still
        // Pending, still not directed.
        control.set("addr-c1", "0.1.0", true, true);
        let state = step(state, &control, &context()).await;
        assert_eq!(state.nodes[0].phase, NodeUpgradePhase::Verifying);
        assert_eq!(state.nodes[1].phase, NodeUpgradePhase::Pending);

        // Tick 3: c1 reverted — back on the old version, reporting our id in
        // failed_upgrade_ids. The run pauses; c2 was never directed.
        control.set_failed("addr-c1", "0.1.0", "up-1");
        let state = step(state, &control, &context()).await;
        assert!(matches!(
            state.nodes[0].phase,
            NodeUpgradePhase::Failed { .. }
        ));
        assert_eq!(state.nodes[1].phase, NodeUpgradePhase::Pending);
        assert!(matches!(state.phase, ClusterUpgradePhase::Paused { .. }));
        // c1 was the only node ever directed.
        assert_eq!(control.directed(), vec!["addr-c1"]);
    }

    #[tokio::test]
    async fn a_node_that_reverts_after_reporting_healthy_is_re_failed() {
        // Guard against the "lucky mid-crash-loop poll" race: if a node was
        // marked Healthy but a later poll shows it reverted our upgrade id,
        // it must go back to Failed and pause the run — not stay Healthy and
        // let the walk advance past a node that didn't really upgrade.
        let control = MockControl::default();
        let mut state = cluster_state(
            vec![record("c1", NodeRole::Council, NodeUpgradePhase::Healthy)],
            1,
        );
        state.phase = ClusterUpgradePhase::UpgradingCouncil;
        control.set_failed("addr-c1", "0.1.0", "up-1");

        let state = step(state, &control, &context()).await;

        assert!(matches!(
            state.nodes[0].phase,
            NodeUpgradePhase::Failed { .. }
        ));
        assert!(matches!(state.phase, ClusterUpgradePhase::Paused { .. }));
    }

    #[tokio::test]
    async fn a_healthy_node_reverting_to_old_version_is_re_failed() {
        // Same guard, but the node quietly reverted without reporting a
        // failed id: it is stably back on the old version. Still a failure.
        let control = MockControl::default();
        let mut state = cluster_state(
            vec![record("c1", NodeRole::Council, NodeUpgradePhase::Healthy)],
            1,
        );
        state.phase = ClusterUpgradePhase::UpgradingCouncil;
        control.set("addr-c1", "0.1.0", true, false);

        let state = step(state, &control, &context()).await;

        assert!(matches!(
            state.nodes[0].phase,
            NodeUpgradePhase::Failed { .. }
        ));
        assert!(matches!(state.phase, ClusterUpgradePhase::Paused { .. }));
    }

    #[tokio::test]
    async fn a_healthy_node_with_a_transient_blip_stays_healthy() {
        // A completed node that briefly stops answering (probe None) must
        // NOT be demoted — only a stable revert fails it.
        let control = MockControl::default();
        let mut state = cluster_state(
            vec![record("c1", NodeRole::Council, NodeUpgradePhase::Healthy)],
            1,
        );
        state.phase = ClusterUpgradePhase::UpgradingCouncil;
        // No entry for addr-c1 → probe returns None (unreachable blip).

        let state = step(state, &control, &context()).await;

        assert_eq!(state.nodes[0].phase, NodeUpgradePhase::Healthy);
        // A single healthy council node is done → advances past the group.
        assert_ne!(state.phase, ClusterUpgradePhase::UpgradingCouncil);
    }

    #[tokio::test]
    async fn low_quorum_still_detects_a_failed_council_node_and_pauses() {
        // The scenario that stalled a real cluster: a council node is in
        // flight and reverts itself. Meanwhile a voter has died, so
        // `quorum_ok` is false. The quorum gate must NOT stop us polling the
        // in-flight node and pausing on its failure — otherwise the run
        // stalls in UpgradingCouncil forever and the cluster never recovers.
        let control = MockControl::default();
        // c1 went through the swap and came back on the OLD version: reverted.
        control.set("addr-c1", "0.1.0", true, false);
        let mut state = cluster_state(
            vec![record("c1", NodeRole::Council, NodeUpgradePhase::Verifying)],
            1,
        );
        state.phase = ClusterUpgradePhase::UpgradingCouncil;
        state.nodes[0].from_version = Some(v("0.1.0"));

        let mut ctx = context();
        ctx.quorum_ok = false;
        let state = step(state, &control, &ctx).await;

        assert!(matches!(
            state.nodes[0].phase,
            NodeUpgradePhase::Failed { .. }
        ));
        assert!(matches!(state.phase, ClusterUpgradePhase::Paused { .. }));
    }

    #[tokio::test]
    async fn low_quorum_does_not_direct_a_new_council_node_but_still_polls() {
        // The other side of the gate: with quorum at risk and a Pending
        // council node, we must not START its upgrade (no directive sent),
        // yet a healthy in-flight peer is still polled to completion.
        let control = MockControl::default();
        control.set("addr-c1", "0.2.0", true, false); // in flight, now at target
        control.set("addr-c2", "0.1.0", true, false); // pending, must stay put
        let mut state = cluster_state(
            vec![
                record("c1", NodeRole::Council, NodeUpgradePhase::Verifying),
                record("c2", NodeRole::Council, NodeUpgradePhase::Pending),
            ],
            1,
        );
        state.phase = ClusterUpgradePhase::UpgradingCouncil;

        let mut ctx = context();
        ctx.quorum_ok = false;
        let state = step(state, &control, &ctx).await;

        // c1 was polled and completed; c2 was NOT directed (quorum preserved).
        assert_eq!(state.nodes[0].phase, NodeUpgradePhase::Healthy);
        assert_eq!(state.nodes[1].phase, NodeUpgradePhase::Pending);
        assert!(control.directed().is_empty());
    }

    #[tokio::test]
    async fn council_done_advances_to_leader_phase() {
        let control = MockControl::default();
        let mut state = cluster_state(
            vec![
                record("c1", NodeRole::Council, NodeUpgradePhase::Healthy),
                record("leader", NodeRole::Leader, NodeUpgradePhase::Pending),
            ],
            1,
        );
        state.phase = ClusterUpgradePhase::UpgradingCouncil;
        control.set("addr-c1", "0.2.0", true, false);
        control.set("addr-leader", "0.1.0", true, false);

        // Council done -> transfer phase (a one-tick pass-through since
        // openraft 0.9 can't gracefully transfer; see the phase comment).
        let state = step(state, &control, &context()).await;
        assert_eq!(state.phase, ClusterUpgradePhase::TransferringLeadership);
        assert!(control.directed().is_empty());

        // Transfer phase collapses straight to UpgradingLeader.
        let state = step(state, &control, &context()).await;
        assert_eq!(state.phase, ClusterUpgradePhase::UpgradingLeader);
    }

    #[tokio::test]
    async fn leader_upgrades_itself_in_place_last() {
        let control = MockControl::default();
        control.set("addr-leader", "0.1.0", true, false);
        let mut state = cluster_state(
            vec![
                record("c1", NodeRole::Council, NodeUpgradePhase::Healthy),
                record("leader", NodeRole::Leader, NodeUpgradePhase::Pending),
            ],
            1,
        );
        state.phase = ClusterUpgradePhase::UpgradingLeader;

        // The current leader directs its own upgrade (over its local API).
        let state = step(state, &control, &context()).await;
        assert_eq!(state.nodes[1].phase, NodeUpgradePhase::Directed);
        assert_eq!(control.directed(), vec!["addr-leader"]);
    }

    #[tokio::test]
    async fn resume_skips_nodes_already_at_target() {
        let control = MockControl::default();
        // A "Pending" node that is in fact already upgraded (a resume after
        // a pause, or a leader crash mid-walk).
        control.set("addr-w1", "0.2.0", true, false);
        let state = cluster_state(
            vec![record("w1", NodeRole::Worker, NodeUpgradePhase::Pending)],
            1,
        );

        let state = step(state, &control, &context()).await;

        assert_eq!(state.nodes[0].phase, NodeUpgradePhase::Healthy);
        assert!(control.directed().is_empty());
    }

    #[tokio::test]
    async fn already_upgraded_cluster_completes() {
        let control = MockControl::default();
        control.set("addr-w1", "0.2.0", true, false);
        let state = cluster_state(
            vec![record("w1", NodeRole::Worker, NodeUpgradePhase::Pending)],
            1,
        );

        // Tick 1: worker observed healthy at target -> council phase (no
        // council nodes -> straight through on tick 2).
        let state = step(state, &control, &context()).await;
        assert_eq!(state.phase, ClusterUpgradePhase::UpgradingCouncil);
        let state = step(state, &control, &context()).await;
        assert_eq!(state.phase, ClusterUpgradePhase::Completed);
    }

    #[tokio::test]
    async fn rollback_sends_rollback_directives() {
        let control = MockControl::default();
        control.set("addr-w1", "0.2.0", true, false);
        let mut state = cluster_state(
            vec![record("w1", NodeRole::Worker, NodeUpgradePhase::Pending)],
            1,
        );
        state.direction = UpgradeDirection::Rollback;
        state.target_version = v("0.1.0");

        let state = step(state, &control, &context()).await;

        assert_eq!(state.nodes[0].phase, NodeUpgradePhase::Directed);
        assert_eq!(*control.rollbacks.lock().unwrap(), vec!["addr-w1"]);
        assert!(control.directed().is_empty());
    }

    #[tokio::test]
    async fn node_reported_revert_marks_failed_and_pauses() {
        let control = MockControl::default();
        // The node is back, healthy, on the old version — and its history
        // says it reverted OUR upgrade id (even though the leader only
        // ever saw it as Directed; the crash loop was too fast to observe).
        control.set_failed("addr-w1", "0.1.0", "up-1");
        let state = cluster_state(
            vec![record("w1", NodeRole::Worker, NodeUpgradePhase::Directed)],
            1,
        );

        let state = step(state, &control, &context()).await;

        assert!(matches!(
            state.nodes[0].phase,
            NodeUpgradePhase::Failed { .. }
        ));
        assert!(matches!(state.phase, ClusterUpgradePhase::Paused { .. }));
    }

    #[tokio::test]
    async fn resume_issues_a_fresh_attempt_id() {
        let mut state = cluster_state(
            vec![record(
                "w1",
                NodeRole::Worker,
                NodeUpgradePhase::Failed {
                    reason: "boom".to_string(),
                },
            )],
            1,
        );
        state.phase = ClusterUpgradePhase::Paused {
            reason: "boom".to_string(),
        };

        let resumed = resume(state);

        // Nodes refuse ids they already reverted, so the retry must not
        // reuse the old one.
        assert_eq!(resumed.upgrade_id, "up-1-retry");
    }

    #[tokio::test]
    async fn healthy_but_not_in_gossip_does_not_complete() {
        let control = MockControl::default();
        // w1 is HTTP-healthy at the target version, but gossip has NOT seen
        // it rejoin the mesh — it must not be marked Healthy yet.
        control.set("addr-w1", "0.2.0", true, false);
        let state = cluster_state(
            vec![record("w1", NodeRole::Worker, NodeUpgradePhase::Verifying)],
            1,
        );

        let ctx = context_alive(["leader"]); // w1 absent from gossip
        let state = step(state, &control, &ctx).await;

        assert_eq!(state.nodes[0].phase, NodeUpgradePhase::Verifying);
        assert_eq!(state.phase, ClusterUpgradePhase::UpgradingWorkers);
    }

    #[tokio::test]
    async fn rejoining_gossip_completes_the_node() {
        let control = MockControl::default();
        control.set("addr-w1", "0.2.0", true, false);
        let state = cluster_state(
            vec![record("w1", NodeRole::Worker, NodeUpgradePhase::Verifying)],
            1,
        );

        // Same probe, but now gossip reports w1 Alive: it completes.
        let ctx = context_alive(["leader", "w1"]);
        let state = step(state, &control, &ctx).await;

        assert_eq!(state.nodes[0].phase, NodeUpgradePhase::Healthy);
        assert_eq!(state.phase, ClusterUpgradePhase::UpgradingCouncil);
    }

    #[test]
    fn quorum_headroom_counts_live_voters_not_configured() {
        // Three configured voters, all live: one may go down (2 remain >= 2).
        assert!(live_quorum_headroom_ok(3, 3));
        // Three configured, but one already dead: upgrading a second would
        // leave 1 live, below the quorum of 2 — refuse.
        assert!(!live_quorum_headroom_ok(3, 2));
        // Five configured, one dead (4 live): taking one more leaves 3 >= 3.
        assert!(live_quorum_headroom_ok(5, 4));
        // Five configured, two dead (3 live): one more leaves 2 < 3 — refuse.
        assert!(!live_quorum_headroom_ok(5, 3));
    }

    #[test]
    fn single_voter_council_always_has_headroom() {
        // A one-voter council is the whole cluster; the leader upgrading
        // itself has no quorum to protect.
        assert!(live_quorum_headroom_ok(1, 1));
        assert!(live_quorum_headroom_ok(0, 0));
    }

    #[test]
    fn two_voter_council_never_has_headroom() {
        // v == 2, quorum 2: losing either member loses quorum, even fully
        // live. (The roadmap discourages 2-voter councils for this reason.)
        assert!(!live_quorum_headroom_ok(2, 2));
        assert!(!live_quorum_headroom_ok(2, 1));
    }

    #[tokio::test]
    async fn resume_returns_failed_nodes_to_pending() {
        let mut state = cluster_state(
            vec![
                record("w1", NodeRole::Worker, NodeUpgradePhase::Healthy),
                record(
                    "w2",
                    NodeRole::Worker,
                    NodeUpgradePhase::Failed {
                        reason: "boom".to_string(),
                    },
                ),
            ],
            1,
        );
        state.phase = ClusterUpgradePhase::Paused {
            reason: "boom".to_string(),
        };

        let state = resume(state);

        assert_eq!(state.phase, ClusterUpgradePhase::UpgradingWorkers);
        assert_eq!(state.nodes[1].phase, NodeUpgradePhase::Pending);
        // The healthy node is untouched.
        assert_eq!(state.nodes[0].phase, NodeUpgradePhase::Healthy);
    }
}

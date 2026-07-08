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
    /// Wall-clock now (injected so tests control timeouts).
    pub now: SystemTime,
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
            poll_and_drive_group(&mut state, control, context, NodeRole::Worker).await;
            if let Some(reason) = first_failure(&state) {
                state.phase = ClusterUpgradePhase::Paused { reason };
            } else if group_done(&state, NodeRole::Worker) {
                state.phase = ClusterUpgradePhase::UpgradingCouncil;
            }
            state
        }

        ClusterUpgradePhase::UpgradingCouncil => {
            if context.quorum_ok || group_in_flight(&state, NodeRole::Council) > 0 {
                poll_and_drive_group(&mut state, control, context, NodeRole::Council).await;
            }
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
            poll_and_drive_group(&mut state, control, context, NodeRole::Leader).await;
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
async fn poll_and_drive_group<C: NodeControl>(
    state: &mut ClusterUpgradeState,
    control: &C,
    context: &StepContext,
    role: NodeRole,
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

    // Pass 1: poll everything already in flight (and Pending nodes, to
    // skip ones that are somehow already at target — the resume story).
    for record in state.nodes.iter_mut().filter(|n| n.role == role) {
        match record.phase {
            NodeUpgradePhase::Healthy
            | NodeUpgradePhase::Failed { .. }
            | NodeUpgradePhase::RolledBack => continue,
            _ => {}
        }

        let Some(probe) = control.probe(&record.address).await else {
            // Unreachable is normal mid-swap (the exec blip); the timeout
            // below catches nodes that never come back.
            check_timeout(record, context.now);
            continue;
        };

        if record.from_version.is_none() {
            record.from_version = Some(probe.version.clone());
        }

        // The node itself says it attempted and reverted this run: failed,
        // regardless of which phase we thought it was in.
        if probe.failed_upgrade_ids.contains(&state_upgrade_id)
            && matches!(
                record.phase,
                NodeUpgradePhase::Directed | NodeUpgradePhase::Verifying
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
            set_phase(record, NodeUpgradePhase::Healthy, context.now);
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
            _ => check_timeout(record, context.now),
        }
    }

    // Pass 2: fill the concurrency budget with Pending nodes.
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
    let mut slots = budget.saturating_sub(in_flight);
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

fn group_in_flight(state: &ClusterUpgradeState, role: NodeRole) -> usize {
    state
        .nodes
        .iter()
        .filter(|n| {
            n.role == role
                && matches!(
                    n.phase,
                    NodeUpgradePhase::Directed | NodeUpgradePhase::Verifying
                )
        })
        .count()
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
    client: reqwest::Client,
    /// Cluster service token, presented so peers accept us as the system
    /// principal. `None` when the cluster runs without API auth.
    service_token: Option<String>,
}

impl HttpNodeControl {
    pub fn new(service_token: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            client,
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
            .client
            .get(format!("http://{address}/v1/version"))
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
            self.client
                .get(format!("http://{address}/v1/health"))
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
                self.client
                    .post(format!("http://{address}/v1/upgrade/apply"))
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
                self.client
                    .post(format!("http://{address}/v1/upgrade/rollback"))
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
/// With `v` voters, quorum is `v/2 + 1`; taking one down leaves `v - 1`
/// reachable, which still forms a quorum iff `v >= 3`. A single-voter
/// council (`v == 1`) is also fine: the leader upgrading itself is the
/// whole cluster restarting, there is no quorum to protect. `v == 2`
/// (which the roadmap discourages) cannot lose either member.
///
/// Members that are ALREADY down aren't visible in the membership config;
/// the per-node health polling in [`step`] covers the observable part.
async fn quorum_headroom_ok(council: &crate::council::CouncilNode) -> bool {
    let metrics = council.metrics().borrow().clone();
    let voters = metrics.membership_config.membership().voter_ids().count();
    voters >= 3 || voters == 1
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
    cancel: tokio_util::sync::CancellationToken,
) {
    use crate::council::types::RaftRequest;

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

        let context = StepContext {
            self_node_id: self_node_id.clone(),
            quorum_ok: quorum_headroom_ok(&council).await,
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

    fn context() -> StepContext {
        StepContext {
            self_node_id: "leader".to_string(),
            quorum_ok: true,
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

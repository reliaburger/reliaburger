//! Rolling an OS update across the fleet (docs/plans/2026-10-01-plan-
//! appliance-product.md, W6).
//!
//! The leader walks the nodes one at a time, workers first, then council
//! members (only while the council can spare a voter), and itself last.
//! For each node it cordons it so the scheduler moves its workloads away,
//! waits for them to go, then tells the node to stage the target version
//! (`POST /v1/os/stage`). The node downloads and checks the release, lets
//! `systemd-sysupdate` write it into its spare slot and reboots. The leader
//! then polls `/v1/version` until the node reports the target version and
//! healthy, and moves on. A node that boots back into its old version (the
//! new one failed its boot checks three times and systemd-boot fell back)
//! or doesn't come back in time pauses the rollout for the operator.
//!
//! The whole rollout lives in Raft (`DesiredState::os_rollout`), so when the
//! leader reboots itself, the next leader picks up where the record says.
//! [`step`] is pure apart from the [`OsControl`] calls, like the bun upgrade
//! orchestrator's, so every transition is tested without a cluster.

use std::collections::BTreeSet;
use std::future::Future;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use crate::upgrade::orchestrator::DirectiveError;

/// How long the leader waits for a cordoned node's workloads to move before
/// it updates the node anyway. A reboot is no worse than the node failing,
/// which the scheduler handles; waiting just makes it gentler.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long a node may take from the stage directive to reporting the new
/// version: downloading about a gigabyte, writing it, and up to three boot
/// attempts before systemd-boot falls back.
pub const UPDATE_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// How long the leader keeps re-sending a stage directive that fails
/// transiently before it pauses the rollout.
pub const DIRECTIVE_RETRY_WINDOW: Duration = Duration::from_secs(120);

/// A cluster-wide OS rollout, as stored in Raft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsRollout {
    /// Unique per run; a resume gets a new one.
    pub rollout_id: String,
    /// The OS version every node should end up on (`YYYY.WW.N`).
    pub target: String,
    /// Where nodes read the signed channel that names the release.
    pub channel_url: String,
    pub phase: OsRolloutPhase,
    /// In the order they're updated: workers, council, the leader last.
    pub nodes: Vec<OsNodeRecord>,
}

/// Where the rollout as a whole is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OsRolloutPhase {
    Running,
    /// Stopped for the operator: `relish os resume` or `abort`.
    Paused {
        reason: String,
    },
    Completed,
    Aborted {
        reason: String,
    },
}

impl OsRolloutPhase {
    /// Whether the rollout has ended, one way or the other.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Aborted { .. })
    }
}

/// One node's part in a rollout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsNodeRecord {
    pub node_id: String,
    /// The node's API address (`host:port`).
    pub address: String,
    /// A council voter: updated only while quorum can spare it.
    pub council: bool,
    /// The OS version the node had when the rollout started.
    pub from_version: String,
    pub phase: OsNodePhase,
    /// Unix seconds when `phase` began.
    pub since: u64,
    /// Unix seconds of the first transient directive failure in a row.
    pub retrying_since: Option<u64>,
}

/// Where one node is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OsNodePhase {
    /// Not started.
    Pending,
    /// Cordoned: the scheduler is moving its workloads away.
    Draining,
    /// Told to stage the target; downloading, writing or rebooting.
    Updating,
    /// On the target version and healthy.
    Done,
    /// Already on the target version when the rollout started.
    Skipped,
    Failed {
        reason: String,
    },
}

impl OsRollout {
    /// Whether the scheduler should keep workloads off `node_id`: it's
    /// draining or updating.
    pub fn is_node_cordoned(&self, node_id: &str) -> bool {
        self.phase == OsRolloutPhase::Running
            && self.nodes.iter().any(|node| {
                node.node_id == node_id
                    && matches!(node.phase, OsNodePhase::Draining | OsNodePhase::Updating)
            })
    }
}

/// What the leader tells a node to do (`POST /v1/os/stage`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsDirective {
    pub rollout_id: String,
    pub version: String,
    pub channel_url: String,
}

/// A node's OS update state, as it reports it in `/v1/version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OsUpdateState {
    /// Nothing in progress.
    #[default]
    Idle,
    /// Downloading, checking and writing `target` into the spare slot.
    Staging { target: String },
    /// Staged; rebooting into `target`.
    Rebooting { target: String },
    /// The last update to `target` failed.
    Failed { target: String, reason: String },
}

/// What the leader learns by polling a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsProbe {
    /// `None` on a node that isn't an appliance.
    pub os_version: Option<String>,
    pub healthy: bool,
    pub update: OsUpdateState,
}

/// The leader's view of each tick, from outside the rollout record.
#[derive(Debug, Clone)]
pub struct RolloutContext {
    /// Whether the council can spare a voter right now.
    pub quorum_ok: bool,
    /// Nodes that still have workloads the scheduler can move.
    pub undrained: BTreeSet<String>,
    /// Unix seconds.
    pub now: u64,
}

/// How the leader reaches nodes: real HTTP, or a mock in tests.
pub trait OsControl {
    fn probe(&self, address: &str) -> impl Future<Output = Option<OsProbe>> + Send;
    fn direct_stage(
        &self,
        address: &str,
        directive: &OsDirective,
    ) -> impl Future<Output = Result<(), DirectiveError>> + Send;
}

/// Advance the rollout by at most one node transition.
pub async fn step<C: OsControl>(
    mut rollout: OsRollout,
    control: &C,
    context: &RolloutContext,
) -> OsRollout {
    if rollout.phase != OsRolloutPhase::Running {
        return rollout;
    }
    let Some(index) = rollout
        .nodes
        .iter()
        .position(|node| !matches!(node.phase, OsNodePhase::Done | OsNodePhase::Skipped))
    else {
        rollout.phase = OsRolloutPhase::Completed;
        return rollout;
    };
    let directive = OsDirective {
        rollout_id: rollout.rollout_id.clone(),
        version: rollout.target.clone(),
        channel_url: rollout.channel_url.clone(),
    };
    let target = rollout.target.clone();
    let node = &mut rollout.nodes[index];
    let failure = match node.phase.clone() {
        OsNodePhase::Pending => {
            if !node.council || context.quorum_ok {
                enter(node, OsNodePhase::Draining, context.now);
            }
            None
        }
        OsNodePhase::Draining => {
            let waited = context.now.saturating_sub(node.since);
            if context.undrained.contains(&node.node_id) && waited < DRAIN_TIMEOUT.as_secs() {
                None
            } else if node.council && !context.quorum_ok {
                // A voter dropped out while this one drained: hold it
                // cordoned until the council can spare it again.
                None
            } else {
                direct(node, control, &directive, context.now).await
            }
        }
        OsNodePhase::Updating => watch(node, control, &target, context.now).await,
        OsNodePhase::Failed { reason } => Some(reason),
        OsNodePhase::Done | OsNodePhase::Skipped => None,
    };
    if let Some(reason) = failure {
        let node = &mut rollout.nodes[index];
        let reason = format!("{}: {reason}", node.node_id);
        enter(
            node,
            OsNodePhase::Failed {
                reason: reason.clone(),
            },
            context.now,
        );
        rollout.phase = OsRolloutPhase::Paused { reason };
    }
    rollout
}

fn enter(node: &mut OsNodeRecord, phase: OsNodePhase, now: u64) {
    node.phase = phase;
    node.since = now;
    node.retrying_since = None;
}

/// Send the stage directive. Returns why the node failed, if it did.
async fn direct<C: OsControl>(
    node: &mut OsNodeRecord,
    control: &C,
    directive: &OsDirective,
    now: u64,
) -> Option<String> {
    match control.direct_stage(&node.address, directive).await {
        Ok(()) => {
            enter(node, OsNodePhase::Updating, now);
            None
        }
        Err(DirectiveError::Refused(reason)) => Some(format!("refused the update: {reason}")),
        Err(DirectiveError::Transient(reason)) => {
            let first = *node.retrying_since.get_or_insert(now);
            (now.saturating_sub(first) >= DIRECTIVE_RETRY_WINDOW.as_secs())
                .then(|| format!("unreachable for the update: {reason}"))
        }
    }
}

/// Poll an updating node. Returns why it failed, if it did.
async fn watch<C: OsControl>(
    node: &mut OsNodeRecord,
    control: &C,
    target: &str,
    now: u64,
) -> Option<String> {
    let timed_out = now.saturating_sub(node.since) >= UPDATE_TIMEOUT.as_secs();
    let Some(probe) = control.probe(&node.address).await else {
        // Rebooting, most likely.
        return timed_out
            .then(|| format!("not back after {} minutes", UPDATE_TIMEOUT.as_secs() / 60));
    };
    if probe.os_version.as_deref() == Some(target) && probe.healthy {
        enter(node, OsNodePhase::Done, now);
        return None;
    }
    if let OsUpdateState::Failed {
        target: failed,
        reason,
    } = &probe.update
        && failed == target
    {
        return Some(reason.clone());
    }
    timed_out.then(|| {
        format!(
            "still on {} after {} minutes",
            probe.os_version.as_deref().unwrap_or("an unknown version"),
            UPDATE_TIMEOUT.as_secs() / 60
        )
    })
}

/// Order the nodes for a rollout: workers, then council members, then the
/// leader. Nodes already on `target` are skipped.
pub fn plan(
    rollout_id: &str,
    target: &str,
    channel_url: &str,
    leader: &str,
    nodes: Vec<(String, String, bool, String)>,
    now: u64,
) -> OsRollout {
    let mut nodes: Vec<OsNodeRecord> = nodes
        .into_iter()
        .map(|(node_id, address, council, from_version)| OsNodeRecord {
            phase: if from_version == target {
                OsNodePhase::Skipped
            } else {
                OsNodePhase::Pending
            },
            node_id,
            address,
            council,
            from_version,
            since: now,
            retrying_since: None,
        })
        .collect();
    nodes.sort_by_key(|node| (node.node_id == leader, node.council, node.node_id.clone()));
    OsRollout {
        rollout_id: rollout_id.to_string(),
        target: target.to_string(),
        channel_url: channel_url.to_string(),
        phase: OsRolloutPhase::Running,
        nodes,
    }
}

/// Resume a paused rollout under a new id: failed nodes start again from
/// the beginning.
pub fn resume(mut rollout: OsRollout, now: u64) -> OsRollout {
    rollout.rollout_id = format!("{}-retry", rollout.rollout_id);
    rollout.phase = OsRolloutPhase::Running;
    for node in &mut rollout.nodes {
        if matches!(node.phase, OsNodePhase::Failed { .. }) {
            enter(node, OsNodePhase::Pending, now);
        }
    }
    rollout
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A fleet whose nodes answer from a table, recording directives.
    #[derive(Default)]
    struct Fleet {
        probes: Mutex<HashMap<String, Option<OsProbe>>>,
        refusals: Mutex<HashMap<String, DirectiveError>>,
        directed: Mutex<Vec<String>>,
    }

    impl Fleet {
        fn answers(&self, address: &str, probe: Option<OsProbe>) {
            self.probes
                .lock()
                .unwrap()
                .insert(address.to_string(), probe);
        }
    }

    impl OsControl for Fleet {
        async fn probe(&self, address: &str) -> Option<OsProbe> {
            self.probes.lock().unwrap().get(address).cloned().flatten()
        }

        async fn direct_stage(
            &self,
            address: &str,
            _directive: &OsDirective,
        ) -> Result<(), DirectiveError> {
            if let Some(error) = self.refusals.lock().unwrap().get(address) {
                return Err(error.clone());
            }
            self.directed.lock().unwrap().push(address.to_string());
            Ok(())
        }
    }

    fn on(version: &str) -> Option<OsProbe> {
        Some(OsProbe {
            os_version: Some(version.into()),
            healthy: true,
            update: OsUpdateState::Idle,
        })
    }

    fn three_nodes() -> OsRollout {
        plan(
            "r1",
            "2026.42.0",
            "https://example/os-channel.json",
            "n1",
            vec![
                (
                    "n1".into(),
                    "10.0.0.1:9117".into(),
                    true,
                    "2026.41.0".into(),
                ),
                (
                    "n2".into(),
                    "10.0.0.2:9117".into(),
                    true,
                    "2026.41.0".into(),
                ),
                (
                    "w1".into(),
                    "10.0.0.3:9117".into(),
                    false,
                    "2026.41.0".into(),
                ),
                (
                    "w2".into(),
                    "10.0.0.4:9117".into(),
                    false,
                    "2026.42.0".into(),
                ),
            ],
            0,
        )
    }

    fn context(now: u64) -> RolloutContext {
        RolloutContext {
            quorum_ok: true,
            undrained: BTreeSet::new(),
            now,
        }
    }

    fn phases(rollout: &OsRollout) -> Vec<(&str, &OsNodePhase)> {
        rollout
            .nodes
            .iter()
            .map(|n| (n.node_id.as_str(), &n.phase))
            .collect()
    }

    #[test]
    fn workers_go_first_the_leader_last_and_nodes_on_target_are_skipped() {
        let rollout = three_nodes();
        let order: Vec<_> = rollout.nodes.iter().map(|n| n.node_id.as_str()).collect();
        assert_eq!(order, ["w1", "w2", "n2", "n1"]);
        assert_eq!(rollout.nodes[1].phase, OsNodePhase::Skipped);
    }

    #[tokio::test]
    async fn each_node_drains_updates_and_comes_back_before_the_next() {
        let fleet = Fleet::default();
        let mut rollout = three_nodes();
        rollout = step(rollout, &fleet, &context(0)).await;
        assert_eq!(phases(&rollout)[0], ("w1", &OsNodePhase::Draining));
        assert!(rollout.is_node_cordoned("w1"));

        // Workloads still on it: wait.
        let mut busy = context(10);
        busy.undrained.insert("w1".into());
        rollout = step(rollout, &fleet, &busy).await;
        assert_eq!(rollout.nodes[0].phase, OsNodePhase::Draining);

        rollout = step(rollout, &fleet, &context(20)).await;
        assert_eq!(rollout.nodes[0].phase, OsNodePhase::Updating);
        assert_eq!(*fleet.directed.lock().unwrap(), ["10.0.0.3:9117"]);

        // Rebooting: unreachable, then still on the old version.
        rollout = step(rollout, &fleet, &context(30)).await;
        fleet.answers("10.0.0.3:9117", on("2026.41.0"));
        rollout = step(rollout, &fleet, &context(40)).await;
        assert_eq!(rollout.nodes[0].phase, OsNodePhase::Updating);
        assert_eq!(phases(&rollout)[2].1, &OsNodePhase::Pending, "n2 waits");

        fleet.answers("10.0.0.3:9117", on("2026.42.0"));
        rollout = step(rollout, &fleet, &context(50)).await;
        assert_eq!(rollout.nodes[0].phase, OsNodePhase::Done);
        assert!(!rollout.is_node_cordoned("w1"));

        // w2 is skipped; n2 starts next.
        rollout = step(rollout, &fleet, &context(60)).await;
        assert_eq!(phases(&rollout)[2], ("n2", &OsNodePhase::Draining));
    }

    #[tokio::test]
    async fn a_voter_waits_while_the_council_cant_spare_it() {
        let fleet = Fleet::default();
        let mut rollout = three_nodes();
        rollout.nodes[0].phase = OsNodePhase::Done;
        let mut short = context(0);
        short.quorum_ok = false;
        rollout = step(rollout, &fleet, &short).await;
        assert_eq!(rollout.nodes[2].phase, OsNodePhase::Pending);
        rollout = step(rollout, &fleet, &context(5)).await;
        assert_eq!(rollout.nodes[2].phase, OsNodePhase::Draining);
        // Quorum lost while it drained: it stays cordoned, undirected.
        short.now = 10;
        rollout = step(rollout, &fleet, &short).await;
        assert_eq!(rollout.nodes[2].phase, OsNodePhase::Draining);
        assert!(fleet.directed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_node_that_wont_drain_is_updated_after_the_drain_timeout() {
        let fleet = Fleet::default();
        let mut rollout = step(three_nodes(), &fleet, &context(0)).await;
        let mut busy = context(DRAIN_TIMEOUT.as_secs());
        busy.undrained.insert("w1".into());
        rollout = step(rollout, &fleet, &busy).await;
        assert_eq!(rollout.nodes[0].phase, OsNodePhase::Updating);
    }

    #[tokio::test]
    async fn a_fallback_reported_by_the_node_pauses_the_rollout() {
        let fleet = Fleet::default();
        let mut rollout = three_nodes();
        rollout.nodes[0].phase = OsNodePhase::Updating;
        fleet.answers(
            "10.0.0.3:9117",
            Some(OsProbe {
                os_version: Some("2026.41.0".into()),
                healthy: true,
                update: OsUpdateState::Failed {
                    target: "2026.42.0".into(),
                    reason: "booted 2026.41.0 instead: 2026.42.0 failed its boot checks".into(),
                },
            }),
        );
        rollout = step(rollout, &fleet, &context(100)).await;
        assert!(
            matches!(&rollout.phase, OsRolloutPhase::Paused { reason } if reason.starts_with("w1: booted 2026.41.0")),
            "{:?}",
            rollout.phase
        );
        assert!(
            !rollout.is_node_cordoned("w1"),
            "a paused rollout cordons nothing"
        );
        // Paused rollouts don't move.
        let again = step(rollout.clone(), &fleet, &context(200)).await;
        assert_eq!(again, rollout);

        let resumed = resume(rollout, 300);
        assert_eq!(resumed.rollout_id, "r1-retry");
        assert_eq!(resumed.phase, OsRolloutPhase::Running);
        assert_eq!(resumed.nodes[0].phase, OsNodePhase::Pending);
    }

    #[tokio::test]
    async fn a_node_that_never_comes_back_pauses_the_rollout() {
        let fleet = Fleet::default();
        let mut rollout = three_nodes();
        rollout.nodes[0].phase = OsNodePhase::Updating;
        rollout = step(rollout, &fleet, &context(UPDATE_TIMEOUT.as_secs() - 1)).await;
        assert_eq!(rollout.phase, OsRolloutPhase::Running);
        rollout = step(rollout, &fleet, &context(UPDATE_TIMEOUT.as_secs())).await;
        assert!(matches!(rollout.phase, OsRolloutPhase::Paused { .. }));
    }

    #[tokio::test]
    async fn a_refusal_pauses_at_once_and_unreachability_after_a_while() {
        let fleet = Fleet::default();
        fleet.refusals.lock().unwrap().insert(
            "10.0.0.3:9117".into(),
            DirectiveError::Transient("connection refused".into()),
        );
        let mut rollout = three_nodes();
        rollout.nodes[0].phase = OsNodePhase::Draining;
        rollout = step(rollout, &fleet, &context(1000)).await;
        assert_eq!(rollout.phase, OsRolloutPhase::Running);
        rollout = step(
            rollout,
            &fleet,
            &context(1000 + DIRECTIVE_RETRY_WINDOW.as_secs()),
        )
        .await;
        assert!(matches!(rollout.phase, OsRolloutPhase::Paused { .. }));

        let fleet = Fleet::default();
        fleet.refusals.lock().unwrap().insert(
            "10.0.0.3:9117".into(),
            DirectiveError::Refused("409: not an appliance".into()),
        );
        let mut rollout = three_nodes();
        rollout.nodes[0].phase = OsNodePhase::Draining;
        rollout = step(rollout, &fleet, &context(1000)).await;
        assert!(
            matches!(&rollout.phase, OsRolloutPhase::Paused { reason } if reason.contains("not an appliance"))
        );
    }

    #[tokio::test]
    async fn the_rollout_completes_when_every_node_is_done_or_skipped() {
        let fleet = Fleet::default();
        let mut rollout = three_nodes();
        for node in &mut rollout.nodes {
            if node.phase == OsNodePhase::Pending {
                node.phase = OsNodePhase::Done;
            }
        }
        rollout = step(rollout, &fleet, &context(0)).await;
        assert_eq!(rollout.phase, OsRolloutPhase::Completed);
        assert!(rollout.phase.is_terminal());
    }

    #[test]
    fn update_states_serialise_tagged() {
        let json = serde_json::to_value(OsUpdateState::Rebooting {
            target: "2026.42.0".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"state": "rebooting", "target": "2026.42.0"})
        );
        assert_eq!(
            serde_json::from_value::<OsUpdateState>(serde_json::json!({"state": "idle"})).unwrap(),
            OsUpdateState::Idle
        );
    }
}

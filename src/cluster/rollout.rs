//! Node-by-node rollouts (#692, decision D10-C).
//!
//! Every node rolls its own share of an app with the app's `[deploy]`
//! table. If the leader handed a new spec to every node at once, every
//! node would roll at once, and an app with one replica per node would be
//! down everywhere for the length of a replacement. So the leader releases
//! a spec change one node at a time: the first node (by node id) that
//! doesn't run the new spec yet gets it, and every node after it keeps the
//! last spec that rolled out everywhere until that node reports its share
//! healthy on the new one.
//!
//! The leader keeps no rollout progress of its own. It reads progress from
//! the nodes' reports (which version each node's placement reconciler last
//! converged on, and whether its instances are healthy), so a new leader
//! carries on where the old one stopped. The only rollout state in the
//! council is [`AppRollouts`]: the specs that finished rolling out
//! everywhere, and the spec whose rollout failed.
//!
//! Everything here is pure, so each rule is a unit test.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::app::AppSpec;
use crate::council::types::DesiredState;
use crate::meat::deploy_types::{AppRollouts, rollout_fingerprint};
use crate::meat::{AppId, NodeId};
use crate::reporting::aggregator::AggregatedState;
use crate::reporting::types::ReportHealthStatus;

/// A node's report that its rollout of an app failed, as its placement
/// reconciler posts it to the leader's `/v1/rollouts/failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolloutFailureReport {
    /// Name of the app.
    pub app_name: String,
    /// Namespace the app belongs to.
    pub namespace: String,
    /// The node whose rollout failed.
    pub node: String,
    /// [`rollout_fingerprint`] of the spec that failed.
    pub fingerprint: String,
    /// What went wrong, as the node's deploy reported it.
    pub reason: String,
}

/// What one node's reports say about its share of an app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeShare {
    /// No instance of the app runs on the node.
    Empty,
    /// The node's reconciler finished deploying this version
    /// ([`rollout_fingerprint`]) and the node runs `healthy` instances of it.
    Converged {
        /// The version the node converged on.
        fingerprint: String,
        /// Every instance it should run is running and healthy.
        healthy: bool,
    },
    /// Instances run, but the node hasn't reported a finished deploy: one is
    /// in flight, failed, or the node's report is missing or stale.
    Unsettled,
}

/// What the leader hands one node for an app.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Release<'a> {
    /// The app's current spec.
    Desired,
    /// The rollout hasn't reached this node: it keeps running the version it
    /// runs. A node with no instance yet starts this spec, the last one that
    /// rolled out everywhere.
    Hold(&'a AppSpec),
}

/// Decide, for every node an app is placed on, whether it gets the app's
/// current spec or holds.
///
/// Nodes are taken in node-id order. A node already converged on the
/// current spec gets it, and if it isn't healthy yet the rollout waits on
/// it. The first node that hasn't converged gets the current spec and the
/// rest hold, so exactly one node rolls at a time. A node with nothing of the
/// app running starts the current spec straight away: a first placement
/// costs no capacity, so it isn't gated. Nor is anything when the council
/// has no earlier fully rolled-out spec, or the current spec is that spec
/// (a scale change).
///
/// When the current spec's rollout failed, no node is handed it that isn't
/// already running it: those nodes hold, and the reconciler leaves alone
/// whatever they run. (With no earlier spec to hold them on, they hold on
/// the failed one, which a held node only starts as a first placement.)
pub fn plan_release<'a>(
    desired: &'a AppSpec,
    rollouts: Option<&'a AppRollouts>,
    shares: &BTreeMap<NodeId, NodeShare>,
) -> BTreeMap<NodeId, Release<'a>> {
    let fingerprint = rollout_fingerprint(desired);
    let failed = rollouts
        .and_then(|rollouts| rollouts.failed_for(&fingerprint))
        .is_some();
    let previous = rollouts
        .and_then(AppRollouts::last_released)
        .filter(|previous| rollout_fingerprint(previous) != fingerprint);
    let previous = match previous {
        Some(previous) => previous,
        None if failed => desired,
        None => {
            return shares
                .keys()
                .map(|node| (node.clone(), Release::Desired))
                .collect();
        }
    };
    let mut rolling = false;
    shares
        .iter()
        .map(|(node, share)| {
            let release = match share {
                NodeShare::Converged {
                    fingerprint: converged,
                    healthy,
                } if *converged == fingerprint => {
                    // Released already. Until it's healthy, the next node
                    // waits on it.
                    rolling |= !healthy;
                    Release::Desired
                }
                NodeShare::Empty if !failed => Release::Desired,
                _ if failed || rolling => Release::Hold(previous),
                _ => {
                    rolling = true;
                    Release::Desired
                }
            };
            (node.clone(), release)
        })
        .collect()
}

/// What `reports` say about `node`'s share of `app_id`, which the leader
/// placed `placed` replicas of there.
pub fn node_share(
    reports: &AggregatedState,
    node: &NodeId,
    app_id: &AppId,
    placed: usize,
) -> NodeShare {
    let Some(report) = reports
        .reports
        .get(node)
        .filter(|_| reports.report_is_fresh(node))
    else {
        return NodeShare::Unsettled;
    };
    let instances: Vec<_> = report
        .running_apps
        .iter()
        .filter(|app| app.app_name == app_id.name && app.namespace == app_id.namespace)
        .collect();
    let converged = report
        .rollouts
        .iter()
        .find(|rollout| rollout.app_name == app_id.name && rollout.namespace == app_id.namespace);
    match converged {
        Some(rollout) => NodeShare::Converged {
            fingerprint: rollout.fingerprint.clone(),
            healthy: instances.len() == placed
                && instances
                    .iter()
                    .all(|app| app.health_status == ReportHealthStatus::Healthy),
        },
        None if instances.is_empty() => NodeShare::Empty,
        None => NodeShare::Unsettled,
    }
}

/// Every node `app_id` is placed on, with its share as `reports` tell it.
pub fn app_shares(
    desired: &DesiredState,
    reports: &AggregatedState,
    app_id: &AppId,
) -> BTreeMap<NodeId, NodeShare> {
    let mut placed: BTreeMap<NodeId, usize> = BTreeMap::new();
    for placement in desired.scheduling.get(app_id).into_iter().flatten() {
        *placed.entry(placement.node_id.clone()).or_default() += 1;
    }
    placed
        .into_iter()
        .map(|(node, count)| {
            let share = node_share(reports, &node, app_id, count);
            (node, share)
        })
        .collect()
}

/// The apps whose current spec now runs healthy on every node they're
/// placed on and isn't yet their newest rollback target: the leader
/// commits a `RolloutReleased` for each.
pub fn newly_released(desired: &DesiredState, reports: &AggregatedState) -> Vec<(AppId, AppSpec)> {
    let mut released: Vec<(AppId, AppSpec)> = desired
        .apps
        .iter()
        .filter(|(app_id, _)| !desired.stopped_apps.contains(*app_id))
        .filter_map(|(app_id, spec)| {
            let fingerprint = rollout_fingerprint(spec);
            let already = desired
                .rollouts
                .get(app_id)
                .and_then(AppRollouts::last_released)
                .is_some_and(|last| rollout_fingerprint(last) == fingerprint);
            if already {
                return None;
            }
            let shares = app_shares(desired, reports, app_id);
            let everywhere = !shares.is_empty()
                && shares.values().all(|share| {
                    matches!(share, NodeShare::Converged { fingerprint: converged, healthy: true }
                        if *converged == fingerprint)
                });
            everywhere.then(|| (app_id.clone(), spec.clone()))
        })
        .collect();
    released.sort_by(|a, b| a.0.cmp(&b.0));
    released
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meat::deploy_types::RolloutFailure;

    fn spec(image: &str) -> AppSpec {
        toml::from_str(&format!("image = {image:?}\nreplicas = 3\n")).unwrap()
    }

    /// A spec that lives for the whole test, for plans that borrow it.
    fn leaked(image: &str) -> &'static AppSpec {
        Box::leak(Box::new(spec(image)))
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name)
    }

    fn on(image: &str, healthy: bool) -> NodeShare {
        NodeShare::Converged {
            fingerprint: rollout_fingerprint(&spec(image)),
            healthy,
        }
    }

    fn released(images: &[&str]) -> AppRollouts {
        let mut rollouts = AppRollouts::default();
        for image in images {
            rollouts.release(&spec(image));
        }
        rollouts
    }

    fn shares(entries: &[(&str, NodeShare)]) -> BTreeMap<NodeId, NodeShare> {
        entries
            .iter()
            .map(|(name, share)| (node(name), share.clone()))
            .collect()
    }

    /// Which nodes get the desired spec, in node order.
    fn desired_nodes(plan: &BTreeMap<NodeId, Release<'_>>) -> Vec<String> {
        plan.iter()
            .filter(|(_, release)| **release == Release::Desired)
            .map(|(node, _)| node.0.clone())
            .collect()
    }

    #[test]
    fn leader_releases_a_spec_change_to_one_node_at_a_time() {
        let rollouts = released(&["web:v1"]);
        let v1 = spec("web:v1");
        let plan = plan_release(
            leaked("web:v2"),
            Some(&rollouts),
            &shares(&[
                ("node-1", on("web:v1", true)),
                ("node-2", on("web:v1", true)),
                ("node-3", on("web:v1", true)),
            ]),
        );
        assert_eq!(desired_nodes(&plan), ["node-1"]);
        assert_eq!(plan[&node("node-2")], Release::Hold(&v1));
        assert_eq!(plan[&node("node-3")], Release::Hold(&v1));
    }

    #[test]
    fn next_node_waits_until_the_previous_share_is_healthy() {
        let rollouts = released(&["web:v1"]);
        let desired = spec("web:v2");
        // node-1 has converged on v2 but its replica isn't healthy yet.
        let waiting = plan_release(
            &desired,
            Some(&rollouts),
            &shares(&[
                ("node-1", on("web:v2", false)),
                ("node-2", on("web:v1", true)),
                ("node-3", on("web:v1", true)),
            ]),
        );
        assert_eq!(desired_nodes(&waiting), ["node-1"]);
        // A deploy still in flight holds the next node back too.
        let in_flight = plan_release(
            &desired,
            Some(&rollouts),
            &shares(&[
                ("node-1", NodeShare::Unsettled),
                ("node-2", on("web:v1", true)),
            ]),
        );
        assert_eq!(desired_nodes(&in_flight), ["node-1"]);
        // Healthy now: node-2 is released, node-3 still waits.
        let next = plan_release(
            &desired,
            Some(&rollouts),
            &shares(&[
                ("node-1", on("web:v2", true)),
                ("node-2", on("web:v1", true)),
                ("node-3", on("web:v1", true)),
            ]),
        );
        assert_eq!(desired_nodes(&next), ["node-1", "node-2"]);
    }

    #[test]
    fn new_leader_resumes_node_by_node_release_from_reports() {
        // A new leader has nothing but the council's record and the nodes'
        // reports: node-1 and node-2 are done, so node-3 is next.
        let rollouts = released(&["web:v1"]);
        let plan = plan_release(
            leaked("web:v2"),
            Some(&rollouts),
            &shares(&[
                ("node-1", on("web:v2", true)),
                ("node-2", on("web:v2", true)),
                ("node-3", on("web:v1", true)),
            ]),
        );
        assert_eq!(desired_nodes(&plan), ["node-1", "node-2", "node-3"]);
    }

    #[test]
    fn first_placement_is_not_gated() {
        // A brand-new app has no fully rolled-out spec: every node starts.
        let fresh = plan_release(
            leaked("web:v1"),
            None,
            &shares(&[("node-1", NodeShare::Empty), ("node-2", NodeShare::Empty)]),
        );
        assert_eq!(desired_nodes(&fresh), ["node-1", "node-2"]);
        // A node that gains its first replica mid-rollout starts the new
        // spec without waiting: it takes no capacity away.
        let rollouts = released(&["web:v1"]);
        let midway = plan_release(
            leaked("web:v2"),
            Some(&rollouts),
            &shares(&[
                ("node-1", NodeShare::Unsettled),
                ("node-2", on("web:v1", true)),
                ("node-3", NodeShare::Empty),
            ]),
        );
        assert_eq!(desired_nodes(&midway), ["node-1", "node-3"]);
        // A scale change keeps the version, so it isn't gated either.
        let mut scaled = spec("web:v1");
        scaled.replicas = crate::config::Replicas::Fixed(5);
        let scale = plan_release(
            &scaled,
            Some(&rollouts),
            &shares(&[
                ("node-1", on("web:v1", true)),
                ("node-2", on("web:v1", false)),
                ("node-3", NodeShare::Empty),
            ]),
        );
        assert_eq!(desired_nodes(&scale), ["node-1", "node-2", "node-3"]);
    }

    #[test]
    fn rollout_failed_stops_retries_of_that_spec() {
        let mut rollouts = released(&["web:v1"]);
        rollouts.failed = Some(RolloutFailure {
            fingerprint: rollout_fingerprint(&spec("web:v2")),
            node: "node-2".into(),
            reason: "health check failed".into(),
            at_unix_ms: 1,
        });
        let v1 = spec("web:v1");
        let plan = plan_release(
            leaked("web:v2"),
            Some(&rollouts),
            &shares(&[
                ("node-1", on("web:v2", true)),
                ("node-2", NodeShare::Unsettled),
                ("node-3", on("web:v1", true)),
                ("node-4", NodeShare::Empty),
            ]),
        );
        // node-1 already runs v2 healthy and keeps it; nobody else gets it,
        // not even a node placing its first replica.
        assert_eq!(desired_nodes(&plan), ["node-1"]);
        for held in ["node-2", "node-3", "node-4"] {
            assert_eq!(plan[&node(held)], Release::Hold(&v1), "{held}");
        }
        // With no earlier spec that rolled out everywhere, nodes still hold.
        let v2 = spec("web:v2");
        let first_only = AppRollouts {
            released: Vec::new(),
            failed: rollouts.failed.clone(),
        };
        let plan = plan_release(
            &v2,
            Some(&first_only),
            &shares(&[
                ("node-1", on("web:v2", true)),
                ("node-2", NodeShare::Unsettled),
            ]),
        );
        assert_eq!(desired_nodes(&plan), ["node-1"]);
        assert_eq!(plan[&node("node-2")], Release::Hold(&v2));
    }
}

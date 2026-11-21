//! What maintenance on one node would affect: the node detail's
//! Maintenance tab, worked out from what the TUI already fetched.
//!
//! It answers "what if this node went away?", for a reboot, an OS update
//! or a decommission. It's read-only, and it's a forecast from the last
//! refresh, not a plan the scheduler has agreed to.

use std::collections::BTreeMap;

use crate::bun::agent::{ClusterInstanceStatus, CouncilStatus};
use crate::bun::summary::CouncilSection;
use crate::relish::nodes_cmd::NodeRow;

/// What losing the node does to one app with an instance on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppRisk {
    /// No ready replica is left anywhere else: the app stops serving.
    GoesDown,
    /// One ready replica is left elsewhere: it keeps serving, with no
    /// spare.
    LosesRedundancy,
    /// Two or more ready replicas are left elsewhere.
    Keeps,
}

/// One app with an instance on the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppImpact {
    pub app: String,
    pub namespace: String,
    /// Instances on this node, ready or not.
    pub here: usize,
    /// Ready instances on the other nodes that are still up.
    pub ready_elsewhere: usize,
    /// Whether the app keeps data on this node, in a managed volume or a
    /// host path, so its replica here waits for the node rather than
    /// moving (#2120).
    pub data_on_node: bool,
    pub risk: AppRisk,
}

/// What losing the node does to the council.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CouncilImpact {
    /// The node doesn't vote, so the council doesn't notice.
    NotAVoter,
    /// A voter gossip already counts as gone: losing it changes nothing
    /// more.
    AlreadyDown,
    /// Enough voters still answer to commit without it.
    KeepsQuorum { voters: u32, answering_after: u32 },
    /// Too few voters would answer to commit: no deploys, no scheduling
    /// changes, until it's back.
    LosesQuorum { voters: u32, answering_after: u32 },
    /// The summary didn't say how many voters answer.
    Unknown,
}

/// Everything the Maintenance tab shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeImpact {
    /// Worst first: apps that go down, then those losing redundancy.
    pub apps: Vec<AppImpact>,
    pub council: CouncilImpact,
}

/// What losing `node` would affect, from the TUI's instance list, its node
/// rows and the council as the summary and `/v1/cluster/council` see it.
pub fn node_impact(
    node: &NodeRow,
    nodes: &[NodeRow],
    instances: &[ClusterInstanceStatus],
    council: Option<&CouncilStatus>,
    council_section: Option<&CouncilSection>,
) -> NodeImpact {
    let name = node.status.node_id.as_str();
    // An instance gossip has given up on the node of can't take over.
    let down = |other: &str| {
        nodes
            .iter()
            .any(|row| row.status.node_id == other && row.status.is_down())
    };
    let mut apps: BTreeMap<(String, String), AppImpact> = BTreeMap::new();
    for row in instances.iter().filter(|row| row.node == name) {
        let instance = &row.instance;
        let entry = apps
            .entry((instance.namespace.clone(), instance.app_name.clone()))
            .or_insert_with(|| AppImpact {
                app: instance.app_name.clone(),
                namespace: instance.namespace.clone(),
                here: 0,
                ready_elsewhere: 0,
                data_on_node: false,
                risk: AppRisk::GoesDown,
            });
        entry.here += 1;
        entry.data_on_node |= instance.data_on_node;
    }
    for row in instances {
        if row.node == name || down(&row.node) || !is_ready(row) {
            continue;
        }
        let key = (
            row.instance.namespace.clone(),
            row.instance.app_name.clone(),
        );
        if let Some(entry) = apps.get_mut(&key) {
            entry.ready_elsewhere += 1;
        }
    }
    let mut apps: Vec<AppImpact> = apps
        .into_values()
        .map(|mut entry| {
            entry.risk = match entry.ready_elsewhere {
                0 => AppRisk::GoesDown,
                1 => AppRisk::LosesRedundancy,
                _ => AppRisk::Keeps,
            };
            entry
        })
        .collect();
    // `sort_by_key` is stable, so apps of one risk keep their name order.
    apps.sort_by_key(|entry| entry.risk as u8);
    NodeImpact {
        apps,
        council: council_impact(node, council, council_section),
    }
}

/// Ready as the app detail's Overview counts it.
fn is_ready(row: &ClusterInstanceStatus) -> bool {
    row.instance.state == "running"
}

fn council_impact(
    node: &NodeRow,
    council: Option<&CouncilStatus>,
    section: Option<&CouncilSection>,
) -> CouncilImpact {
    // The council's own member list says who votes; a learner sits on the
    // council without a vote. Without it, gossip's council flag will do.
    let voter = council
        .filter(|council| !council.members.is_empty())
        .map(|council| {
            council
                .members
                .iter()
                .any(|member| member.name == node.status.node_id && member.voter)
        })
        .unwrap_or(node.status.is_council);
    if !voter {
        return CouncilImpact::NotAVoter;
    }
    if node.status.state != "alive" {
        return CouncilImpact::AlreadyDown;
    }
    let Some(section) = section else {
        return CouncilImpact::Unknown;
    };
    let voters = section.voters;
    let answering_after = section.voters_answering.saturating_sub(1);
    // The upgrade orchestrator asks the same question before it restarts a
    // voter. It waves a lone voter through, since there's no other way to
    // upgrade one, but losing a lone voter does stop the council.
    let keeps = voters > 1
        && crate::upgrade::orchestrator::live_quorum_headroom_ok(
            voters as usize,
            section.voters_answering as usize,
        );
    if keeps {
        CouncilImpact::KeepsQuorum {
            voters,
            answering_after,
        }
    } else {
        CouncilImpact::LosesQuorum {
            voters,
            answering_after,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::agent::{CouncilMemberInfo, InstanceStatus};
    use crate::relish::tui::fixtures::TestScenario;

    fn instance(app: &str, node: &str, state: &str) -> ClusterInstanceStatus {
        ClusterInstanceStatus {
            node: node.into(),
            instance: InstanceStatus {
                id: format!("{app}-{node}"),
                app_name: app.into(),
                namespace: "default".into(),
                state: state.into(),
                restart_count: 0,
                host_port: None,
                exit_code: None,
                pid: None,
                runtime_unknown: false,
                status_age_ms: None,
                age_seconds: None,
                image: None,
                data_on_node: false,
                reason: None,
            },
        }
    }

    fn healthy() -> crate::relish::tui::app::ClusterData {
        TestScenario::HealthyCluster.data()
    }

    fn impact_on(
        data: &crate::relish::tui::app::ClusterData,
        node: &str,
        section: Option<&CouncilSection>,
    ) -> NodeImpact {
        let row = data
            .nodes
            .iter()
            .find(|row| row.status.node_id == node)
            .unwrap();
        node_impact(
            row,
            &data.nodes,
            &data.instances,
            data.council.as_ref(),
            section,
        )
    }

    fn section(voters: u32, answering: u32) -> CouncilSection {
        CouncilSection {
            leader: Some("node-1".into()),
            term: 4,
            voters,
            voters_answering: answering,
            quorum_ok: true,
            lagging: 0,
            other_leaders: Vec::new(),
        }
    }

    fn risk_of(impact: &NodeImpact, app: &str) -> AppRisk {
        impact
            .apps
            .iter()
            .find(|entry| entry.app == app)
            .unwrap_or_else(|| panic!("{app} missing from {impact:?}"))
            .risk
    }

    #[test]
    fn a_single_replica_app_goes_down_with_its_node() {
        let mut data = healthy();
        data.instances.push(instance("cache", "node-1", "running"));
        let impact = impact_on(&data, "node-1", None);
        assert_eq!(risk_of(&impact, "cache"), AppRisk::GoesDown);
        assert_eq!(impact.apps[0].app, "cache", "the worst comes first");
    }

    #[test]
    fn an_app_with_one_ready_replica_elsewhere_loses_redundancy() {
        let impact = impact_on(&healthy(), "node-1", None);
        assert_eq!(risk_of(&impact, "web"), AppRisk::LosesRedundancy);
    }

    #[test]
    fn an_app_with_two_ready_replicas_elsewhere_keeps_going() {
        let mut data = healthy();
        data.instances.push(instance("web", "node-3", "running"));
        let impact = impact_on(&data, "node-1", None);
        assert_eq!(risk_of(&impact, "web"), AppRisk::Keeps);
    }

    // The last ready replica: the one elsewhere is there but not ready.
    #[test]
    fn an_app_whose_other_replicas_are_not_ready_goes_down() {
        let mut data = TestScenario::DegradedApp.data();
        data.instances.push(instance("web", "node-3", "failed"));
        let impact = impact_on(&data, "node-1", None);
        assert_eq!(risk_of(&impact, "web"), AppRisk::GoesDown);
    }

    #[test]
    fn a_replica_on_a_dead_node_does_not_count_as_elsewhere() {
        let mut data = healthy();
        data.nodes[1].status.state = "dead".into();
        let impact = impact_on(&data, "node-1", None);
        assert_eq!(risk_of(&impact, "web"), AppRisk::GoesDown);
    }

    #[test]
    fn an_app_that_keeps_data_on_the_node_says_so() {
        let mut data = healthy();
        let mut db = instance("db", "node-1", "running");
        db.instance.data_on_node = true;
        data.instances.push(db);
        let impact = impact_on(&data, "node-1", None);
        let db = impact.apps.iter().find(|entry| entry.app == "db").unwrap();
        assert!(db.data_on_node);
        assert!(
            !impact
                .apps
                .iter()
                .find(|entry| entry.app == "web")
                .unwrap()
                .data_on_node
        );
    }

    #[test]
    fn apps_in_other_namespaces_are_separate() {
        let mut data = healthy();
        let mut other = instance("web", "node-1", "running");
        other.instance.namespace = "team-b".into();
        data.instances.push(other);
        let impact = impact_on(&data, "node-1", None);
        let team_b = impact
            .apps
            .iter()
            .find(|entry| entry.namespace == "team-b")
            .unwrap();
        assert_eq!(team_b.risk, AppRisk::GoesDown);
        assert_eq!(risk_of(&impact, "web"), AppRisk::GoesDown, "team-b first");
    }

    #[test]
    fn a_voter_whose_loss_leaves_quorum_keeps_it() {
        let impact = impact_on(&healthy(), "node-1", Some(&section(3, 3)));
        assert_eq!(
            impact.council,
            CouncilImpact::KeepsQuorum {
                voters: 3,
                answering_after: 2
            }
        );
    }

    #[test]
    fn a_voter_whose_loss_breaks_quorum_is_flagged() {
        // Three voters, one already silent: losing a second leaves one,
        // below the majority of two.
        let impact = impact_on(&healthy(), "node-1", Some(&section(3, 2)));
        assert_eq!(
            impact.council,
            CouncilImpact::LosesQuorum {
                voters: 3,
                answering_after: 1
            }
        );
    }

    #[test]
    fn losing_the_only_voter_loses_quorum() {
        let impact = impact_on(&healthy(), "node-1", Some(&section(1, 1)));
        assert!(matches!(impact.council, CouncilImpact::LosesQuorum { .. }));
    }

    #[test]
    fn a_learner_is_not_a_voter() {
        let mut data = healthy();
        let council = data.council.as_mut().unwrap();
        council.members = ["node-1", "node-2", "node-3"]
            .iter()
            .enumerate()
            .map(|(index, name)| CouncilMemberInfo {
                raft_id: index as u64,
                name: (*name).into(),
                address: String::new(),
                voter: *name != "node-3",
            })
            .collect();
        let impact = impact_on(&data, "node-3", Some(&section(2, 2)));
        assert_eq!(impact.council, CouncilImpact::NotAVoter);
        let impact = impact_on(&data, "node-2", Some(&section(2, 2)));
        assert!(matches!(impact.council, CouncilImpact::LosesQuorum { .. }));
    }

    #[test]
    fn a_voter_already_down_changes_nothing_more() {
        let mut data = healthy();
        data.nodes[2].status.state = "dead".into();
        let impact = impact_on(&data, "node-3", Some(&section(3, 2)));
        assert_eq!(impact.council, CouncilImpact::AlreadyDown);
    }

    #[test]
    fn without_the_summary_quorum_is_unknown() {
        let impact = impact_on(&healthy(), "node-1", None);
        assert_eq!(impact.council, CouncilImpact::Unknown);
    }
}

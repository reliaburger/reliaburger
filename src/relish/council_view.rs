//! The council as every node sees it (#424).
//!
//! One node's `/v1/cluster/council` answer is only that node's opinion. A
//! split brain looks healthy from either half, so `relish council status`,
//! the `relish status` header and `relish wtf` all ask every node, through
//! the entry node's relay, and compare the answers here.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::bun::agent::{CouncilRole, CouncilStatus, NodeStatus};
use crate::relish::RelishError;
use crate::relish::client::BunClient;

/// How long one node gets to answer before it counts as unreachable.
const NODE_TIMEOUT: Duration = Duration::from_secs(5);

/// One council member as an answering node lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CouncilMemberObservation {
    /// The member's node name.
    pub name: String,
    /// Whether it votes.
    pub voter: bool,
}

/// What one node said about the council, or why it said nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CouncilNodeObservation {
    /// The node asked.
    pub node_id: String,
    /// Why the node did not answer; `None` when it did.
    pub error: Option<String>,
    /// Its own role.
    pub role: CouncilRole,
    /// The recovery epoch it holds, if any.
    pub recovery_epoch: Option<u64>,
    /// The newer epoch that fenced it, when it is fenced.
    pub fenced_by: Option<u64>,
    /// The leader it names.
    pub leader: Option<String>,
    /// Its Raft term.
    pub term: u64,
    /// Its last applied log index.
    pub last_applied: Option<u64>,
    /// Its last log index.
    pub last_log_index: Option<u64>,
    /// The council membership it holds.
    pub members: Vec<CouncilMemberObservation>,
}

impl CouncilNodeObservation {
    /// The observation for a node that answered with `status`.
    pub fn answered(node_id: &str, status: &CouncilStatus) -> Self {
        Self {
            node_id: node_id.to_string(),
            error: None,
            role: status.role,
            recovery_epoch: status.recovery_epoch,
            fenced_by: status.fenced_by,
            leader: status.leader.clone(),
            term: status.term,
            last_applied: status.last_applied_log,
            last_log_index: status.last_log_index,
            members: status
                .members
                .iter()
                .map(|member| CouncilMemberObservation {
                    name: member.name.clone(),
                    voter: member.voter,
                })
                .collect(),
        }
    }

    /// The observation for a node that could not be asked or didn't answer.
    pub fn unanswered(node_id: &str, error: impl Into<String>) -> Self {
        Self {
            node_id: node_id.to_string(),
            error: Some(error.into()),
            role: CouncilRole::Worker,
            recovery_epoch: None,
            fenced_by: None,
            leader: None,
            term: 0,
            last_applied: None,
            last_log_index: None,
            members: Vec::new(),
        }
    }

    fn answered_ok(&self) -> bool {
        self.error.is_none()
    }

    fn serving(&self) -> bool {
        self.answered_ok() && self.fenced_by.is_none()
    }
}

/// Ask every node gossip knows for its council view, through `client`'s
/// relay. Nodes gossip already declares down are listed without asking.
pub async fn survey(client: &BunClient) -> Result<Vec<CouncilNodeObservation>, RelishError> {
    let nodes = client.nodes().await?;
    Ok(survey_nodes(client, &nodes).await)
}

/// [`survey`] over an already fetched node list.
pub async fn survey_nodes(client: &BunClient, nodes: &[NodeStatus]) -> Vec<CouncilNodeObservation> {
    let asks = nodes.iter().map(|node| async move {
        if node.is_down() {
            return CouncilNodeObservation::unanswered(
                &node.node_id,
                format!("gossip reports it {}", node.state),
            );
        }
        let relay = match client.via_node(&node.node_id) {
            Ok(relay) => relay,
            Err(error) => {
                return CouncilNodeObservation::unanswered(&node.node_id, error.to_string());
            }
        };
        match tokio::time::timeout(NODE_TIMEOUT, relay.council()).await {
            Ok(Ok(status)) => CouncilNodeObservation::answered(&node.node_id, &status),
            Ok(Err(error)) => CouncilNodeObservation::unanswered(&node.node_id, error.to_string()),
            Err(_) => CouncilNodeObservation::unanswered(
                &node.node_id,
                format!("timed out after {}s", NODE_TIMEOUT.as_secs()),
            ),
        }
    });
    let mut observations = futures_util::future::join_all(asks).await;
    observations.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    observations
}

/// A fenced node, and the epochs either side of its fence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FencedNode {
    /// The fenced node.
    pub node_id: String,
    /// The epoch of the council it belonged to.
    pub epoch: u64,
    /// The newer epoch that replaced it.
    pub fenced_by: u64,
}

/// What the answers add up to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CouncilSummary {
    /// The newest recovery epoch any node holds.
    pub epoch: Option<u64>,
    /// Nodes still serving (not fenced), by the epoch they hold. More than
    /// one entry is a split: two councils from different recoveries.
    pub epochs: BTreeMap<u64, Vec<String>>,
    /// Every leader a serving node names. More than one is a split brain.
    pub leaders: Vec<String>,
    /// Nodes fenced out of a replaced council.
    pub fenced: Vec<FencedNode>,
    /// The voters of the newest council, as its leader (or failing that,
    /// any of its members) lists them.
    pub voters: Vec<String>,
    /// Those voters that answered and serve.
    pub voters_answering: Vec<String>,
    /// Nodes that did not answer, with the reason.
    pub unanswered: Vec<(String, String)>,
}

impl CouncilSummary {
    /// Whether a majority of the newest council's voters answered.
    pub fn quorum_ok(&self) -> bool {
        !self.voters.is_empty() && self.voters_answering.len() > self.voters.len() / 2
    }

    /// Voters of the newest council that did not answer.
    pub fn voters_missing(&self) -> Vec<String> {
        self.voters
            .iter()
            .filter(|voter| !self.voters_answering.contains(voter))
            .cloned()
            .collect()
    }

    /// Whether this is a single node with no council at all.
    pub fn standalone(&self) -> bool {
        self.voters.is_empty() && self.epoch.is_none() && self.leaders.is_empty()
    }

    /// One-line warnings for everything wrong, worst first.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.epochs.len() > 1 {
            let sides = self
                .epochs
                .iter()
                .map(|(epoch, nodes)| format!("epoch {epoch}: {}", nodes.join(", ")))
                .collect::<Vec<_>>()
                .join("; ");
            warnings.push(format!("nodes serve different recovery epochs ({sides})"));
        }
        if self.leaders.len() > 1 {
            warnings.push(format!(
                "more than one leader is reported: {}",
                self.leaders.join(", ")
            ));
        }
        if !self.fenced.is_empty() {
            let names = self
                .fenced
                .iter()
                .map(|node| {
                    format!(
                        "{} (epoch {}, replaced by {})",
                        node.node_id, node.epoch, node.fenced_by
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            warnings.push(format!(
                "fenced: {names}; re-enrol each with `relish council re-enrol`"
            ));
        }
        let missing = self.voters_missing();
        if !missing.is_empty() {
            let verdict = if self.quorum_ok() {
                "quorum degraded"
            } else {
                "quorum lost"
            };
            warnings.push(format!(
                "{verdict}: voter(s) not answering: {}",
                missing.join(", ")
            ));
        }
        warnings
    }
}

/// Add up every node's answer.
pub fn summarise(observations: &[CouncilNodeObservation]) -> CouncilSummary {
    let serving: Vec<&CouncilNodeObservation> =
        observations.iter().filter(|o| o.serving()).collect();

    let mut epochs: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for node in &serving {
        if let Some(epoch) = node.recovery_epoch {
            epochs.entry(epoch).or_default().push(node.node_id.clone());
        }
    }
    let epoch = observations
        .iter()
        .filter(|o| o.answered_ok())
        .flat_map(|o| o.recovery_epoch.into_iter().chain(o.fenced_by))
        .max();

    let mut leaders = BTreeSet::new();
    for node in &serving {
        if node.role == CouncilRole::Leader {
            leaders.insert(node.node_id.clone());
        }
        if let Some(leader) = &node.leader {
            leaders.insert(leader.clone());
        }
    }

    let fenced = observations
        .iter()
        .filter_map(|o| {
            o.fenced_by.map(|fenced_by| FencedNode {
                node_id: o.node_id.clone(),
                epoch: o.recovery_epoch.unwrap_or_default(),
                fenced_by,
            })
        })
        .collect();

    // The newest council's own account of its membership: its leader's if
    // one answered, otherwise any member of that epoch.
    let newest: Vec<&&CouncilNodeObservation> = serving
        .iter()
        .filter(|o| o.recovery_epoch == epoch && !o.members.is_empty())
        .collect();
    let authority = newest
        .iter()
        .find(|o| o.role == CouncilRole::Leader)
        .or_else(|| newest.first());
    let voters: Vec<String> = authority
        .map(|o| {
            o.members
                .iter()
                .filter(|member| member.voter)
                .map(|member| member.name.clone())
                .collect()
        })
        .unwrap_or_default();
    let voters_answering = voters
        .iter()
        .filter(|voter| serving.iter().any(|o| &o.node_id == *voter))
        .cloned()
        .collect();

    let unanswered = observations
        .iter()
        .filter_map(|o| {
            o.error
                .as_ref()
                .map(|error| (o.node_id.clone(), error.clone()))
        })
        .collect();

    CouncilSummary {
        epoch,
        epochs,
        leaders: leaders.into_iter().collect(),
        fenced,
        voters,
        voters_answering,
        unanswered,
    }
}

/// The one-line council header `relish status` prints, plus a warning line
/// for each problem. Empty for a standalone node.
pub fn render_status_header(summary: &CouncilSummary) -> String {
    use std::fmt::Write as _;
    if summary.standalone() {
        return String::new();
    }
    let mut out = String::new();
    let epoch = summary
        .epoch
        .map(|epoch| epoch.to_string())
        .unwrap_or_else(|| "-".to_string());
    let leader = match summary.leaders.as_slice() {
        [] => "none".to_string(),
        [one] => one.clone(),
        many => many.join(" AND "),
    };
    let _ = writeln!(
        out,
        "council: epoch {epoch}, leader {leader}, {}/{} voters",
        summary.voters_answering.len(),
        summary.voters.len()
    );
    for warning in summary.warnings() {
        let _ = writeln!(out, "warning: {warning}");
    }
    out
}

fn index_cell(index: Option<u64>) -> String {
    index
        .map(|i| i.to_string())
        .unwrap_or_else(|| "-".to_string())
}

/// The human `relish council status` report.
pub fn render_council_status(
    observations: &[CouncilNodeObservation],
    summary: &CouncilSummary,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let epoch = summary
        .epoch
        .map(|epoch| epoch.to_string())
        .unwrap_or_else(|| "-".to_string());
    let leader = if summary.leaders.is_empty() {
        "(none)".to_string()
    } else {
        summary.leaders.join(", ")
    };
    let quorum = if summary.voters.is_empty() {
        "no council voters known".to_string()
    } else {
        let verdict = if !summary.quorum_ok() {
            "quorum LOST"
        } else if summary.voters_answering.len() < summary.voters.len() {
            "quorum degraded"
        } else {
            "quorum ok"
        };
        format!(
            "{}/{} voters, {verdict}",
            summary.voters_answering.len(),
            summary.voters.len()
        )
    };
    let _ = writeln!(out, "Epoch:   {epoch}");
    let _ = writeln!(out, "Leader:  {leader}");
    let _ = writeln!(out, "Quorum:  {quorum}");
    out.push('\n');

    let width = observations
        .iter()
        .map(|o| o.node_id.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let _ = writeln!(
        out,
        "{:<width$}  {:<9}  {:<10}  {:>5}  {:>8}  {:>8}  {:<7}  REACHABLE",
        "NODE", "ROLE", "EPOCH", "TERM", "APPLIED", "LOG", "MEMBER"
    );
    for node in observations {
        if node.error.is_some() {
            let _ = writeln!(
                out,
                "{:<width$}  {:<9}  {:<10}  {:>5}  {:>8}  {:>8}  {:<7}  no",
                node.node_id, "?", "?", "?", "?", "?", "?"
            );
            continue;
        }
        let epoch = match (node.recovery_epoch, node.fenced_by) {
            (Some(epoch), Some(newer)) => format!("{epoch} < {newer}"),
            (Some(epoch), None) => epoch.to_string(),
            (None, _) => "-".to_string(),
        };
        let member = match node.role {
            CouncilRole::Leader | CouncilRole::Follower | CouncilRole::Candidate => "voter",
            CouncilRole::Learner => "learner",
            CouncilRole::Fenced | CouncilRole::Starting | CouncilRole::Worker => "-",
        };
        let _ = writeln!(
            out,
            "{:<width$}  {:<9}  {:<10}  {:>5}  {:>8}  {:>8}  {:<7}  yes",
            node.node_id,
            node.role.as_str(),
            epoch,
            node.term,
            index_cell(node.last_applied),
            index_cell(node.last_log_index),
            member
        );
    }

    if !summary.unanswered.is_empty() {
        out.push('\n');
        let _ = writeln!(out, "Did not answer:");
        for (node, reason) in &summary.unanswered {
            let _ = writeln!(out, "  {node}: {reason}");
        }
    }
    let warnings = summary.warnings();
    if !warnings.is_empty() {
        out.push('\n');
        for warning in warnings {
            let _ = writeln!(out, "WARNING: {warning}");
        }
    }
    out
}

/// The machine-readable `relish council status -o json` document.
#[derive(Debug, Clone, Serialize)]
pub struct CouncilReport<'a> {
    /// What the answers add up to.
    pub summary: &'a CouncilSummary,
    /// Every node's answer.
    pub nodes: &'a [CouncilNodeObservation],
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, voter: bool) -> CouncilMemberObservation {
        CouncilMemberObservation {
            name: name.to_string(),
            voter,
        }
    }

    fn node(
        name: &str,
        role: CouncilRole,
        epoch: Option<u64>,
        leader: Option<&str>,
    ) -> CouncilNodeObservation {
        CouncilNodeObservation {
            node_id: name.to_string(),
            error: None,
            role,
            recovery_epoch: epoch,
            fenced_by: None,
            leader: leader.map(str::to_string),
            term: 4,
            last_applied: Some(120),
            last_log_index: Some(120),
            members: vec![
                member("node-1", true),
                member("node-2", true),
                member("node-3", true),
            ],
        }
    }

    fn healthy() -> Vec<CouncilNodeObservation> {
        vec![
            node("node-1", CouncilRole::Follower, Some(0), Some("node-2")),
            node("node-2", CouncilRole::Leader, Some(0), Some("node-2")),
            node("node-3", CouncilRole::Follower, Some(0), Some("node-2")),
        ]
    }

    /// The #424 split, after the fix: the recovered survivor leads epoch 1
    /// alone, and the two old voters came back fenced.
    fn after_recovery() -> Vec<CouncilNodeObservation> {
        let mut recovered = node("node-1", CouncilRole::Leader, Some(1), Some("node-1"));
        recovered.term = 2;
        recovered.last_applied = Some(7);
        recovered.last_log_index = Some(7);
        recovered.members = vec![member("node-1", true)];
        let mut old2 = node("node-2", CouncilRole::Fenced, Some(0), None);
        old2.fenced_by = Some(1);
        let mut old3 = node("node-3", CouncilRole::Fenced, Some(0), None);
        old3.fenced_by = Some(1);
        vec![recovered, old2, old3]
    }

    #[test]
    fn a_healthy_council_has_one_epoch_one_leader_and_quorum() {
        let summary = summarise(&healthy());
        assert_eq!(summary.epoch, Some(0));
        assert_eq!(summary.leaders, vec!["node-2".to_string()]);
        assert_eq!(summary.voters.len(), 3);
        assert!(summary.quorum_ok());
        assert!(summary.warnings().is_empty());
    }

    #[test]
    fn fenced_old_voters_are_named_and_the_newest_council_is_the_authority() {
        let summary = summarise(&after_recovery());
        assert_eq!(summary.epoch, Some(1));
        assert_eq!(summary.voters, vec!["node-1".to_string()]);
        assert!(summary.quorum_ok());
        assert_eq!(summary.fenced.len(), 2);
        // Fenced nodes don't count as a second epoch: the fence is working.
        assert_eq!(summary.epochs.len(), 1);
        let warnings = summary.warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("node-2 (epoch 0, replaced by 1)"));
    }

    #[test]
    fn two_serving_epochs_and_two_leaders_are_a_split() {
        // What #424 looked like before the fence: both halves serve.
        let mut nodes = healthy();
        nodes[0] = node("node-1", CouncilRole::Leader, Some(1), Some("node-1"));
        nodes[0].members = vec![member("node-1", true)];
        nodes[1].role = CouncilRole::Leader;
        nodes[2].leader = Some("node-2".to_string());
        let summary = summarise(&nodes);
        assert_eq!(summary.epochs.len(), 2);
        assert_eq!(
            summary.leaders,
            vec!["node-1".to_string(), "node-2".to_string()]
        );
        let warnings = summary.warnings();
        assert!(warnings[0].contains("different recovery epochs"));
        assert!(warnings[1].contains("more than one leader"));
    }

    #[test]
    fn an_unanswering_voter_degrades_quorum_and_two_lose_it() {
        let mut nodes = healthy();
        nodes[2] = CouncilNodeObservation::unanswered("node-3", "timed out after 5s");
        let summary = summarise(&nodes);
        assert!(summary.quorum_ok());
        assert_eq!(summary.voters_missing(), vec!["node-3".to_string()]);
        assert!(summary.warnings()[0].starts_with("quorum degraded"));

        nodes[0] = CouncilNodeObservation::unanswered("node-1", "connection refused");
        let summary = summarise(&nodes);
        assert!(!summary.quorum_ok());
        assert!(summary.warnings()[0].starts_with("quorum lost"));
    }

    #[test]
    fn a_standalone_node_prints_no_header() {
        let worker = CouncilNodeObservation::answered("solo", &CouncilStatus::default());
        assert_eq!(render_status_header(&summarise(&[worker])), "");
    }

    #[test]
    fn status_header_for_a_healthy_council() {
        insta::assert_snapshot!(render_status_header(&summarise(&healthy())), @"council: epoch 0, leader node-2, 3/3 voters");
    }

    #[test]
    fn status_header_after_recovery_warns_about_the_fenced_voters() {
        insta::assert_snapshot!(render_status_header(&summarise(&after_recovery())));
    }

    #[test]
    fn council_status_table_for_a_healthy_council() {
        let nodes = healthy();
        insta::assert_snapshot!(render_council_status(&nodes, &summarise(&nodes)));
    }

    #[test]
    fn council_status_table_after_recovery_shows_the_fence() {
        let mut nodes = after_recovery();
        nodes.push(CouncilNodeObservation::unanswered(
            "node-4",
            "timed out after 5s",
        ));
        insta::assert_snapshot!(render_council_status(&nodes, &summarise(&nodes)));
    }
}

//! Cluster-wide views assembled from every node's local answer.
//!
//! Deploy history, events and jobs live on the node that produced them. The
//! node a client talks to asks every live member for its share (with
//! `local=true`, so a peer answers for itself and never fans out again),
//! tags each row with the node it came from, and turns every member that
//! didn't answer into a warning. The shapes here are what those endpoints
//! return; the merging is kept free of I/O so it can be tested on its own.

use serde::{Deserialize, Serialize};

use super::agent::JobStatus;
use super::events::ClusterEvent;
use crate::meat::deploy_types::DeployHistoryEntry;

/// One row of a cluster-wide view, with the node that holds it.
///
/// `flatten` writes the row's own fields next to `node`, so a client that
/// only knew the single-node shape still finds `image` or `state` where it
/// expects them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeTagged<T> {
    /// The node that recorded or runs this row.
    pub node: String,
    /// The row itself.
    #[serde(flatten)]
    pub row: T,
}

/// `GET /v1/deploys/history/{app}`: every node's record of the app's deploys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterDeployHistory {
    /// The app asked about.
    pub app: String,
    /// Its namespace.
    pub namespace: String,
    /// Oldest first; one entry per node that rolled the deploy out.
    pub history: Vec<NodeTagged<DeployHistoryEntry>>,
    /// One line per member whose share is missing.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// `GET /v1/events`: the newest events across every node, oldest first.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClusterEvents {
    /// Merged events; each names its node.
    pub events: Vec<ClusterEvent>,
    /// One line per member whose events are missing.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// `GET /v1/jobs?cluster=true`: every node's run-to-completion workloads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClusterJobs {
    /// Jobs sorted by node, namespace and name.
    pub jobs: Vec<NodeTagged<JobStatus>>,
    /// One line per member whose jobs are missing.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// One API token as `GET /v1/token/list` describes it: never the secret
/// or its hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenSummary {
    /// The name given to `relish token create`.
    pub name: String,
    /// The stable principal id audit events name (`token:<digest>`). Names
    /// can be reused after a revoke; this can't, so merging joins on it.
    pub principal: String,
    /// The role the token grants (`admin`, `deployer`, `read-only`).
    pub role: String,
    /// The apps and namespaces the token is confined to; `None` for each
    /// means no restriction.
    pub scope: crate::sesame::types::TokenScope,
    /// Creation time, Unix seconds.
    pub created_at: u64,
    /// Expiry, Unix seconds; `None` for a token that never expires.
    pub expires_at: Option<u64>,
    /// When any node last authenticated a request with this token, Unix
    /// seconds; `None` if no node has since it last started.
    pub last_used: Option<u64>,
}

/// `GET /v1/token/list`: the cluster's API tokens, with the latest use any
/// node saw for each.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClusterTokens {
    /// Tokens in the answering node's store order.
    pub tokens: Vec<TokenSummary>,
    /// One line per member whose last-use times are missing.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Fold every peer's last-use times into this node's token list.
///
/// `local` decides which tokens exist: it comes from this node's Raft
/// state. A peer's row joins on the principal, not the name, so a token
/// revoked and re-created under the same name never inherits the old
/// one's use. Each token keeps the latest time any node saw.
pub fn merge_token_last_used(
    mut local: Vec<TokenSummary>,
    peers: Vec<Vec<TokenSummary>>,
) -> Vec<TokenSummary> {
    let mut latest: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for row in peers.into_iter().flatten() {
        if let Some(seen) = row.last_used {
            let entry = latest.entry(row.principal).or_insert(seen);
            *entry = (*entry).max(seen);
        }
    }
    for token in &mut local {
        if let Some(&seen) = latest.get(&token.principal) {
            token.last_used = token.last_used.max(Some(seen));
        }
    }
    local
}

/// Tag every row of each node's answer with that node.
pub fn tag_rows<T>(answers: Vec<(String, Vec<T>)>) -> Vec<NodeTagged<T>> {
    answers
        .into_iter()
        .flat_map(|(node, rows)| {
            rows.into_iter().map(move |row| NodeTagged {
                node: node.clone(),
                row,
            })
        })
        .collect()
}

/// Merge each node's history, oldest deploy first.
///
/// Ties on start time order by node so the view is stable between refreshes.
pub fn merge_deploy_history(
    answers: Vec<(String, Vec<DeployHistoryEntry>)>,
) -> Vec<NodeTagged<DeployHistoryEntry>> {
    let mut history = tag_rows(answers);
    history.sort_by(|left, right| {
        (left.row.created_at, &left.node, left.row.id.0).cmp(&(
            right.row.created_at,
            &right.node,
            right.row.id.0,
        ))
    });
    history
}

/// Merge each node's events and keep the newest `limit`, oldest first.
///
/// Sequence numbers are per process, so they only break ties within one
/// node. An event that didn't name a node gets the node that recorded it.
pub fn merge_events(answers: Vec<(String, Vec<ClusterEvent>)>, limit: usize) -> Vec<ClusterEvent> {
    let mut events: Vec<ClusterEvent> = answers
        .into_iter()
        .flat_map(|(node, events)| {
            events.into_iter().map(move |mut event| {
                if event.node.is_none() {
                    event.node = Some(node.clone());
                }
                event
            })
        })
        .collect();
    events.sort_by(|left, right| {
        (left.timestamp, &left.node, left.sequence).cmp(&(
            right.timestamp,
            &right.node,
            right.sequence,
        ))
    });
    let skip = events.len().saturating_sub(limit);
    events.split_off(skip)
}

/// Merge each node's jobs, sorted by node, namespace and name.
pub fn merge_jobs(answers: Vec<(String, Vec<JobStatus>)>) -> Vec<NodeTagged<JobStatus>> {
    let mut jobs = tag_rows(answers);
    jobs.sort_by(|left, right| {
        (
            &left.node,
            &left.row.namespace,
            &left.row.name,
            &left.row.instance_id,
        )
            .cmp(&(
                &right.node,
                &right.row.namespace,
                &right.row.name,
                &right.row.instance_id,
            ))
    });
    jobs
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::bun::events::{EventKind, EventSeverity};
    use crate::meat::deploy_types::{DeployId, DeployResult};
    use crate::meat::types::AppId;

    fn event(sequence: u64, timestamp: u64, node: Option<&str>) -> ClusterEvent {
        ClusterEvent {
            sequence,
            timestamp,
            kind: EventKind::Deploy,
            severity: EventSeverity::Info,
            app: Some("web".into()),
            namespace: Some("default".into()),
            node: node.map(str::to_string),
            action: None,
            principal: None,
            details: Default::default(),
            message: format!("event {sequence}"),
        }
    }

    fn history_entry(id: u64, created_secs: u64) -> DeployHistoryEntry {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(created_secs);
        DeployHistoryEntry {
            id: DeployId(id),
            app_id: AppId::new("web", "default"),
            image: format!("web:{id}"),
            result: DeployResult::Completed,
            created_at: at,
            completed_at: at,
            steps_completed: 1,
            steps_total: 1,
            spec: None,
        }
    }

    fn job(name: &str) -> JobStatus {
        JobStatus {
            name: name.into(),
            namespace: "default".into(),
            instance_id: format!("{name}-0"),
            image: "proc-grill:image-ignored".into(),
            state: "stopped".into(),
            restart_count: 0,
            age_seconds: 5,
        }
    }

    #[test]
    fn merged_events_interleave_by_time_and_name_their_node() {
        let merged = merge_events(
            vec![
                ("b".into(), vec![event(1, 20, None), event(2, 40, None)]),
                (
                    "a".into(),
                    vec![event(7, 10, None), event(8, 30, Some("c"))],
                ),
            ],
            10,
        );
        let order: Vec<(u64, Option<&str>)> = merged
            .iter()
            .map(|event| (event.timestamp, event.node.as_deref()))
            .collect();
        assert_eq!(
            order,
            [
                (10, Some("a")),
                (20, Some("b")),
                (30, Some("c")),
                (40, Some("b"))
            ]
        );
    }

    #[test]
    fn merged_events_keep_only_the_newest_limit() {
        let merged = merge_events(
            vec![
                ("a".into(), vec![event(1, 1, None), event(2, 3, None)]),
                ("b".into(), vec![event(1, 2, None), event(2, 4, None)]),
            ],
            2,
        );
        let times: Vec<u64> = merged.iter().map(|event| event.timestamp).collect();
        assert_eq!(times, [3, 4]);
    }

    #[test]
    fn merged_history_is_oldest_first_with_every_nodes_record() {
        let merged = merge_deploy_history(vec![
            ("b".into(), vec![history_entry(2, 200)]),
            (
                "a".into(),
                vec![history_entry(1, 100), history_entry(2, 200)],
            ),
        ]);
        let rows: Vec<(&str, u64)> = merged
            .iter()
            .map(|entry| (entry.node.as_str(), entry.row.id.0))
            .collect();
        assert_eq!(rows, [("a", 1), ("a", 2), ("b", 2)]);
    }

    #[test]
    fn tagged_rows_serialise_flat_beside_their_node() {
        let json = serde_json::to_value(NodeTagged {
            node: "n2".to_string(),
            row: job("migrate"),
        })
        .unwrap();
        assert_eq!(json["node"], "n2");
        assert_eq!(json["name"], "migrate");
        let back: NodeTagged<JobStatus> = serde_json::from_value(json).unwrap();
        assert_eq!(back.row.name, "migrate");
    }

    #[test]
    fn merged_jobs_sort_by_node_then_name() {
        let merged = merge_jobs(vec![
            ("b".into(), vec![job("seed")]),
            ("a".into(), vec![job("seed"), job("migrate")]),
        ]);
        let rows: Vec<(&str, &str)> = merged
            .iter()
            .map(|job| (job.node.as_str(), job.row.name.as_str()))
            .collect();
        assert_eq!(rows, [("a", "migrate"), ("a", "seed"), ("b", "seed")]);
    }
}

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

/// The spec `relish rollback` re-applies: the newest successful deploy whose
/// spec differs from `current`, with `current`'s replica count.
///
/// `history` is an app's records from every node, in any order. Each node
/// records its own share of a deploy, so one rollout appears once per node,
/// and each copy's `replicas` is that node's share. Comparing specs with the
/// replica count set aside collapses the copies, and taking the count from
/// `current` stops a rollback from shrinking the app to one node's share.
/// So a rollback changes the version, never the scale.
///
/// `current` is the spec in Raft on a cluster. Without one (a single node),
/// the newest record is the current version.
pub fn rollback_target(
    history: &[DeployHistoryEntry],
    current: Option<&crate::config::app::AppSpec>,
) -> Option<crate::config::app::AppSpec> {
    let mut deployed: Vec<&DeployHistoryEntry> = history
        .iter()
        .filter(|entry| {
            entry.result == crate::meat::deploy_types::DeployResult::Completed
                && entry.spec.is_some()
        })
        .collect();
    deployed.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
    let current = match current {
        Some(spec) => spec.clone(),
        None => (**deployed.first()?.spec.as_ref()?).clone(),
    };
    // A node records its share under the ordinals the leader gave it (#398);
    // they belong to the placement, so they're set aside with the count.
    let at_current_scale = |spec: &crate::config::app::AppSpec| {
        let mut spec = spec.clone();
        spec.replicas = current.replicas;
        spec.ordinals = current.ordinals.clone();
        spec
    };
    deployed
        .iter()
        .filter_map(|entry| entry.spec.as_deref())
        .map(at_current_scale)
        .find(|spec| *spec != current)
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

    /// What node `node` recorded for deploying `image` at `secs`, with its
    /// share of `replicas`.
    fn rolled_out(image: &str, share: u32, secs: u64) -> DeployHistoryEntry {
        let mut entry = history_entry(secs, secs);
        let spec: crate::config::app::AppSpec =
            toml::from_str(&format!("image = {image:?}\nreplicas = {share}\n")).unwrap();
        entry.image = image.into();
        entry.spec = Some(Box::new(spec));
        entry
    }

    fn spec(image: &str, replicas: u32) -> crate::config::app::AppSpec {
        toml::from_str(&format!("image = {image:?}\nreplicas = {replicas}\n")).unwrap()
    }

    /// F07 part 2: on a cluster every node records its own share of each
    /// deploy, so the merged history holds v2 three times. The version
    /// before the current one is v1, not another copy of v2.
    #[test]
    fn rollback_ignores_the_ordinals_each_node_recorded() {
        // Each node records its share under the ordinals the leader gave it
        // (#398); the spec in Raft has none. Compared as they are, every
        // copy of v2 looked like an older version and v2 was "rolled back"
        // to itself (found by the #459 cluster test).
        let with_ordinals = |image: &str, ordinals: Vec<u32>, secs: u64| {
            let mut entry = rolled_out(image, ordinals.len() as u32, secs);
            entry.spec.as_mut().unwrap().ordinals = Some(ordinals);
            entry
        };
        let history = [
            with_ordinals("web:v1", vec![0], 10),
            with_ordinals("web:v1", vec![1], 11),
            with_ordinals("web:v2", vec![0], 20),
            with_ordinals("web:v2", vec![1], 21),
        ];
        let target = rollback_target(&history, Some(&spec("web:v2", 2))).unwrap();
        assert_eq!(target.image.as_deref(), Some("web:v1"));
        assert_eq!(target.ordinals, None);
        assert_eq!(target.replicas, crate::config::Replicas::Fixed(2));
    }

    #[test]
    fn rollback_skips_other_nodes_copies_of_the_current_version() {
        let history = [
            rolled_out("web:v1", 1, 10),
            rolled_out("web:v1", 2, 11),
            rolled_out("web:v2", 1, 20),
            rolled_out("web:v2", 1, 21),
            rolled_out("web:v2", 1, 22),
        ];
        let target = rollback_target(&history, Some(&spec("web:v2", 3))).unwrap();
        assert_eq!(target.image.as_deref(), Some("web:v1"));
    }

    /// A node's record carries its share of the replicas, not the app's.
    /// Rolling back with it would cut a three-replica app to one; rollback
    /// changes the version and keeps the scale.
    #[test]
    fn rollback_keeps_the_apps_replica_count() {
        let history = [rolled_out("web:v1", 1, 10), rolled_out("web:v2", 1, 20)];
        let target = rollback_target(&history, Some(&spec("web:v2", 3))).unwrap();
        assert_eq!(target.replicas, crate::config::Replicas::Fixed(3));
    }

    /// Without a council there's no spec in Raft: the newest record is the
    /// current version.
    #[test]
    fn rollback_without_a_current_spec_takes_the_newest_record_as_current() {
        let history = [rolled_out("web:v1", 2, 10), rolled_out("web:v2", 2, 20)];
        let target = rollback_target(&history, None).unwrap();
        assert_eq!(target.image.as_deref(), Some("web:v1"));
        assert_eq!(target.replicas, crate::config::Replicas::Fixed(2));
    }

    #[test]
    fn rollback_ignores_failed_deploys_and_has_nothing_without_an_earlier_version() {
        let mut failed = rolled_out("web:broken", 1, 15);
        failed.result = DeployResult::Failed;
        let history = [rolled_out("web:v1", 1, 10), failed];
        assert_eq!(rollback_target(&history, Some(&spec("web:v1", 1))), None);
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

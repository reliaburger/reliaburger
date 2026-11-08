//! `GET /v1/cluster/summary`: the whole cluster at a glance.
//!
//! One answer to "is my cluster all right?", the same size on three nodes
//! or three hundred: counts and verdicts, never one row per thing. Any node
//! assembles it, asking its peers with a deadline, and every section says
//! how much of the cluster it saw. A node that didn't answer turns a section
//! `partial`, never into a silently smaller number; a source that couldn't
//! be read at all turns it `missing`, never into a zero.
//!
//! `relish status`, `relish agent context`, the TUI and Brioche read this
//! one shape. The types here cross the wire; the assembly from what the
//! node collected is kept free of I/O so the tests run it over fixtures.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The most entries any summary list carries (the worst apps, the busiest
/// nodes). The summary is counts; `relish apps` and `relish nodes` list.
pub const SUMMARY_LIST_MAX: usize = 5;

/// How much of the cluster a section saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Every source answered.
    Complete,
    /// Some sources answered and some didn't: counts are lower bounds and
    /// `coverage` says how many answered.
    Partial,
    /// Nothing could be read. The section's `value` is absent, and absent
    /// never means zero.
    Missing,
}

impl Source {
    /// The source of a section that asked `asked` nodes and heard from
    /// `answered` of them. Asking nobody is complete: there was nothing to
    /// miss.
    pub fn from_coverage(answered: u32, asked: u32) -> Self {
        if answered >= asked {
            Source::Complete
        } else if answered == 0 {
            Source::Missing
        } else {
            Source::Partial
        }
    }
}

/// How many nodes a fanned-out section heard from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Coverage {
    /// Nodes that answered in time.
    pub answered: u32,
    /// Nodes asked.
    pub asked: u32,
}

/// One section of the summary, with how much of the cluster it saw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Section<T> {
    /// Whether `value` covers the whole cluster.
    pub source: Source,
    /// Why the section is partial or missing: which nodes didn't answer,
    /// or which read failed. `None` when it's complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// For a section read from every node, how many answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    /// What was seen. `None` exactly when `source` is `missing`.
    pub value: Option<T>,
}

impl<T> Section<T> {
    /// A section whose one source answered.
    pub fn complete(value: T) -> Self {
        Section {
            source: Source::Complete,
            reason: None,
            coverage: None,
            value: Some(value),
        }
    }

    /// A section that couldn't be read, and why.
    pub fn missing(reason: impl Into<String>) -> Self {
        Section {
            source: Source::Missing,
            reason: Some(reason.into()),
            coverage: None,
            value: None,
        }
    }

    /// A section read from `coverage.asked` nodes. `reasons` names each
    /// node that didn't answer; with no answers at all the value is
    /// dropped, because a count over nobody isn't a zero.
    pub fn from_nodes(value: T, coverage: Coverage, reasons: &[String]) -> Self {
        let source = Source::from_coverage(coverage.answered, coverage.asked);
        let reason = (!reasons.is_empty()).then(|| reasons.join("; "));
        Section {
            source,
            reason: match (source, reason) {
                (Source::Complete, reason) => reason,
                (_, Some(reason)) => Some(reason),
                (_, None) => Some(format!(
                    "{} of {} nodes answered",
                    coverage.answered, coverage.asked
                )),
            },
            coverage: Some(coverage),
            value: (source != Source::Missing).then_some(value),
        }
    }

    /// A section the caller's token can't see, which is missing rather
    /// than empty: a scoped token can't vouch for the whole cluster.
    pub fn not_visible(what: &str) -> Self {
        Section::missing(format!("{what} needs an unscoped token"))
    }
}

/// `GET /v1/cluster/summary`: the whole cluster at a glance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClusterSummary {
    /// When the node assembled this, Unix milliseconds.
    pub generated_at_unix_ms: u64,
    /// The node that answered.
    pub answered_by: String,
    /// That node's build version.
    pub version: String,
    /// Whether the caller's token is confined to some apps or namespaces.
    /// A scoped summary counts only what the token may see, and the
    /// cluster-wide sections it can't vouch for are `missing`.
    pub scoped: bool,
    /// Gossip membership by state.
    pub nodes: Section<NodesSection>,
    /// The Raft council.
    pub council: Section<CouncilSection>,
    /// The gossip layer itself.
    pub gossip: Section<GossipSection>,
    /// Which build each node runs.
    pub versions: Section<VersionsSection>,
    /// What the scheduler has committed: requests, not measured load.
    pub capacity: Section<CapacitySection>,
    /// Measured load, from the metrics pipeline.
    pub usage: Section<UsageSection>,
    /// Apps by how they're doing.
    pub apps: Section<AppsSection>,
    /// Instances running against instances wanted.
    pub instances: Section<InstancesSection>,
    /// Run-to-completion jobs.
    pub jobs: Section<JobsSection>,
    /// Whether log ingestion is live.
    pub logs: Section<LogsSection>,
    /// Active Smoker faults: what you see may be a drill.
    pub faults: Section<FaultsSection>,
    /// Alerts firing now.
    pub alerts: Section<AlertsSection>,
    /// The certificate that expires soonest.
    pub certs: Section<CertsSection>,
    /// GitOps sync, `None` when GitOps isn't configured.
    pub gitops: Section<GitOpsSection>,
}

/// Gossip membership by state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NodesSection {
    /// Nodes gossip hears from.
    pub alive: u32,
    /// Nodes gossip suspects but hasn't given up on.
    pub suspect: u32,
    /// Nodes gossip has declared dead.
    pub dead: u32,
    /// Nodes that left the cluster.
    pub left: u32,
    /// Alive nodes cordoned by an upgrade, which take no new work.
    pub cordoned: u32,
    /// Nodes with a data path at or above
    /// [`DISK_PRESSURE_PERCENT`] full, among those whose usage sample
    /// arrived.
    pub disk_pressure: u32,
}

/// A data path at or above this many percent full counts as disk pressure,
/// the same threshold `relish wtf`'s `disk-high` check uses.
pub const DISK_PRESSURE_PERCENT: f64 = 85.0;

/// The Raft council.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CouncilSection {
    /// The leader's node name, `None` while there isn't one.
    pub leader: Option<String>,
    /// The current Raft term.
    pub term: u64,
    /// Voters in the membership.
    pub voters: u32,
    /// Voters gossip sees alive.
    pub voters_answering: u32,
    /// Whether enough voters answer to commit.
    pub quorum_ok: bool,
    /// Voters more than [`LAGGING_ENTRIES`] behind the leader's log.
    pub lagging: u32,
}

/// A voter more than this many log entries behind the leader is lagging,
/// as `relish council` counts it.
pub const LAGGING_ENTRIES: u64 = 100;

/// The gossip layer itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GossipSection {
    /// Members in the answering node's table, any state.
    pub members: u32,
    /// Membership updates the answering node has received since it started.
    pub updates_received: u64,
    /// When the answering node last saw a member join, change state or
    /// leave, Unix milliseconds. `None` before the first change.
    pub last_change_unix_ms: Option<u64>,
}

/// Which build each node runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VersionsSection {
    /// One entry per version, the most common first.
    pub versions: Vec<VersionCount>,
    /// Whether a cluster upgrade is under way.
    pub upgrade_in_progress: bool,
}

/// How many nodes run one version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VersionCount {
    /// The build version, e.g. `0.2.8`.
    pub version: String,
    /// Nodes running it.
    pub nodes: u32,
}

/// What the scheduler has committed: the sums of requests, from the
/// leader's view of every node's state report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CapacitySection {
    /// Nodes whose fresh report the totals include.
    pub nodes_reporting: u32,
    /// CPU requested by running instances and job work, millicores.
    pub cpu_allocated_millicores: u64,
    /// Schedulable CPU, millicores.
    pub cpu_total_millicores: u64,
    /// Memory requested, MiB.
    pub memory_allocated_mb: u64,
    /// Schedulable memory, MiB.
    pub memory_total_mb: u64,
    /// GPUs requested by running instances.
    pub gpu_allocated: u32,
    /// GPUs the nodes detected and enabled.
    pub gpu_total: u32,
}

/// Measured load, from the newest metrics samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UsageSection {
    /// Mean CPU use across nodes, percent.
    pub cpu_percent: Option<f64>,
    /// Memory in use across nodes, percent of their total.
    pub memory_percent: Option<f64>,
    /// Disk in use across nodes, percent of their total.
    pub disk_percent: Option<f64>,
    /// The nodes using the most CPU, busiest first, at most
    /// [`SUMMARY_LIST_MAX`].
    pub busiest: Vec<NodePercent>,
    /// When the newest sample was taken, Unix milliseconds: proof the
    /// metrics pipeline is live. `None` when there are no samples.
    pub newest_sample_unix_ms: Option<u64>,
}

/// One node's share of a measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodePercent {
    /// The node.
    pub node: String,
    /// Percent.
    pub percent: f64,
}

/// Apps by how they're doing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AppsSection {
    /// Apps the council knows.
    pub total: u32,
    /// Every replica runs.
    pub running: u32,
    /// Some replicas run, not all.
    pub degraded: u32,
    /// It wants replicas and none run.
    pub down: u32,
    /// Some nodes didn't answer, so what runs there isn't known.
    pub unknown: u32,
    /// It wants no replicas.
    pub stopped: u32,
    /// The scheduler couldn't place every replica (a subset of the above).
    pub unschedulable: u32,
    /// The worst apps, worst first, at most [`SUMMARY_LIST_MAX`]; only
    /// apps that aren't running or stopped.
    pub worst: Vec<AppBrief>,
}

/// One app in a line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AppBrief {
    /// The app.
    pub app: String,
    /// Its namespace.
    pub namespace: String,
    /// How it's doing.
    pub state: AppState,
    /// Replicas it wants.
    pub desired: u32,
    /// Replicas running on the nodes that answered.
    pub running: u32,
}

/// How an app is doing. The order is worst first.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AppState {
    /// It wants replicas and none run.
    Down,
    /// Some of its replicas run, not all.
    Degraded,
    /// Every replica runs, but a finding is about it. Only a reader that
    /// ran `wtf`'s checks can say so; the summary never does.
    Troubled,
    /// Some nodes didn't answer, so what runs there isn't known.
    Unknown,
    /// Every replica runs and nothing is wrong with it.
    Running,
    /// It wants no replicas.
    Stopped,
}

impl AppState {
    /// The state's word, as the JSON spells it.
    pub fn word(self) -> &'static str {
        match self {
            AppState::Down => "down",
            AppState::Degraded => "degraded",
            AppState::Troubled => "troubled",
            AppState::Unknown => "unknown",
            AppState::Running => "running",
            AppState::Stopped => "stopped",
        }
    }
}

/// How one app is doing: the rollup `relish agent context`, `relish status`
/// and the summary share.
///
/// `running` counts replicas running on the nodes that answered; with
/// `nodes_unanswered`, a low count may only mean a node's answer is
/// missing, so the app is `unknown` rather than `down` or `degraded`.
/// `has_findings` says a `wtf` finding is about the app.
pub fn app_state(
    desired: u32,
    running: u32,
    nodes_unanswered: bool,
    has_findings: bool,
) -> AppState {
    if desired == 0 {
        AppState::Stopped
    } else if running == 0 && !nodes_unanswered {
        AppState::Down
    } else if running < desired && !nodes_unanswered {
        AppState::Degraded
    } else if has_findings {
        AppState::Troubled
    } else if nodes_unanswered {
        AppState::Unknown
    } else {
        AppState::Running
    }
}

/// Instances running against instances wanted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstancesSection {
    /// Instances running on the nodes that answered.
    pub running: u32,
    /// Replicas the apps want, summed.
    pub desired: u32,
    /// Instances in any other state (starting, failed, stopped).
    pub not_running: u32,
}

/// Run-to-completion jobs: runs by state and the tasks behind them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobsSection {
    /// Job runs running now.
    pub running: u32,
    /// Tasks waiting for a node, across every running run.
    pub tasks_queued: u64,
    /// Tasks nodes hold (running or about to), across every running run.
    pub tasks_running: u64,
    /// Runs that finished successfully in the last hour.
    pub succeeded_last_hour: u32,
    /// Runs that failed, or finished with failed tasks, in the last hour.
    pub failed_last_hour: u32,
    /// Job definitions with a schedule (`cron`).
    pub scheduled: u32,
}

/// Whether log ingestion is live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LogsSection {
    /// The newest line any node ingested, Unix milliseconds. `None` when no
    /// node has ingested a line.
    pub newest_unix_ms: Option<u64>,
    /// The node whose newest line is oldest: the one ingestion is behind
    /// on. `None` when fewer than one node has ingested a line.
    pub slowest: Option<NodeTime>,
}

/// One node's timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NodeTime {
    /// The node.
    pub node: String,
    /// Unix milliseconds.
    pub unix_ms: u64,
}

/// Active Smoker faults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FaultsSection {
    /// Faults active now.
    pub active: u32,
    /// The ones that expire soonest, at most [`SUMMARY_LIST_MAX`].
    pub soonest: Vec<FaultBrief>,
}

/// One fault in a line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FaultBrief {
    /// What kind, e.g. `delay`.
    pub kind: String,
    /// What it targets: a service, or `node NAME`.
    pub target: String,
    /// Seconds until it expires.
    pub remaining_seconds: u64,
}

/// Alerts firing now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AlertsSection {
    /// Alerts firing.
    pub firing: u32,
    /// The first few, at most [`SUMMARY_LIST_MAX`].
    pub first: Vec<String>,
}

/// The certificate that expires soonest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CertsSection {
    /// Certificates seen.
    pub count: u32,
    /// The one that expires first; `None` when none were seen.
    pub soonest: Option<CertBrief>,
}

/// One certificate's expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CertBrief {
    /// What it is, e.g. `node node-3`.
    pub subject: String,
    /// When it expires, Unix seconds.
    pub not_after_unix_seconds: u64,
}

/// GitOps sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitOpsSection {
    /// Whether a GitOps repository is configured. When it isn't, the other
    /// fields are empty and readers hide the line.
    pub configured: bool,
    /// The commit last applied.
    pub commit: Option<String>,
    /// When it was applied, Unix seconds.
    pub synced_at_unix_seconds: Option<u64>,
    /// The last sync's error, if it failed.
    pub error: Option<String>,
}

/// `GET /v1/cluster/capacity`: what the scheduler has committed on each
/// node, from the leader's view of their state reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClusterCapacity {
    /// Nodes with a fresh report, by name.
    pub nodes: Vec<NodeCapacity>,
    /// Live members whose report is missing or stale, so they're left out
    /// of `nodes` and of every total.
    pub not_reporting: Vec<String>,
}

/// One node's committed and schedulable resources.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NodeCapacity {
    /// The node.
    pub node: String,
    /// CPU requested by running instances and job work, millicores.
    pub cpu_allocated_millicores: u64,
    /// Schedulable CPU, millicores.
    pub cpu_total_millicores: u64,
    /// Memory requested, MiB.
    pub memory_allocated_mb: u64,
    /// Schedulable memory, MiB.
    pub memory_total_mb: u64,
    /// GPUs requested by running instances.
    pub gpu_allocated: u32,
    /// GPUs the node detected, when `[resources] gpu_enabled` is set.
    pub gpu_total: u32,
    /// Whether a cluster upgrade has cordoned it: it takes no new work.
    pub cordoned: bool,
}

impl CapacitySection {
    /// The cluster's totals over every reporting node.
    pub fn from_nodes(nodes: &[NodeCapacity]) -> Self {
        let mut section = CapacitySection {
            nodes_reporting: u32::try_from(nodes.len()).unwrap_or(u32::MAX),
            ..CapacitySection::default()
        };
        for node in nodes {
            section.cpu_allocated_millicores += node.cpu_allocated_millicores;
            section.cpu_total_millicores += node.cpu_total_millicores;
            section.memory_allocated_mb += node.memory_allocated_mb;
            section.memory_total_mb += node.memory_total_mb;
            section.gpu_allocated += node.gpu_allocated;
            section.gpu_total += node.gpu_total;
        }
        section
    }
}

/// `GET /v1/cluster/summary/node`: one node's share of the summary, read
/// from its own stores. The node assembling the summary asks every peer
/// for this; nothing in it grows with the cluster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeSummary {
    /// The node.
    pub node: String,
    /// Its build version.
    pub version: String,
    /// Its own view of the council.
    pub council: NodeCouncilView,
    /// Its instances, counted per app.
    pub apps: Vec<AppInstances>,
    /// The newest log line it ingested, Unix milliseconds.
    pub newest_log_unix_ms: Option<u64>,
    /// Its newest metrics sample and the node-level values in it.
    pub usage: NodeUsage,
    /// The fullest of its data paths, percent; `None` when it couldn't
    /// read them.
    pub data_disk_percent: Option<f64>,
    /// Smoker faults it holds.
    pub faults: Vec<FaultBrief>,
    /// Alerts firing on it.
    pub alerts: Vec<String>,
    /// When its node certificate expires, Unix seconds.
    pub certificate_not_after: Option<u64>,
}

/// One node's view of the Raft council, as much of it as the summary
/// needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCouncilView {
    /// The leader it names.
    pub leader: Option<String>,
    /// Its Raft term.
    pub term: u64,
    /// Its last applied log index.
    pub last_applied: Option<u64>,
    /// The voters it holds.
    pub voters: Vec<String>,
    /// Whether a newer council fenced it out.
    pub fenced: bool,
}

/// One app's instances on one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInstances {
    /// The app.
    pub app: String,
    /// Its namespace.
    pub namespace: String,
    /// Instances running.
    pub running: u32,
    /// Instances in any other state.
    pub not_running: u32,
}

/// A node's newest node-level metrics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeUsage {
    /// When the newest sample was taken, Unix milliseconds.
    pub newest_sample_unix_ms: Option<u64>,
    /// CPU in use, percent.
    pub cpu_percent: Option<f64>,
    /// Memory in use, bytes.
    pub memory_used_bytes: Option<f64>,
    /// Memory total, bytes.
    pub memory_total_bytes: Option<f64>,
    /// Disk in use across its disks, bytes.
    pub disk_used_bytes: Option<f64>,
    /// Disk total across its disks, bytes.
    pub disk_total_bytes: Option<f64>,
}

/// `GET /v1/cluster/summary/leader`: the parts of the summary only the
/// Raft leader can vouch for. A node outside Raft holds an empty desired
/// state, so the assembling node asks the leader for these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaderView {
    /// Committed resources per node.
    pub capacity: ClusterCapacity,
    /// Every app the council knows, with its desired replicas.
    pub apps: Vec<crate::bun::diagnostics::DesiredAppEvidence>,
    /// Whether a cluster upgrade is under way.
    pub upgrade_in_progress: bool,
    /// GitOps sync.
    pub gitops: GitOpsSection,
    /// Job runs.
    pub jobs: Vec<JobRun>,
}

/// One job run or schedule, as the leader holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRun {
    /// The job's name.
    pub name: String,
    /// Its namespace.
    pub namespace: String,
    /// Where it is.
    pub state: JobRunState,
    /// Tasks waiting for a node.
    pub tasks_queued: u64,
    /// Tasks nodes hold.
    pub tasks_running: u64,
    /// When it finished, Unix seconds; `None` while it runs.
    pub finished_at_unix_seconds: Option<u64>,
}

/// Where a job run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobRunState {
    /// Running or stopping.
    Running,
    /// Every task succeeded.
    Succeeded,
    /// It failed, or finished with failed tasks.
    Failed,
    /// It was cancelled.
    Cancelled,
    /// A definition waiting for its schedule.
    Scheduled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_node_answering_is_complete() {
        assert_eq!(Source::from_coverage(12, 12), Source::Complete);
        assert_eq!(Source::from_coverage(0, 0), Source::Complete);
    }

    #[test]
    fn some_nodes_answering_is_partial_and_none_is_missing() {
        assert_eq!(Source::from_coverage(11, 12), Source::Partial);
        assert_eq!(Source::from_coverage(0, 12), Source::Missing);
    }

    #[test]
    fn a_partial_section_keeps_its_lower_bound_and_names_who_is_missing() {
        let section = Section::from_nodes(
            JobsSection {
                running: 3,
                ..JobsSection::default()
            },
            Coverage {
                answered: 11,
                asked: 12,
            },
            &["node-12: timed out".to_string()],
        );
        assert_eq!(section.source, Source::Partial);
        assert_eq!(section.reason.as_deref(), Some("node-12: timed out"));
        assert_eq!(section.value.map(|jobs| jobs.running), Some(3));
    }

    #[test]
    fn a_partial_section_without_reasons_says_how_many_answered() {
        let section = Section::from_nodes(
            JobsSection::default(),
            Coverage {
                answered: 2,
                asked: 3,
            },
            &[],
        );
        assert_eq!(section.reason.as_deref(), Some("2 of 3 nodes answered"));
    }

    #[test]
    fn a_section_nobody_answered_has_no_value_rather_than_zeros() {
        let section = Section::from_nodes(
            JobsSection::default(),
            Coverage {
                answered: 0,
                asked: 3,
            },
            &["every node timed out".to_string()],
        );
        assert_eq!(section.source, Source::Missing);
        assert_eq!(section.value, None);
    }

    #[test]
    fn a_section_serialises_its_source_in_snake_case_and_omits_empty_fields() {
        let complete = serde_json::to_value(Section::complete(AlertsSection::default())).unwrap();
        assert_eq!(complete["source"], "complete");
        assert!(complete.get("reason").is_none());
        assert!(complete.get("coverage").is_none());
        let missing =
            serde_json::to_value(Section::<AlertsSection>::not_visible("alerts")).unwrap();
        assert_eq!(missing["source"], "missing");
        assert_eq!(missing["value"], serde_json::Value::Null);
        assert_eq!(missing["reason"], "alerts needs an unscoped token");
    }

    #[test]
    fn a_section_round_trips_through_json() {
        let section = Section::from_nodes(
            LogsSection {
                newest_unix_ms: Some(2_000),
                slowest: Some(NodeTime {
                    node: "node-11".to_string(),
                    unix_ms: 1_000,
                }),
            },
            Coverage {
                answered: 2,
                asked: 3,
            },
            &["node-3: connection refused".to_string()],
        );
        let json = serde_json::to_string(&section).unwrap();
        let back: Section<LogsSection> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, section);
    }

    #[test]
    fn capacity_adds_up_every_reporting_node_gpus_included() {
        let node = |name: &str, gpu_allocated: u32, gpu_total: u32| NodeCapacity {
            node: name.to_string(),
            cpu_allocated_millicores: 1500,
            cpu_total_millicores: 4000,
            memory_allocated_mb: 1024,
            memory_total_mb: 8192,
            gpu_allocated,
            gpu_total,
            cordoned: false,
        };
        let section = CapacitySection::from_nodes(&[node("node-1", 2, 4), node("node-2", 0, 4)]);
        assert_eq!(section.nodes_reporting, 2);
        assert_eq!(section.cpu_allocated_millicores, 3000);
        assert_eq!(section.cpu_total_millicores, 8000);
        assert_eq!(section.memory_total_mb, 16384);
        assert_eq!((section.gpu_allocated, section.gpu_total), (2, 8));
    }

    #[test]
    fn an_app_that_wants_nothing_is_stopped() {
        assert_eq!(app_state(0, 0, false, false), AppState::Stopped);
        assert_eq!(app_state(0, 0, true, true), AppState::Stopped);
    }

    #[test]
    fn an_app_with_no_replicas_running_is_down_and_with_some_is_degraded() {
        assert_eq!(app_state(3, 0, false, false), AppState::Down);
        assert_eq!(app_state(3, 1, false, false), AppState::Degraded);
    }

    #[test]
    fn a_missing_node_makes_a_short_app_unknown_rather_than_down() {
        assert_eq!(app_state(3, 0, true, false), AppState::Unknown);
        assert_eq!(app_state(3, 1, true, false), AppState::Unknown);
    }

    #[test]
    fn a_finding_makes_a_full_app_troubled() {
        assert_eq!(app_state(2, 2, false, true), AppState::Troubled);
        assert_eq!(app_state(2, 2, false, false), AppState::Running);
    }

    #[test]
    fn app_states_order_worst_first() {
        let mut states = [
            AppState::Stopped,
            AppState::Running,
            AppState::Unknown,
            AppState::Troubled,
            AppState::Degraded,
            AppState::Down,
        ];
        states.sort();
        assert_eq!(
            states.iter().map(|state| state.word()).collect::<Vec<_>>(),
            [
                "down", "degraded", "troubled", "unknown", "running", "stopped"
            ]
        );
    }
}

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

pub mod verdict;

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
///
/// Its JSON Schema is written by hand, below: the derived one would repeat
/// these four fields' descriptions for each of the fourteen sections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// What every section of the summary says about itself. Each section also
/// has a `value`: what was seen, `null` exactly when `source` is `missing`.
///
/// For the schema only: each section's schema refers to this one
/// definition and adds its own `value`. It mirrors `Section`'s fields and
/// their serde attributes, and the doc comment above is what a reader of
/// `relish agent schema` sees.
#[derive(JsonSchema)]
#[allow(dead_code)] // never built; schemars reads its shape
struct SectionHeader {
    /// Whether `value` covers the whole cluster.
    source: Source,
    /// Why the section is partial or missing: which nodes didn't answer,
    /// or which read failed. `None` when it's complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// For a section read from every node, how many answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coverage: Option<Coverage>,
}

/// A section's schema: [`SectionHeader`] by reference, plus a `value` of
/// `T` or `null`. Inlined where it's used, so there's no definition per
/// section either.
impl<T: JsonSchema> JsonSchema for Section<T> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("Section_for_{}", T::schema_name()).into()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        format!("reliaburger::bun::summary::Section<{}>", T::schema_id()).into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let header = generator.subschema_for::<SectionHeader>();
        let value = generator.subschema_for::<Option<T>>();
        schemars::json_schema!({
            "allOf": [header],
            "properties": { "value": value },
            "required": ["value"],
        })
    }
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
    /// Agent leases held and proposals waiting for a person.
    pub agents: Section<AgentsSection>,
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
    /// Leaders other serving nodes name besides `leader`: a split brain
    /// when it isn't empty (#424).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_leaders: Vec<String>,
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
    /// The target service's namespace; `None` for a node fault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
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

/// Agent leases held and proposals waiting for a person, as far as the
/// caller's scope reaches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentsSection {
    /// Live leases.
    pub leases: u32,
    /// The first few, in target order, at most [`SUMMARY_LIST_MAX`].
    pub first_leases: Vec<LeaseBrief>,
    /// Proposals waiting for approval.
    pub waiting: u32,
    /// Waiting proposals that can't apply as they stand: an app they touch
    /// changed after they were made, someone else's lease covers one, or
    /// they outlived their TTL.
    pub stale: u32,
    /// The waiting proposals, stale ones first, then oldest first, at most
    /// [`SUMMARY_LIST_MAX`].
    pub first_proposals: Vec<ProposalBrief>,
}

/// One lease in a line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LeaseBrief {
    /// `app:shop/web` or `namespace:shop`.
    pub target: String,
    /// The holder's token name.
    pub holder: String,
    /// Whole seconds left when the leader answered.
    pub expires_in_seconds: u64,
}

/// One waiting proposal in a line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProposalBrief {
    /// Its number, for `relish proposal show`.
    pub id: u64,
    /// `apply`, `scale`, `rollback`, `stop`, `delete`, `restart`, `cancel_deploy`,
    /// `clear_fault` or `undo`.
    pub kind: String,
    /// The author's token name.
    pub author: String,
    /// The namespace every touched app is in.
    pub namespace: String,
    /// The touched apps' names.
    pub apps: Vec<String>,
    /// When it was made, Unix milliseconds on the leader's clock.
    pub created_at_unix_ms: u64,
    /// Why it can't apply as it stands; `None` while it still can.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
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
    /// Committed resources per node; `None` on a standalone node, which
    /// keeps no state reports.
    pub capacity: Option<ClusterCapacity>,
    /// Every app the council knows, with its desired replicas.
    pub apps: Vec<crate::bun::diagnostics::DesiredAppEvidence>,
    /// Whether a cluster upgrade is under way.
    pub upgrade_in_progress: bool,
    /// GitOps sync.
    pub gitops: GitOpsSection,
    /// Job runs.
    pub jobs: Vec<JobRun>,
    /// Every live agent lease.
    pub leases: Vec<crate::proposal::lease::LeaseView>,
    /// Every proposal waiting for approval.
    pub proposals: Vec<ProposalBrief>,
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

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

/// Everything the assembling node collected, already trimmed to what the
/// caller's token may see. [`assemble`] turns it into a [`ClusterSummary`]
/// without any I/O, so the tests drive it with fixtures.
#[derive(Debug, Clone)]
pub struct SummaryInputs {
    /// When the node assembled it, Unix milliseconds.
    pub now_unix_ms: u64,
    /// The node assembling it.
    pub answered_by: String,
    /// That node's build version.
    pub version: String,
    /// What the caller may see.
    pub visibility: Visibility,
    /// Every member gossip knows, down ones included; `Err` when the
    /// listing failed.
    pub members: Result<Vec<crate::bun::agent::NodeStatus>, String>,
    /// The assembling node's gossip counters; `None` without gossip.
    pub gossip: Option<GossipCounters>,
    /// The shares of the nodes that answered, the assembling node's own
    /// included.
    pub shares: Vec<NodeSummary>,
    /// The nodes that didn't answer in time, each with why.
    pub unanswered: Vec<(String, String)>,
    /// The leader's view; `Err` when no leader answered.
    pub leader: Result<LeaderView, String>,
}

/// What the caller's token may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visibility {
    /// The token is confined to some apps or namespaces, so the
    /// cluster-wide sections it can't vouch for are `missing`.
    pub scoped: bool,
    /// The caller may read cluster metrics and alerts.
    pub metrics: bool,
}

/// One node's gossip counters, copied out of [`crate::mustard::stats::GossipStats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GossipCounters {
    /// Updates received since the node started.
    pub updates_received: u64,
    /// The last membership change, Unix milliseconds.
    pub last_change_unix_ms: Option<u64>,
}

/// Assemble the summary from what was collected.
pub fn assemble(inputs: &SummaryInputs) -> ClusterSummary {
    let coverage = Coverage {
        answered: count(inputs.shares.len()),
        asked: count(inputs.shares.len() + inputs.unanswered.len()),
    };
    let reasons: Vec<String> = inputs
        .unanswered
        .iter()
        .map(|(node, reason)| format!("{node}: {reason}"))
        .collect();
    let visibility = inputs.visibility;
    let shares = &inputs.shares;
    let leader = inputs.leader.as_ref();
    let leader_missing = |what: &str| -> String {
        match leader {
            Err(error) => format!("{what} comes from the leader: {error}"),
            Ok(_) => String::new(),
        }
    };

    ClusterSummary {
        generated_at_unix_ms: inputs.now_unix_ms,
        answered_by: inputs.answered_by.clone(),
        version: inputs.version.clone(),
        scoped: visibility.scoped,
        nodes: match &inputs.members {
            Ok(members) => Section::from_nodes(
                nodes_section(members, shares, leader.ok()),
                coverage,
                &reasons,
            ),
            Err(error) => Section::missing(format!("gossip membership: {error}")),
        },
        council: council_section(shares, &inputs.unanswered),
        gossip: match (&inputs.members, inputs.gossip) {
            (Ok(members), Some(counters)) => Section::complete(GossipSection {
                members: count(members.len()),
                updates_received: counters.updates_received,
                last_change_unix_ms: counters.last_change_unix_ms,
            }),
            (Err(error), _) => Section::missing(format!("gossip membership: {error}")),
            (_, None) => Section::missing("this node runs no gossip"),
        },
        versions: {
            let mut section = Section::from_nodes(
                VersionsSection {
                    versions: version_counts(shares),
                    upgrade_in_progress: leader.is_ok_and(|view| view.upgrade_in_progress),
                },
                coverage,
                &reasons,
            );
            if section.source == Source::Complete && leader.is_err() {
                section.source = Source::Partial;
                section.reason = Some(leader_missing("whether an upgrade runs"));
            }
            section
        },
        capacity: if visibility.scoped {
            Section::not_visible("capacity")
        } else {
            match leader {
                Ok(LeaderView {
                    capacity: Some(capacity),
                    ..
                }) => capacity_section(capacity),
                Ok(_) => Section::missing("a standalone node keeps no state reports"),
                Err(_) => Section::missing(leader_missing("capacity")),
            }
        },
        usage: if visibility.scoped || !visibility.metrics {
            Section::not_visible("usage")
        } else {
            Section::from_nodes(usage_section(shares), coverage, &reasons)
        },
        apps: match leader {
            Ok(view) => Section::from_nodes(
                apps_section(&view.apps, shares, !inputs.unanswered.is_empty()),
                coverage,
                &reasons,
            ),
            Err(_) => Section::missing(leader_missing("the app list")),
        },
        instances: match leader {
            Ok(view) => {
                Section::from_nodes(instances_section(&view.apps, shares), coverage, &reasons)
            }
            Err(_) => Section::missing(leader_missing("desired replicas")),
        },
        jobs: match leader {
            Ok(view) => Section::complete(jobs_section(&view.jobs, inputs.now_unix_ms / 1000)),
            Err(_) => Section::missing(leader_missing("jobs")),
        },
        logs: if visibility.scoped {
            Section::not_visible("logs")
        } else {
            Section::from_nodes(logs_section(shares), coverage, &reasons)
        },
        faults: Section::from_nodes(faults_section(shares), coverage, &reasons),
        alerts: if visibility.scoped || !visibility.metrics {
            Section::not_visible("alerts")
        } else {
            Section::from_nodes(alerts_section(shares), coverage, &reasons)
        },
        certs: Section::from_nodes(certs_section(shares), coverage, &reasons),
        gitops: match leader {
            Ok(view) => Section::complete(view.gitops.clone()),
            Err(_) => Section::missing(leader_missing("GitOps")),
        },
        agents: match leader {
            Ok(view) => Section::complete(agents_section(&view.leases, &view.proposals)),
            Err(_) => Section::missing(leader_missing("the list of leases and proposals")),
        },
    }
}

fn count(length: usize) -> u32 {
    u32::try_from(length).unwrap_or(u32::MAX)
}

fn nodes_section(
    members: &[crate::bun::agent::NodeStatus],
    shares: &[NodeSummary],
    leader: Option<&LeaderView>,
) -> NodesSection {
    let state = |name: &str| count(members.iter().filter(|m| m.state == name).count());
    NodesSection {
        alive: state("alive"),
        suspect: state("suspect"),
        dead: state("dead"),
        left: state("left"),
        cordoned: leader
            .and_then(|view| view.capacity.as_ref())
            .map_or(0, |capacity| {
                count(capacity.nodes.iter().filter(|n| n.cordoned).count())
            }),
        disk_pressure: count(
            shares
                .iter()
                .filter(|share| {
                    share
                        .data_disk_percent
                        .is_some_and(|percent| percent >= DISK_PRESSURE_PERCENT)
                })
                .count(),
        ),
    }
}

/// The council as its leader's own answer describes it, checked against
/// which voters answered at all.
fn council_section(
    shares: &[NodeSummary],
    unanswered: &[(String, String)],
) -> Section<CouncilSection> {
    if shares.is_empty() {
        return Section::missing("no node answered");
    }
    let serving: Vec<&NodeSummary> = shares.iter().filter(|s| !s.council.fenced).collect();
    // The leader most serving nodes name; ties go to the newest term.
    let mut named: std::collections::BTreeMap<&str, (usize, u64)> = Default::default();
    for share in &serving {
        if let Some(leader) = &share.council.leader {
            let entry = named.entry(leader.as_str()).or_default();
            entry.0 += 1;
            entry.1 = entry.1.max(share.council.term);
        }
    }
    let leader = named
        .iter()
        .max_by_key(|(_, (votes, term))| (*votes, *term))
        .map(|(name, _)| (*name).to_string());
    let authority = leader
        .as_deref()
        .and_then(|name| serving.iter().find(|s| s.node == name))
        .or_else(|| serving.iter().find(|s| !s.council.voters.is_empty()));
    let Some(authority) = authority else {
        // Nobody holds a council: a standalone node.
        return Section::complete(CouncilSection {
            leader: None,
            term: 0,
            voters: 0,
            voters_answering: 0,
            quorum_ok: true,
            lagging: 0,
            other_leaders: Vec::new(),
        });
    };
    let other_leaders: Vec<String> = named
        .keys()
        .filter(|name| Some(**name) != leader.as_deref())
        .map(|name| (*name).to_string())
        .collect();
    let voters = &authority.council.voters;
    let answering: Vec<&&NodeSummary> = serving
        .iter()
        .filter(|s| voters.contains(&s.node))
        .collect();
    let furthest = answering
        .iter()
        .filter_map(|s| s.council.last_applied)
        .max()
        .unwrap_or(0);
    let lagging = answering
        .iter()
        .filter(|s| {
            s.council
                .last_applied
                .is_some_and(|applied| furthest.saturating_sub(applied) > LAGGING_ENTRIES)
        })
        .count();
    let section = CouncilSection {
        leader,
        term: serving.iter().map(|s| s.council.term).max().unwrap_or(0),
        voters: count(voters.len()),
        voters_answering: count(answering.len()),
        quorum_ok: answering.len() > voters.len() / 2,
        lagging: count(lagging),
        other_leaders,
    };
    let missing: Vec<String> = voters
        .iter()
        .filter(|voter| !answering.iter().any(|s| &&s.node == voter))
        .map(|voter| {
            let why = unanswered
                .iter()
                .find(|(node, _)| node == voter)
                .map_or("fenced or not answering", |(_, reason)| reason.as_str());
            format!("voter {voter}: {why}")
        })
        .collect();
    Section::from_nodes(
        section,
        Coverage {
            answered: count(answering.len()),
            asked: count(voters.len()),
        },
        &missing,
    )
}

fn version_counts(shares: &[NodeSummary]) -> Vec<VersionCount> {
    let mut counts: std::collections::BTreeMap<&str, u32> = Default::default();
    for share in shares {
        *counts.entry(share.version.as_str()).or_default() += 1;
    }
    let mut versions: Vec<VersionCount> = counts
        .into_iter()
        .map(|(version, nodes)| VersionCount {
            version: version.to_string(),
            nodes,
        })
        .collect();
    versions.sort_by(|a, b| {
        b.nodes
            .cmp(&a.nodes)
            .then_with(|| a.version.cmp(&b.version))
    });
    versions
}

fn capacity_section(capacity: &ClusterCapacity) -> Section<CapacitySection> {
    let reporting = count(capacity.nodes.len());
    let reasons: Vec<String> = capacity
        .not_reporting
        .iter()
        .map(|node| format!("{node}: no fresh state report"))
        .collect();
    Section::from_nodes(
        CapacitySection::from_nodes(&capacity.nodes),
        Coverage {
            answered: reporting,
            asked: reporting + count(capacity.not_reporting.len()),
        },
        &reasons,
    )
}

fn percent(used: f64, total: f64) -> Option<f64> {
    (total > 0.0).then(|| used / total * 100.0)
}

fn usage_section(shares: &[NodeSummary]) -> UsageSection {
    let cpu: Vec<(&str, f64)> = shares
        .iter()
        .filter_map(|s| s.usage.cpu_percent.map(|cpu| (s.node.as_str(), cpu)))
        .collect();
    let sum = |field: fn(&NodeUsage) -> Option<f64>| -> f64 {
        shares.iter().filter_map(|s| field(&s.usage)).sum()
    };
    let mut busiest: Vec<NodePercent> = cpu
        .iter()
        .map(|(node, percent)| NodePercent {
            node: (*node).to_string(),
            percent: *percent,
        })
        .collect();
    busiest.sort_by(|a, b| {
        b.percent
            .total_cmp(&a.percent)
            .then_with(|| a.node.cmp(&b.node))
    });
    busiest.truncate(SUMMARY_LIST_MAX);
    UsageSection {
        cpu_percent: (!cpu.is_empty())
            .then(|| cpu.iter().map(|(_, percent)| percent).sum::<f64>() / cpu.len() as f64),
        memory_percent: percent(sum(|u| u.memory_used_bytes), sum(|u| u.memory_total_bytes)),
        disk_percent: percent(sum(|u| u.disk_used_bytes), sum(|u| u.disk_total_bytes)),
        busiest,
        newest_sample_unix_ms: shares
            .iter()
            .filter_map(|s| s.usage.newest_sample_unix_ms)
            .max(),
    }
}

/// Instances running per app, summed over the nodes that answered.
fn running_per_app(shares: &[NodeSummary]) -> std::collections::HashMap<(&str, &str), u32> {
    let mut running = std::collections::HashMap::new();
    for app in shares.iter().flat_map(|s| &s.apps) {
        *running
            .entry((app.app.as_str(), app.namespace.as_str()))
            .or_default() += app.running;
    }
    running
}

fn apps_section(
    desired: &[crate::bun::diagnostics::DesiredAppEvidence],
    shares: &[NodeSummary],
    nodes_unanswered: bool,
) -> AppsSection {
    let running = running_per_app(shares);
    let mut section = AppsSection {
        total: count(desired.len()),
        ..AppsSection::default()
    };
    let mut worst = Vec::new();
    for app in desired {
        let running = running
            .get(&(app.app.as_str(), app.namespace.as_str()))
            .copied()
            .unwrap_or(0);
        let state = app_state(app.desired_replicas, running, nodes_unanswered, false);
        match state {
            AppState::Down => section.down += 1,
            AppState::Degraded => section.degraded += 1,
            AppState::Unknown => section.unknown += 1,
            AppState::Running | AppState::Troubled => section.running += 1,
            AppState::Stopped => section.stopped += 1,
        }
        if app.unschedulable.is_some() {
            section.unschedulable += 1;
        }
        if state < AppState::Running {
            worst.push(AppBrief {
                app: app.app.clone(),
                namespace: app.namespace.clone(),
                state,
                desired: app.desired_replicas,
                running,
            });
        }
    }
    worst.sort_by(|a, b| (a.state, &a.namespace, &a.app).cmp(&(b.state, &b.namespace, &b.app)));
    worst.truncate(SUMMARY_LIST_MAX);
    section.worst = worst;
    section
}

fn instances_section(
    desired: &[crate::bun::diagnostics::DesiredAppEvidence],
    shares: &[NodeSummary],
) -> InstancesSection {
    let apps = shares.iter().flat_map(|s| &s.apps);
    InstancesSection {
        running: apps.clone().map(|app| app.running).sum(),
        desired: desired.iter().map(|app| app.desired_replicas).sum(),
        not_running: apps.map(|app| app.not_running).sum(),
    }
}

/// How far back "the last hour" reaches, in seconds.
const LAST_HOUR_SECONDS: u64 = 3600;

fn jobs_section(runs: &[JobRun], now_unix_seconds: u64) -> JobsSection {
    let recent = |run: &JobRun| {
        run.finished_at_unix_seconds
            .is_some_and(|at| now_unix_seconds.saturating_sub(at) <= LAST_HOUR_SECONDS)
    };
    let mut section = JobsSection::default();
    for run in runs {
        match run.state {
            JobRunState::Running => {
                section.running += 1;
                section.tasks_queued += run.tasks_queued;
                section.tasks_running += run.tasks_running;
            }
            JobRunState::Succeeded if recent(run) => section.succeeded_last_hour += 1,
            JobRunState::Failed if recent(run) => section.failed_last_hour += 1,
            JobRunState::Scheduled => section.scheduled += 1,
            JobRunState::Succeeded | JobRunState::Failed | JobRunState::Cancelled => {}
        }
    }
    section
}

fn logs_section(shares: &[NodeSummary]) -> LogsSection {
    let newest: Vec<NodeTime> = shares
        .iter()
        .filter_map(|s| {
            s.newest_log_unix_ms.map(|unix_ms| NodeTime {
                node: s.node.clone(),
                unix_ms,
            })
        })
        .collect();
    LogsSection {
        newest_unix_ms: newest.iter().map(|n| n.unix_ms).max(),
        slowest: newest
            .iter()
            .min_by(|a, b| a.unix_ms.cmp(&b.unix_ms).then_with(|| a.node.cmp(&b.node)))
            .cloned(),
    }
}

fn faults_section(shares: &[NodeSummary]) -> FaultsSection {
    let mut faults: Vec<FaultBrief> = shares.iter().flat_map(|s| s.faults.clone()).collect();
    let active = count(faults.len());
    faults.sort_by(|a, b| {
        (a.remaining_seconds, &a.target, &a.kind).cmp(&(b.remaining_seconds, &b.target, &b.kind))
    });
    faults.truncate(SUMMARY_LIST_MAX);
    FaultsSection {
        active,
        soonest: faults,
    }
}

fn alerts_section(shares: &[NodeSummary]) -> AlertsSection {
    let mut alerts: Vec<String> = shares.iter().flat_map(|s| s.alerts.clone()).collect();
    alerts.sort();
    let firing = count(alerts.len());
    alerts.truncate(SUMMARY_LIST_MAX);
    AlertsSection {
        firing,
        first: alerts,
    }
}

fn certs_section(shares: &[NodeSummary]) -> CertsSection {
    let certificates: Vec<(&str, u64)> = shares
        .iter()
        .filter_map(|s| s.certificate_not_after.map(|at| (s.node.as_str(), at)))
        .collect();
    CertsSection {
        count: count(certificates.len()),
        soonest: certificates
            .iter()
            .min_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)))
            .map(|(node, at)| CertBrief {
                subject: format!("node {node}"),
                not_after_unix_seconds: *at,
            }),
    }
}

fn agents_section(
    leases: &[crate::proposal::lease::LeaseView],
    proposals: &[ProposalBrief],
) -> AgentsSection {
    let mut waiting: Vec<ProposalBrief> = proposals.to_vec();
    waiting.sort_by_key(|proposal| (proposal.stale.is_none(), proposal.id));
    let stale = count(waiting.iter().filter(|p| p.stale.is_some()).count());
    waiting.truncate(SUMMARY_LIST_MAX);
    AgentsSection {
        leases: count(leases.len()),
        first_leases: leases
            .iter()
            .take(SUMMARY_LIST_MAX)
            .map(|lease| LeaseBrief {
                target: lease.target.clone(),
                holder: lease.holder.clone(),
                expires_in_seconds: lease.expires_in_seconds,
            })
            .collect(),
        waiting: count(proposals.len()),
        stale,
        first_proposals: waiting,
    }
}

/// Every proposal in `desired` waiting for approval at `now_unix_ms`, each
/// saying whether it's stale, for the leader's view.
pub fn waiting_proposals(
    desired: &crate::council::types::DesiredState,
    now_unix_ms: u64,
) -> Vec<ProposalBrief> {
    desired
        .proposals
        .pending()
        .map(|proposal| ProposalBrief {
            id: proposal.id.0,
            kind: proposal.kind.name().to_string(),
            author: proposal.author.name.clone(),
            namespace: proposal.namespace.clone(),
            apps: proposal.apps().map(|app| app.name.clone()).collect(),
            created_at_unix_ms: proposal.created_at_unix_ms,
            stale: staleness(desired, proposal, now_unix_ms),
        })
        .collect()
}

/// Why a waiting proposal can't apply as it stands, or `None` while it
/// still can: the same causes that make approving it fail. It applies only
/// while every touched app's revision still equals the one it saw, no one
/// else's lease covers one of them and it touches no more than it showed;
/// one past its TTL should already have been expired by the reaper.
pub(crate) fn staleness(
    desired: &crate::council::types::DesiredState,
    proposal: &crate::proposal::Proposal,
    now_unix_ms: u64,
) -> Option<String> {
    if let Some(change) = proposal.changes.iter().find(|change| {
        crate::proposal::revision::current_revision(desired, &change.app) != change.before.revision
    }) {
        return Some(format!("{} changed after it was made", change.app));
    }
    // Its own pinned lease keeps others off, until a person breaks it.
    if let Some((app, lease)) = proposal.apps().find_map(|app| {
        desired
            .agent_leases
            .blocker(app, &proposal.author.principal_id, now_unix_ms)
            .map(|lease| (app, lease))
    }) {
        return Some(format!("{app} is leased by {}", lease.holder.name));
    }
    // Approving it would fail too: it touches more than it showed (#1682).
    if let Some(grew) = crate::proposal::build::blast_radius_growth(desired, proposal) {
        return Some(grew);
    }
    (proposal.expires_at_unix_ms <= now_unix_ms)
        .then(|| "past its TTL, and the reaper hasn't expired it".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_node_answering_is_complete() {
        assert_eq!(Source::from_coverage(12, 12), Source::Complete);
        assert_eq!(Source::from_coverage(0, 0), Source::Complete);
    }

    fn summary_schema() -> serde_json::Value {
        schemars::generate::SchemaSettings::default()
            .for_serialize()
            .into_generator()
            .into_root_schema_for::<ClusterSummary>()
            .to_value()
    }

    #[test]
    fn the_schema_describes_what_every_section_shares_once() {
        let schema = summary_schema();
        let definitions = schema["$defs"].as_object().unwrap();
        let sections: Vec<&String> = definitions
            .keys()
            .filter(|name| name.starts_with("Section") && *name != "SectionHeader")
            .collect();
        assert!(
            sections.is_empty(),
            "one definition per section: {sections:?}"
        );
        let header = &definitions["SectionHeader"];
        assert!(header["properties"]["source"].is_object(), "{header}");
        assert!(header["properties"]["coverage"].is_object(), "{header}");
        assert_eq!(header["required"], serde_json::json!(["source"]));

        let nodes = &schema["properties"]["nodes"];
        assert_eq!(
            nodes["allOf"][0]["$ref"], "#/$defs/SectionHeader",
            "{nodes}"
        );
        let value = nodes["properties"]["value"].to_string();
        assert!(value.contains("#/$defs/NodesSection"), "{value}");
        assert!(value.contains("null"), "absent never means zero: {value}");
        assert_eq!(nodes["required"], serde_json::json!(["value"]), "{nodes}");
        assert_eq!(
            nodes["description"], "Gossip membership by state.",
            "the field keeps its own description"
        );
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

    // -- assembly ---------------------------------------------------------

    const NOW_MS: u64 = 1_800_000_000_000;

    fn member(name: &str, state: &str) -> crate::bun::agent::NodeStatus {
        crate::bun::agent::NodeStatus {
            node_id: name.to_string(),
            address: "127.0.0.1:9000".to_string(),
            api_address: None,
            state: state.to_string(),
            incarnation: 1,
            is_council: false,
            is_leader: false,
            labels: Default::default(),
        }
    }

    fn share(name: &str, apps: &[(&str, u32)]) -> NodeSummary {
        NodeSummary {
            node: name.to_string(),
            version: "0.2.8".to_string(),
            council: NodeCouncilView {
                leader: Some("node-1".to_string()),
                term: 7,
                last_applied: Some(500),
                voters: vec![
                    "node-1".to_string(),
                    "node-2".to_string(),
                    "node-3".to_string(),
                ],
                fenced: false,
            },
            apps: apps
                .iter()
                .map(|(app, running)| AppInstances {
                    app: (*app).to_string(),
                    namespace: "default".to_string(),
                    running: *running,
                    not_running: 0,
                })
                .collect(),
            newest_log_unix_ms: Some(NOW_MS - 2_000),
            usage: NodeUsage {
                newest_sample_unix_ms: Some(NOW_MS - 6_000),
                cpu_percent: Some(30.0),
                memory_used_bytes: Some(4.0),
                memory_total_bytes: Some(8.0),
                disk_used_bytes: Some(1.0),
                disk_total_bytes: Some(4.0),
            },
            data_disk_percent: Some(40.0),
            faults: Vec::new(),
            alerts: Vec::new(),
            certificate_not_after: Some(1_900_000_000),
        }
    }

    fn desired(app: &str, replicas: u32) -> crate::bun::diagnostics::DesiredAppEvidence {
        crate::bun::diagnostics::DesiredAppEvidence {
            app: app.to_string(),
            namespace: "default".to_string(),
            desired_replicas: replicas,
            scheduled_replicas: replicas,
            placements: Default::default(),
            service_port: None,
            blocked: None,
            volume_home_away: None,
            unschedulable: None,
            volume_homes: Vec::new(),
            rollout_failed: None,
            owner: None,
        }
    }

    fn node_capacity(name: &str) -> NodeCapacity {
        NodeCapacity {
            node: name.to_string(),
            cpu_allocated_millicores: 1000,
            cpu_total_millicores: 4000,
            memory_allocated_mb: 1024,
            memory_total_mb: 8192,
            gpu_allocated: 1,
            gpu_total: 2,
            cordoned: false,
        }
    }

    fn leader_view() -> LeaderView {
        LeaderView {
            capacity: Some(ClusterCapacity {
                nodes: vec![
                    node_capacity("node-1"),
                    node_capacity("node-2"),
                    node_capacity("node-3"),
                ],
                not_reporting: Vec::new(),
            }),
            apps: vec![desired("api", 3), desired("web", 3), desired("batch", 0)],
            upgrade_in_progress: false,
            gitops: GitOpsSection {
                configured: false,
                commit: None,
                synced_at_unix_seconds: None,
                error: None,
            },
            jobs: Vec::new(),
            leases: Vec::new(),
            proposals: Vec::new(),
        }
    }

    /// Three healthy nodes, each running one replica of `api` and `web`.
    fn healthy() -> SummaryInputs {
        SummaryInputs {
            now_unix_ms: NOW_MS,
            answered_by: "node-1".to_string(),
            version: "0.2.8".to_string(),
            visibility: Visibility {
                scoped: false,
                metrics: true,
            },
            members: Ok(vec![
                member("node-1", "alive"),
                member("node-2", "alive"),
                member("node-3", "alive"),
            ]),
            gossip: Some(GossipCounters {
                updates_received: 42,
                last_change_unix_ms: Some(NOW_MS - 240_000),
            }),
            shares: ["node-1", "node-2", "node-3"]
                .iter()
                .map(|name| share(name, &[("api", 1), ("web", 1)]))
                .collect(),
            unanswered: Vec::new(),
            leader: Ok(leader_view()),
        }
    }

    fn sources(summary: &ClusterSummary) -> Vec<(&'static str, Source)> {
        vec![
            ("nodes", summary.nodes.source),
            ("council", summary.council.source),
            ("gossip", summary.gossip.source),
            ("versions", summary.versions.source),
            ("capacity", summary.capacity.source),
            ("usage", summary.usage.source),
            ("apps", summary.apps.source),
            ("instances", summary.instances.source),
            ("jobs", summary.jobs.source),
            ("logs", summary.logs.source),
            ("faults", summary.faults.source),
            ("alerts", summary.alerts.source),
            ("certs", summary.certs.source),
            ("gitops", summary.gitops.source),
            ("agents", summary.agents.source),
        ]
    }

    #[test]
    fn a_healthy_cluster_is_complete_everywhere() {
        let summary = assemble(&healthy());
        for (name, source) in sources(&summary) {
            assert_eq!(source, Source::Complete, "{name}");
        }
        let apps = summary.apps.value.unwrap();
        assert_eq!((apps.total, apps.running, apps.stopped), (3, 2, 1));
        assert!(apps.worst.is_empty());
        let instances = summary.instances.value.unwrap();
        assert_eq!((instances.running, instances.desired), (6, 6));
        let council = summary.council.value.unwrap();
        assert_eq!(council.leader.as_deref(), Some("node-1"));
        assert_eq!((council.voters, council.voters_answering), (3, 3));
        assert!(council.quorum_ok);
        let gossip = summary.gossip.value.unwrap();
        assert_eq!((gossip.members, gossip.updates_received), (3, 42));
        let versions = summary.versions.value.unwrap();
        assert_eq!(
            versions.versions,
            [VersionCount {
                version: "0.2.8".to_string(),
                nodes: 3
            }]
        );
    }

    #[test]
    fn a_node_that_did_not_answer_makes_the_fanned_sections_partial() {
        let mut inputs = healthy();
        inputs.shares.pop();
        inputs.unanswered = vec![("node-3".to_string(), "timed out".to_string())];
        let summary = assemble(&inputs);
        assert_eq!(summary.apps.source, Source::Partial);
        assert_eq!(
            summary.apps.coverage,
            Some(Coverage {
                answered: 2,
                asked: 3
            })
        );
        assert_eq!(summary.apps.reason.as_deref(), Some("node-3: timed out"));
        // Two of three replicas seen: unknown, not degraded.
        let apps = summary.apps.value.unwrap();
        assert_eq!(apps.unknown, 2);
        assert_eq!(apps.degraded, 0);
        let council = summary.council.value.as_ref().unwrap();
        assert_eq!(summary.council.source, Source::Partial);
        assert_eq!(council.voters_answering, 2);
        assert!(council.quorum_ok);
        assert_eq!(
            summary.council.reason.as_deref(),
            Some("voter node-3: timed out")
        );
        // Gossip and capacity come from one place each, which answered.
        assert_eq!(summary.gossip.source, Source::Complete);
        assert_eq!(summary.capacity.source, Source::Complete);
    }

    #[test]
    fn short_apps_are_down_or_degraded_and_listed_worst_first() {
        let mut inputs = healthy();
        inputs.shares = ["node-1", "node-2", "node-3"]
            .iter()
            .map(|name| share(name, &[("web", u32::from(*name == "node-1"))]))
            .collect();
        let apps = assemble(&inputs).apps.value.unwrap();
        assert_eq!((apps.down, apps.degraded), (1, 1));
        let worst: Vec<(&str, AppState)> = apps
            .worst
            .iter()
            .map(|app| (app.app.as_str(), app.state))
            .collect();
        assert_eq!(
            worst,
            [("api", AppState::Down), ("web", AppState::Degraded)]
        );
    }

    #[test]
    fn without_a_leader_the_leader_sections_are_missing_not_empty() {
        let mut inputs = healthy();
        inputs.leader = Err("no leader is known".to_string());
        let summary = assemble(&inputs);
        for section in [
            summary.capacity.source,
            summary.apps.source,
            summary.instances.source,
            summary.jobs.source,
            summary.gitops.source,
            summary.agents.source,
        ] {
            assert_eq!(section, Source::Missing);
        }
        assert_eq!(summary.apps.value, None);
        assert_eq!(
            summary.apps.reason.as_deref(),
            Some("the app list comes from the leader: no leader is known")
        );
        assert_eq!(summary.versions.source, Source::Partial);
        // Node-local sections don't need the leader.
        assert_eq!(summary.logs.source, Source::Complete);
    }

    #[test]
    fn a_scoped_token_sees_no_cluster_wide_sections() {
        let mut inputs = healthy();
        inputs.visibility.scoped = true;
        let summary = assemble(&inputs);
        assert!(summary.scoped);
        for (name, section) in [
            ("capacity", &summary.capacity.reason),
            ("usage", &summary.usage.reason),
            ("logs", &summary.logs.reason),
            ("alerts", &summary.alerts.reason),
        ] {
            assert_eq!(
                section.as_deref(),
                Some(format!("{name} needs an unscoped token").as_str())
            );
        }
        assert_eq!(summary.apps.source, Source::Complete);
    }

    #[test]
    fn without_metrics_permission_usage_and_alerts_are_missing() {
        let mut inputs = healthy();
        inputs.visibility.metrics = false;
        let summary = assemble(&inputs);
        assert_eq!(summary.usage.source, Source::Missing);
        assert_eq!(summary.alerts.source, Source::Missing);
        assert_eq!(summary.logs.source, Source::Complete);
    }

    #[test]
    fn usage_averages_cpu_and_names_the_busiest_node() {
        let mut inputs = healthy();
        inputs.shares[1].usage.cpu_percent = Some(90.0);
        inputs.shares[2].usage.newest_sample_unix_ms = Some(NOW_MS - 1_000);
        let usage = assemble(&inputs).usage.value.unwrap();
        assert_eq!(usage.cpu_percent, Some(50.0));
        assert_eq!(usage.busiest[0].node, "node-2");
        assert_eq!(usage.memory_percent, Some(50.0));
        assert_eq!(usage.disk_percent, Some(25.0));
        assert_eq!(usage.newest_sample_unix_ms, Some(NOW_MS - 1_000));
    }

    #[test]
    fn logs_name_the_node_ingestion_is_behind_on() {
        let mut inputs = healthy();
        inputs.shares[2].newest_log_unix_ms = Some(NOW_MS - 9_000);
        inputs.shares[1].newest_log_unix_ms = None;
        let logs = assemble(&inputs).logs.value.unwrap();
        assert_eq!(logs.newest_unix_ms, Some(NOW_MS - 2_000));
        assert_eq!(
            logs.slowest,
            Some(NodeTime {
                node: "node-3".to_string(),
                unix_ms: NOW_MS - 9_000
            })
        );
    }

    #[test]
    fn nodes_count_states_cordons_and_disk_pressure() {
        let mut inputs = healthy();
        inputs.members = Ok(vec![
            member("node-1", "alive"),
            member("node-2", "alive"),
            member("node-3", "suspect"),
            member("node-4", "dead"),
        ]);
        inputs.shares[0].data_disk_percent = Some(91.0);
        if let Ok(view) = &mut inputs.leader {
            view.capacity.as_mut().unwrap().nodes[1].cordoned = true;
        }
        let nodes = assemble(&inputs).nodes.value.unwrap();
        assert_eq!(
            nodes,
            NodesSection {
                alive: 2,
                suspect: 1,
                dead: 1,
                left: 0,
                cordoned: 1,
                disk_pressure: 1,
            }
        );
    }

    #[test]
    fn a_lagging_voter_is_counted() {
        let mut inputs = healthy();
        inputs.shares[2].council.last_applied = Some(100);
        let council = assemble(&inputs).council.value.unwrap();
        assert_eq!(council.lagging, 1);
    }

    /// #424: a split brain looks healthy from either half, so the summary
    /// names every leader a serving node names, not just the most-named.
    #[test]
    fn a_split_council_names_its_other_leader() {
        let mut inputs = healthy();
        inputs.shares[2].council.leader = Some("node-3".to_string());
        inputs.shares[2].council.term = 9;
        let council = assemble(&inputs).council.value.unwrap();
        assert_eq!(council.leader.as_deref(), Some("node-1"));
        assert_eq!(council.other_leaders, ["node-3"]);
        let whole = assemble(&healthy()).council.value.unwrap();
        assert!(whole.other_leaders.is_empty());
    }

    #[test]
    fn a_standalone_node_has_no_council_to_lose() {
        let mut inputs = healthy();
        inputs.shares.truncate(1);
        inputs.shares[0].council = NodeCouncilView::default();
        let summary = assemble(&inputs);
        assert_eq!(summary.council.source, Source::Complete);
        let council = summary.council.value.unwrap();
        assert_eq!(council.voters, 0);
        assert!(council.quorum_ok);
    }

    #[test]
    fn capacity_is_partial_while_a_node_is_not_reporting() {
        let mut inputs = healthy();
        if let Ok(view) = &mut inputs.leader {
            view.capacity.as_mut().unwrap().not_reporting = vec!["node-4".to_string()];
        }
        let capacity = assemble(&inputs).capacity;
        assert_eq!(capacity.source, Source::Partial);
        assert_eq!(
            capacity.reason.as_deref(),
            Some("node-4: no fresh state report")
        );
        let value = capacity.value.unwrap();
        assert_eq!((value.gpu_allocated, value.gpu_total), (3, 6));
    }

    #[test]
    fn jobs_count_runs_and_only_the_last_hours_finishes() {
        let run = |state, finished: Option<u64>| JobRun {
            name: "etl".to_string(),
            namespace: "default".to_string(),
            state,
            tasks_queued: 10,
            tasks_running: 2,
            finished_at_unix_seconds: finished,
        };
        let now = NOW_MS / 1000;
        let mut inputs = healthy();
        if let Ok(view) = &mut inputs.leader {
            view.jobs = vec![
                run(JobRunState::Running, None),
                run(JobRunState::Failed, Some(now - 60)),
                run(JobRunState::Failed, Some(now - 7200)),
                run(JobRunState::Succeeded, Some(now - 10)),
                run(JobRunState::Scheduled, None),
            ];
        }
        let jobs = assemble(&inputs).jobs.value.unwrap();
        assert_eq!(
            jobs,
            JobsSection {
                running: 1,
                tasks_queued: 10,
                tasks_running: 2,
                succeeded_last_hour: 1,
                failed_last_hour: 1,
                scheduled: 1,
            }
        );
    }

    #[test]
    fn faults_and_alerts_keep_their_counts_but_list_only_a_few() {
        let mut inputs = healthy();
        inputs.shares[0].faults = (0..8)
            .map(|index| FaultBrief {
                kind: "delay".to_string(),
                target: format!("svc-{index}"),
                namespace: Some("default".to_string()),
                remaining_seconds: 100 - index,
            })
            .collect();
        inputs.shares[1].alerts = (0..7).map(|index| format!("alert-{index}")).collect();
        let summary = assemble(&inputs);
        let faults = summary.faults.value.unwrap();
        assert_eq!(faults.active, 8);
        assert_eq!(faults.soonest.len(), SUMMARY_LIST_MAX);
        assert_eq!(faults.soonest[0].target, "svc-7");
        let alerts = summary.alerts.value.unwrap();
        assert_eq!((alerts.firing, alerts.first.len()), (7, SUMMARY_LIST_MAX));
    }

    // -- agents -----------------------------------------------------------

    fn lease_view(app: &str, holder: &str) -> crate::proposal::lease::LeaseView {
        crate::proposal::lease::LeaseView {
            target: format!("app:shop/{app}"),
            kind: crate::proposal::lease::LeaseKind::App,
            namespace: "shop".to_string(),
            app: Some(app.to_string()),
            holder: holder.to_string(),
            purpose: "p99 over the SLO".to_string(),
            proposal: None,
            acquired_at_unix_ms: NOW_MS - 10_000,
            expires_at_unix_ms: NOW_MS + 110_000,
            expires_in_seconds: 110,
        }
    }

    fn proposal_brief(id: u64, stale: Option<&str>) -> ProposalBrief {
        ProposalBrief {
            id,
            kind: "scale".to_string(),
            author: "holmes".to_string(),
            namespace: "shop".to_string(),
            apps: vec!["web".to_string()],
            created_at_unix_ms: NOW_MS - 60_000,
            stale: stale.map(str::to_string),
        }
    }

    #[test]
    fn agents_count_leases_and_waiting_proposals_and_list_the_stale_first() {
        let mut inputs = healthy();
        if let Ok(view) = &mut inputs.leader {
            view.leases = (0..7)
                .map(|index| lease_view(&format!("app-{index}"), "holmes"))
                .collect();
            view.proposals = vec![
                proposal_brief(3, None),
                proposal_brief(5, Some("shop/web changed after it was made")),
                proposal_brief(4, None),
            ];
        }
        let summary = assemble(&inputs);
        assert_eq!(summary.agents.source, Source::Complete);
        let agents = summary.agents.value.unwrap();
        assert_eq!((agents.leases, agents.waiting, agents.stale), (7, 3, 1));
        assert_eq!(agents.first_leases.len(), SUMMARY_LIST_MAX);
        assert_eq!(
            agents.first_leases[0],
            LeaseBrief {
                target: "app:shop/app-0".to_string(),
                holder: "holmes".to_string(),
                expires_in_seconds: 110,
            }
        );
        let order: Vec<u64> = agents.first_proposals.iter().map(|p| p.id).collect();
        assert_eq!(order, [5, 3, 4], "stale first, then oldest");
    }

    #[test]
    fn no_leases_and_no_proposals_is_a_complete_zero() {
        let agents = assemble(&healthy()).agents;
        assert_eq!(agents.source, Source::Complete);
        assert_eq!(agents.value, Some(AgentsSection::default()));
    }

    /// The summary carries what a person needs at a glance, never the
    /// holder's own words: a lease's purpose is untrusted text.
    #[test]
    fn the_agents_section_never_carries_a_lease_purpose() {
        let mut inputs = healthy();
        if let Ok(view) = &mut inputs.leader {
            view.leases = vec![lease_view("web", "holmes")];
        }
        let json = serde_json::to_string(&assemble(&inputs).agents).unwrap();
        assert!(!json.contains("p99 over the SLO"), "{json}");
    }

    fn proposal_desired() -> crate::council::types::DesiredState {
        let spec: crate::config::app::AppSpec =
            toml::from_str("image = \"web:1\"\nreplicas = 3\nnamespace = \"shop\"").unwrap();
        let mut desired = crate::council::types::DesiredState::default();
        desired
            .apps
            .insert(crate::meat::AppId::new("web", "shop"), spec);
        let author = crate::proposal::Principal {
            name: "holmes".to_string(),
            principal_id: "token:holmes".to_string(),
            kind: crate::sesame::types::TokenKind::Agent,
            role: crate::sesame::types::ApiRole::Deployer,
        };
        let request = crate::proposal::ProposalRequest {
            kind: crate::proposal::RequestKind::Scale,
            namespace: Some("shop".to_string()),
            app: Some("web".to_string()),
            replicas: Some(5),
            config: None,
            of: None,
            fault: None,
            node: None,
            reason: "p99 over the SLO".to_string(),
            ttl_seconds: None,
            idempotency_key: None,
        };
        let proposal = crate::proposal::build::build(
            &desired,
            &request,
            &author,
            NOW_MS,
            crate::proposal::build::as_declared,
        )
        .unwrap();
        let snapshot = desired.clone();
        let outcome = desired.proposals.create(
            &mut desired.agent_leases,
            proposal,
            |app| crate::proposal::revision::current_revision(&snapshot, app),
            NOW_MS,
        );
        assert!(
            matches!(outcome, crate::proposal::ProposalOutcome::Created { .. }),
            "{outcome:?}"
        );
        desired
    }

    #[test]
    fn a_fresh_waiting_proposal_is_not_stale() {
        let waiting = waiting_proposals(&proposal_desired(), NOW_MS + 1_000);
        assert_eq!(waiting.len(), 1);
        let brief = &waiting[0];
        assert_eq!(
            (
                brief.kind.as_str(),
                brief.author.as_str(),
                brief.namespace.as_str()
            ),
            ("scale", "holmes", "shop")
        );
        assert_eq!(brief.apps, ["web"]);
        assert_eq!(brief.stale, None);
    }

    /// Approving a proposal whose app moved fails, so the summary says so
    /// before anyone tries.
    #[test]
    fn a_proposal_whose_app_changed_since_is_stale() {
        let mut desired = proposal_desired();
        let web = crate::meat::AppId::new("web", "shop");
        desired.apps.get_mut(&web).unwrap().image = Some("web:2".to_string());
        let waiting = waiting_proposals(&desired, NOW_MS + 1_000);
        assert_eq!(
            waiting[0].stale.as_deref(),
            Some("shop/web changed after it was made")
        );
    }

    #[test]
    fn a_proposal_past_its_ttl_that_is_still_waiting_is_stale() {
        let desired = proposal_desired();
        let expires = desired
            .proposals
            .pending()
            .next()
            .unwrap()
            .expires_at_unix_ms;
        let waiting = waiting_proposals(&desired, expires);
        assert_eq!(
            waiting[0].stale.as_deref(),
            Some("past its TTL, and the reaper hasn't expired it")
        );
    }

    /// Approving fails while someone else's lease covers an app the
    /// proposal touches, so the summary says so before anyone tries. Its
    /// own pinned lease keeps others off until a person breaks it.
    #[test]
    fn a_proposal_another_holders_lease_blocks_is_stale() {
        use crate::proposal::lease::{LeaseHolder, LeaseRequest, LeaseTarget};
        let mut desired = proposal_desired();
        let now = NOW_MS + 1_000;
        desired
            .agent_leases
            .break_lease(&LeaseTarget::app("shop", "web"), now)
            .unwrap();
        desired
            .agent_leases
            .take(
                &LeaseRequest {
                    target: LeaseTarget::namespace("shop"),
                    holder: LeaseHolder {
                        name: "capacity-bot".to_string(),
                        principal_id: "token:capacity-bot".to_string(),
                    },
                    purpose: "rebalancing".to_string(),
                    ttl_ms: crate::proposal::lease::DEFAULT_LEASE_TTL_MS,
                },
                now,
            )
            .unwrap();
        let waiting = waiting_proposals(&desired, now + 1_000);
        assert_eq!(
            waiting[0].stale.as_deref(),
            Some("shop/web is leased by capacity-bot")
        );
    }

    #[test]
    fn only_pending_proposals_are_waiting() {
        let mut desired = proposal_desired();
        let id = desired.proposals.pending().next().unwrap().id;
        let author = desired.proposals.get(id).unwrap().author.clone();
        let desired_ref = &mut desired;
        let outcome = desired_ref.proposals.withdraw(
            &mut desired_ref.agent_leases,
            id,
            &author,
            NOW_MS + 1_000,
        );
        assert!(
            matches!(outcome, crate::proposal::ProposalOutcome::Withdrawn { .. }),
            "{outcome:?}"
        );
        assert!(waiting_proposals(&desired, NOW_MS + 2_000).is_empty());
    }

    #[test]
    fn the_summary_stays_small_on_a_large_cluster() {
        let mut inputs = healthy();
        let names: Vec<String> = (0..100).map(|index| format!("node-{index}")).collect();
        inputs.members = Ok(names.iter().map(|name| member(name, "alive")).collect());
        if let Ok(view) = &mut inputs.leader {
            view.apps = (0..1000)
                .map(|index| desired(&format!("app-{index:04}"), 1))
                .collect();
        }
        inputs.shares = names
            .iter()
            .enumerate()
            .map(|(node, name)| {
                let apps: Vec<(String, u32)> = (0..10)
                    .map(|slot| (format!("app-{:04}", node * 10 + slot), 1))
                    .collect();
                let apps: Vec<(&str, u32)> =
                    apps.iter().map(|(app, n)| (app.as_str(), *n)).collect();
                share(name, &apps)
            })
            .collect();
        let started = std::time::Instant::now();
        let summary = assemble(&inputs);
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
        let apps = summary.apps.value.as_ref().unwrap();
        assert_eq!((apps.total, apps.running), (1000, 1000));
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.len() < 8 * 1024, "{} bytes", json.len());
    }
}

use serde::{Deserialize, Serialize};

/// Evidence collected from one diagnostic source.
///
/// `Degraded` retains usable partial facts. `Unavailable` means the source
/// should have answered but did not. `Unsupported` means the current API
/// cannot make the observation honestly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Evidence<T> {
    /// A timestamped observation from the source.
    Available {
        /// Unix timestamp, in seconds, when the source was read.
        observed_at: u64,
        /// The value returned by the source.
        value: T,
    },
    /// Usable observations whose inventory or freshness is incomplete.
    Degraded {
        /// Unix timestamp, in seconds, when the source was read.
        observed_at: u64,
        /// Facts which remain safe to diagnose.
        value: T,
        /// Why the source cannot support an unconditional OK result.
        reason: String,
    },
    /// A source which exists but could not be read.
    Unavailable {
        /// Concrete collection failure, suitable for an operator.
        reason: String,
    },
    /// A fact which this build cannot currently observe.
    Unsupported {
        /// Missing capability or telemetry contract.
        reason: String,
    },
}

impl<T> Evidence<T> {
    /// Construct an available observation.
    pub fn available(observed_at: u64, value: T) -> Self {
        Self::Available { observed_at, value }
    }

    /// Return the observed value, when one exists.
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Available { value, .. } | Self::Degraded { value, .. } => Some(value),
            Self::Unavailable { .. } | Self::Unsupported { .. } => None,
        }
    }

    pub(crate) fn unknown_reason(&self) -> Option<&str> {
        match self {
            Self::Available { .. } => None,
            Self::Degraded { reason, .. }
            | Self::Unavailable { reason }
            | Self::Unsupported { reason } => Some(reason),
        }
    }
}

/// Membership and reachability evidence for one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeObservation {
    /// Stable cluster node identifier.
    pub node_id: String,
    /// Membership state reported by Mustard.
    pub membership_state: String,
    /// Whether that node's authenticated Bun API answered collection.
    pub agent_reachable: bool,
}

/// Current council composition and liveness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CouncilObservation {
    /// Whether this deployment uses a Raft council.
    pub enabled: bool,
    /// Configured council member count.
    pub member_count: usize,
    /// Council members whose Bun API answered collection.
    pub reachable_members: usize,
    /// Current leader node identifier, when one is elected.
    pub leader: Option<String>,
}

/// One timestamped workload restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartObservation {
    /// Application name.
    pub app: String,
    /// Application namespace.
    pub namespace: String,
    /// Instance which restarted, when the event identified it.
    pub instance: Option<String>,
    /// Unix timestamp, in seconds, of the restart.
    pub timestamp: u64,
    /// Human-readable restart reason.
    pub reason: String,
}

/// One current or recent deploy operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployObservation {
    /// Stable operation identifier.
    pub operation_id: String,
    /// Application name.
    pub app: String,
    /// Application namespace.
    pub namespace: String,
    /// Version or image being deployed, when the operation exposes it.
    pub version: Option<String>,
    /// Unix timestamp, in seconds, when the operation started.
    pub started_at: u64,
    /// Current operation phase.
    pub phase: String,
    /// Whether the operation is still active.
    pub active: bool,
}

/// Current service availability evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceObservation {
    /// Service application name.
    pub app: String,
    /// Service namespace.
    pub namespace: String,
    /// Desired replica count from deployed configuration.
    pub desired_replicas: u32,
    /// Backends currently marked healthy.
    pub healthy_backends: usize,
    /// All backends currently present.
    pub total_backends: usize,
}

/// One active Smoker fault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultObservation {
    /// Fault identifier.
    pub id: u64,
    /// Fault type as reported by Smoker.
    pub fault_type: String,
    /// Target resource.
    pub target: String,
    /// Principal which injected the fault.
    pub injected_by: String,
    /// Remaining lifetime, in seconds.
    pub remaining_seconds: u64,
}

/// One firing alert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertObservation {
    /// Application associated with the alert, when available.
    pub app: Option<String>,
    /// Namespace associated with the alert, when available.
    pub namespace: Option<String>,
    /// Alert text.
    pub message: String,
}

/// Current filesystem usage for one node and storage domain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskObservation {
    /// Node identifier.
    pub node_id: String,
    /// Configured storage domains sharing this filesystem.
    pub storage_domains: Vec<String>,
    /// Bytes currently consumed on the filesystem.
    pub used_bytes: u64,
    /// Total usable filesystem capacity in bytes.
    pub total_bytes: u64,
    /// Percentage of usable capacity currently consumed.
    pub used_percent: f64,
}

/// Observed cgroup CPU throttling for one application.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuThrottleObservation {
    /// Application name.
    pub app: String,
    /// Application namespace.
    pub namespace: String,
    /// Increase in throttled CPU seconds over the observation window.
    pub throttled_seconds_delta: f64,
    /// Observation window in seconds.
    pub window_seconds: u64,
}

/// Public certificate lifecycle metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateObservation {
    /// `node` or `workload`.
    pub certificate_kind: String,
    /// Certificate subject, SPIFFE ID or node identity.
    pub identity: String,
    /// X.509 issuer distinguished name.
    pub issuer: String,
    /// X.509 serial number in hexadecimal.
    pub serial: String,
    /// Unix timestamp, in seconds, when the certificate expires.
    pub not_after: u64,
    /// Current automatic rotation state.
    pub rotation_state: String,
    /// Whether Bun can rotate this leaf without restarting its consumer.
    pub automatic_rotation: bool,
}

/// Pickle listener and redundancy evidence for one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryObservation {
    /// Node identifier which supplied the evidence.
    pub node_id: String,
    /// Whether peer reachability and redundancy apply to this node.
    pub clustered: bool,
    /// Whether the registry listener entered its serving loop.
    pub ready: bool,
    /// Whether peers can reach the selected listener.
    pub peer_reachable: bool,
    /// Whether membership can satisfy the configured redundancy target.
    pub redundancy_possible: bool,
    /// Layers below their configured redundancy target.
    pub under_replicated_layers: usize,
}

/// A recent application log line used only for correlation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogObservation {
    /// Application name.
    pub app: String,
    /// Application namespace.
    pub namespace: String,
    /// Unix timestamp, in seconds, of the log entry.
    pub timestamp: u64,
    /// Whether the source classified this as an error.
    pub is_error: bool,
    /// Log text.
    pub line: String,
}

/// Evidence used by cluster-wide diagnostic checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterEvidence {
    /// Membership plus authenticated per-node reachability.
    pub nodes: Evidence<Vec<NodeObservation>>,
    /// Council composition and leader state.
    pub council: Evidence<CouncilObservation>,
    /// Active Smoker faults.
    pub faults: Evidence<Vec<FaultObservation>>,
    /// Per-node disk capacity evidence.
    pub disks: Evidence<Vec<DiskObservation>>,
    /// Safe public certificate metadata.
    pub certificates: Evidence<Vec<CertificateObservation>>,
    /// Per-node Pickle readiness and redundancy evidence.
    pub registry: Evidence<Vec<RegistryObservation>>,
}

/// Evidence used by application diagnostic checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplicationEvidence {
    /// Timestamped restart events.
    pub restarts: Evidence<Vec<RestartObservation>>,
    /// Active and recent deploy operations.
    pub deploys: Evidence<Vec<DeployObservation>>,
    /// Desired replicas and observed service backends.
    pub services: Evidence<Vec<ServiceObservation>>,
    /// Currently firing alerts.
    pub alerts: Evidence<Vec<AlertObservation>>,
    /// Actual cgroup throttled-time deltas.
    pub cpu_throttling: Evidence<Vec<CpuThrottleObservation>>,
    /// Recent logs for unhealthy applications.
    pub recent_logs: Evidence<Vec<LogObservation>>,
}

/// Complete timestamped input to the pure diagnosis engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WtfInputs {
    /// Operator-visible cluster name.
    pub cluster_name: String,
    /// Unix timestamp, in seconds, used for all diagnostic windows.
    pub collected_at: u64,
    /// Optional application scope. Cluster-wide checks are omitted when set.
    pub app: Option<String>,
    /// Cluster-wide evidence.
    pub cluster: ClusterEvidence,
    /// Application evidence.
    pub applications: ApplicationEvidence,
}

/// A related event which helps explain a finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrelatedEvent {
    /// Unix timestamp, in seconds.
    pub timestamp: u64,
    /// Stable event kind, for example `deploy`, `restart` or `log`.
    pub kind: String,
    /// Human-readable event summary.
    pub message: String,
}

/// One critical or warning diagnosis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WtfFinding {
    /// Stable pattern identifier.
    pub id: String,
    /// Short operator-facing title.
    pub title: String,
    /// Supporting evidence.
    pub details: Vec<String>,
    /// Concrete next action.
    pub suggestion: String,
    /// Events correlated with the finding.
    pub correlated_events: Vec<CorrelatedEvent>,
    /// Resource affected by the finding.
    pub affected_resource: String,
}

/// A diagnostic check whose evidence could not be established.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WtfUnknown {
    /// Stable evidence source identifier.
    pub source: String,
    /// Why no verdict was possible.
    pub reason: String,
    /// Scope affected by the missing evidence.
    pub affected_resource: String,
}

/// A check which completed and found no problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WtfOk {
    /// Stable check identifier.
    pub id: String,
    /// Human-readable evidence-backed result.
    pub description: String,
}

/// Counts for each diagnostic outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WtfSummary {
    /// Critical finding count.
    pub critical_count: usize,
    /// Warning finding count.
    pub warning_count: usize,
    /// Unknown evidence count.
    pub unknown_count: usize,
    /// Successful check count.
    pub ok_count: usize,
}

/// Versioned output from `relish wtf`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WtfReport {
    /// Serialisation contract version.
    pub schema_version: u32,
    /// Cluster name supplied by the collector.
    pub cluster_name: String,
    /// Unix timestamp, in seconds, when evidence collection completed.
    pub collected_at: u64,
    /// Number of nodes represented by membership evidence.
    pub node_count: usize,
    /// Findings requiring immediate action.
    pub critical: Vec<WtfFinding>,
    /// Findings which need attention but do not prove an outage.
    pub warnings: Vec<WtfFinding>,
    /// Checks which could not reach an honest verdict.
    pub unknown: Vec<WtfUnknown>,
    /// Checks which completed successfully.
    pub ok: Vec<WtfOk>,
    /// Counts matching the four lists above.
    pub summary: WtfSummary,
}

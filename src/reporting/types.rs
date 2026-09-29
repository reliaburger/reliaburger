/// State report types for the hierarchical reporting tree.
///
/// A `StateReport` is sent by each worker node to its assigned council
/// member at the configured reporting interval. It contains all runtime
/// state: running apps, resource usage, and recent events.
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::app::AppSpec;
use crate::mayo::rollup::NodeRollup;
use crate::meat::NodeId;
use crate::meat::cluster_state::NodeCapabilities;

/// Sent by each worker node to its assigned council member at the
/// reporting interval. Also sent as a full report during the leader
/// learning period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateReport {
    /// Identity of the reporting node.
    pub node_id: NodeId,
    /// Wall-clock timestamp for staleness detection.
    pub timestamp: SystemTime,
    /// All apps currently running on this node.
    pub running_apps: Vec<RunningApp>,
    /// Cached desired-state specs this node was last assigned.
    pub cached_specs: Vec<CachedSpec>,
    /// Current resource usage.
    pub resource_usage: ResourceUsage,
    /// Recent event log (bounded to last N events).
    pub event_log: Vec<NodeEvent>,
    /// Whether this node can run image builds (`buildah` on PATH,
    /// probed once at worker startup). Build submissions are routed to
    /// a capable node (Phase 12 F2).
    pub has_buildah: bool,
}

/// A single running app instance on a worker node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningApp {
    /// Original execution identity, when durable runtime evidence is available.
    #[serde(default)]
    pub execution: Option<crate::grill::RuntimeExecution>,
    /// Name of the app.
    pub app_name: String,
    /// Namespace the app belongs to.
    pub namespace: String,
    /// Instance index (e.g. 0 for "web-0").
    pub instance_id: u32,
    /// OCI image reference.
    pub image: String,
    /// Host port, if allocated.
    pub port: Option<u16>,
    /// Current health status.
    pub health_status: ReportHealthStatus,
    /// Time since the instance started.
    pub uptime: Duration,
    /// Per-instance resource usage.
    pub resource_usage: AppResourceUsage,
}

/// Typed capability and enforcement evidence sent beside the state report.
/// Explicit protocol compatibility governs admission of every message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapabilityReport {
    /// Node that observed this evidence.
    pub node_id: NodeId,
    /// Live node capabilities used for placement, including DNS readiness.
    pub capabilities: NodeCapabilities,
    /// Per-workload egress enforcement evidence.
    pub egress_enforcement: Vec<EgressEnforcementEvidence>,
    /// A live enforcement incident has fenced at least one workload and has
    /// not yet recovered. The scheduler treats the node as unready meanwhile.
    pub egress_degraded: bool,
    /// Workloads fenced by the current incident. This remains present after
    /// they stop so operators don't lose the evidence before the next report.
    pub egress_affected_workloads: Vec<EgressAffectedWorkload>,
}

/// Critical-subsystem readiness lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeReadinessReport {
    /// Node that observed the subsystem states.
    pub node_id: NodeId,
    /// Point-in-time critical-subsystem decision and supporting states.
    pub evidence: crate::bun::readiness::NodeReadinessEvidence,
}

/// A workload fenced after its live egress security boundary disappeared.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EgressAffectedWorkload {
    /// App name from the deployed specification.
    pub app_name: String,
    /// Namespace containing the app.
    pub namespace: String,
}

/// Identifies a workload whose requested egress policy was evaluated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressEnforcementEvidence {
    /// App name from the deployed specification.
    pub app_name: String,
    /// Namespace containing the app.
    pub namespace: String,
    /// Replica ordinal within the app.
    pub instance_id: u32,
    /// Kernel-truth enforcement state observed for the instance.
    pub status: EgressEnforcementStatus,
}

/// Per-instance egress enforcement evidence reported by the node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EgressEnforcementStatus {
    /// The workload did not request an egress allowlist.
    #[default]
    NotRequested,
    /// All four hooks are live and the instance cgroup has its enforcement flag.
    Enforced,
    /// An allowlist was requested but the live kernel state cannot prove it.
    Unenforced,
}

/// Health status as reported in a StateReport.
///
/// Named `ReportHealthStatus` to avoid collision with
/// `bun::health::HealthStatus` which represents probe results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportHealthStatus {
    /// Instance is healthy (passing health checks).
    Healthy,
    /// Instance is unhealthy after consecutive failures.
    Unhealthy { consecutive_failures: u32 },
    /// Instance is starting up (health checks not yet passed).
    Starting,
    /// Health status is not known.
    Unknown,
}

/// Cached desired-state spec on a worker node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSpec {
    /// Name of the app.
    pub app_name: String,
    /// SHA-256 hash of the serialised spec.
    pub spec_hash: [u8; 32],
    /// The full app spec, included for state reconstruction.
    pub spec: AppSpec,
}

/// Aggregate resource usage on a node.
///
/// "Used" values are the sums of the *requested* resources of running
/// instances (commitments, not measured consumption — measurement is
/// Mayo's job). Totals are schedulable capacity: system totals minus
/// the `[resources]` reservation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceUsage {
    pub cpu_used_millicores: u32,
    pub memory_used_mb: u32,
    pub disk_used_mb: u64,
    pub gpu_used: u8,
    pub allocated_ports: Vec<u16>,
    /// Schedulable CPU capacity.
    pub cpu_total_millicores: u32,
    /// Schedulable memory capacity.
    pub memory_total_mb: u32,
}

/// Per-instance resource usage.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct AppResourceUsage {
    pub cpu_millicores: u32,
    pub memory_mb: u32,
}

/// A notable event on a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeEvent {
    /// When the event occurred.
    pub timestamp: SystemTime,
    /// Category of event.
    pub kind: EventKind,
    /// Human-readable detail.
    pub detail: String,
}

/// Categories of node events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    ContainerStart,
    ContainerStop,
    ContainerCrash,
    HealthCheckFail,
    HealthCheckRecover,
    ImagePull,
    SpecUpdate,
    Restart,
}

/// Messages sent over the reporting tree transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReportingMessage {
    /// A full state report from a worker to its council parent.
    Report(StateReport),
    /// Acknowledgement from the council member.
    Ack { node_id: NodeId },
    /// Aggregated reports forwarded from council member to leader.
    AggregatedReport {
        reports: HashMap<NodeId, StateReport>,
    },
    /// Pre-aggregated metrics rollup from worker to council aggregator.
    MetricsRollup(NodeRollup),
    /// Live scheduling capabilities and per-workload security evidence.
    CapabilityReport(NodeCapabilityReport),
    /// Live critical-subsystem readiness, sent only once the agent has
    /// evidence. Absence or lease expiry is not ready.
    NodeReadinessReport(NodeReadinessReport),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report(name: &str) -> StateReport {
        StateReport {
            has_buildah: false,
            node_id: NodeId::new(name),
            timestamp: SystemTime::now(),
            running_apps: vec![RunningApp {
                execution: None,
                app_name: "web".to_string(),
                namespace: "default".to_string(),
                instance_id: 0,
                image: "nginx:latest".to_string(),
                port: Some(8080),
                health_status: ReportHealthStatus::Healthy,
                uptime: Duration::from_secs(60),
                resource_usage: AppResourceUsage {
                    cpu_millicores: 100,
                    memory_mb: 64,
                },
            }],
            cached_specs: vec![],
            resource_usage: ResourceUsage {
                cpu_used_millicores: 500,
                memory_used_mb: 1024,
                disk_used_mb: 5000,
                gpu_used: 0,
                allocated_ports: vec![8080],
                cpu_total_millicores: 4000,
                memory_total_mb: 8192,
            },
            event_log: vec![NodeEvent {
                timestamp: SystemTime::now(),
                kind: EventKind::ContainerStart,
                detail: "started web-0".to_string(),
            }],
        }
    }

    #[test]
    fn state_report_bincode_round_trip() {
        let report = sample_report("node-1");
        let encoded = bincode::serialize(&report).unwrap();
        let decoded: StateReport = bincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded.node_id, NodeId::new("node-1"));
        assert_eq!(decoded.running_apps.len(), 1);
        assert_eq!(decoded.running_apps[0].app_name, "web");
        assert_eq!(decoded.running_apps[0].port, Some(8080));
        assert_eq!(
            decoded.running_apps[0].health_status,
            ReportHealthStatus::Healthy
        );
        assert_eq!(decoded.resource_usage.cpu_used_millicores, 500);
        assert_eq!(decoded.event_log.len(), 1);
        assert_eq!(decoded.event_log[0].kind, EventKind::ContainerStart);
    }

    #[test]
    fn capability_report_message_round_trip() {
        let capability = NodeCapabilityReport {
            node_id: NodeId::new("node-1"),
            capabilities: NodeCapabilities {
                egress: crate::sesame::egress::EgressEnforcementCapability {
                    connect_ipv4: true,
                    connect_ipv6: true,
                    udp_ipv4: true,
                    udp_ipv6: true,
                    pre_start: true,
                },
                dns: crate::onion::dns::DnsCapability {
                    enabled: true,
                    ready: true,
                    ipv4: true,
                    ipv6: false,
                    workload_reachable: true,
                },
            },
            egress_enforcement: vec![EgressEnforcementEvidence {
                app_name: "web".to_string(),
                namespace: "default".to_string(),
                instance_id: 0,
                status: EgressEnforcementStatus::Enforced,
            }],
            egress_degraded: false,
            egress_affected_workloads: Vec::new(),
        };
        let encoded = bincode::serialize(&ReportingMessage::CapabilityReport(capability)).unwrap();
        let decoded: ReportingMessage = bincode::deserialize(&encoded).unwrap();
        let ReportingMessage::CapabilityReport(decoded) = decoded else {
            panic!("expected capability report");
        };
        assert!(decoded.capabilities.egress.can_enforce_allowlist());
        assert!(decoded.capabilities.dns.can_resolve_internal());
        assert_eq!(
            decoded.egress_enforcement[0].status,
            EgressEnforcementStatus::Enforced
        );
    }

    #[test]
    fn readiness_report_message_round_trip() {
        let message = ReportingMessage::NodeReadinessReport(NodeReadinessReport {
            node_id: NodeId::new("node-1"),
            evidence: crate::bun::readiness::NodeReadinessEvidence {
                ready: true,
                observed_at_unix_ms: 42,
                subsystems: vec![crate::bun::readiness::SubsystemEvidence {
                    name: "agent".to_string(),
                    critical: true,
                    state: crate::bun::readiness::SubsystemState::Ready,
                    state_since_unix_ms: 41,
                    last_error: None,
                    last_error_unix_ms: None,
                    restart_count: 0,
                }],
            },
        });
        let encoded = bincode::serialize(&message).unwrap();
        let decoded: ReportingMessage = bincode::deserialize(&encoded).unwrap();
        let ReportingMessage::NodeReadinessReport(decoded) = decoded else {
            panic!("expected node readiness report");
        };
        assert!(decoded.evidence.ready);
    }

    #[test]
    fn state_report_max_events_under_1mib() {
        let mut report = sample_report("node-1");
        // Fill with 100 events (the configured max)
        report.event_log = (0..100)
            .map(|i| NodeEvent {
                timestamp: SystemTime::now(),
                kind: EventKind::ContainerStart,
                detail: format!("event {i} with some detail text"),
            })
            .collect();
        let encoded = bincode::serialize(&report).unwrap();
        assert!(
            encoded.len() < 1_048_576,
            "report is {} bytes, exceeds 1 MiB",
            encoded.len()
        );
    }

    #[test]
    fn health_status_variants_serialize_distinctly() {
        let healthy = bincode::serialize(&ReportHealthStatus::Healthy).unwrap();
        let unhealthy = bincode::serialize(&ReportHealthStatus::Unhealthy {
            consecutive_failures: 3,
        })
        .unwrap();
        let starting = bincode::serialize(&ReportHealthStatus::Starting).unwrap();
        let unknown = bincode::serialize(&ReportHealthStatus::Unknown).unwrap();
        // All four variants must produce different bytes
        assert_ne!(healthy, unhealthy);
        assert_ne!(healthy, starting);
        assert_ne!(healthy, unknown);
        assert_ne!(unhealthy, starting);
        assert_ne!(unhealthy, unknown);
        assert_ne!(starting, unknown);
    }

    #[test]
    fn reporting_message_round_trip() {
        let msg = ReportingMessage::Report(sample_report("w1"));
        let encoded = bincode::serialize(&msg).unwrap();
        let decoded: ReportingMessage = bincode::deserialize(&encoded).unwrap();
        match decoded {
            ReportingMessage::Report(r) => assert_eq!(r.node_id, NodeId::new("w1")),
            _ => panic!("expected Report variant"),
        }
    }

    #[test]
    fn metrics_rollup_message_round_trip() {
        use crate::mayo::rollup::{NodeRollup, RollupAggregate, RollupEntry};
        use std::collections::BTreeMap;

        let rollup = NodeRollup {
            node_id: NodeId::new("w1"),
            timestamp: 1000,
            entries: vec![RollupEntry {
                metric_name: "cpu_usage".to_string(),
                labels: BTreeMap::new(),
                aggregate: RollupAggregate {
                    min: 10.0,
                    max: 90.0,
                    sum: 300.0,
                    count: 6,
                },
            }],
        };
        let msg = ReportingMessage::MetricsRollup(rollup);
        let encoded = bincode::serialize(&msg).unwrap();
        let decoded: ReportingMessage = bincode::deserialize(&encoded).unwrap();
        match decoded {
            ReportingMessage::MetricsRollup(r) => {
                assert_eq!(r.node_id, NodeId::new("w1"));
                assert_eq!(r.entries.len(), 1);
                assert_eq!(r.entries[0].aggregate.count, 6);
            }
            _ => panic!("expected MetricsRollup variant"),
        }
    }
}

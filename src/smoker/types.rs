/// Smoker fault injection types.
///
/// Core data structures for the fault injection engine. These types
/// are shared between the agent (which executes faults) and the CLI
/// (which parses and submits them).
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// FaultId
// ---------------------------------------------------------------------------

/// Unique identifier for an active fault.
///
/// Monotonically increasing per node. Two faults on different nodes
/// may share the same numeric ID — that's fine, because faults are
/// always scoped to a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FaultId(pub u64);

impl fmt::Display for FaultId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fault-{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// FaultType
// ---------------------------------------------------------------------------

/// The type of fault being injected.
///
/// Network faults (Delay, Drop, DnsNxdomain, Partition, Bandwidth)
/// require eBPF on Linux. Resource faults (CpuStress, MemoryPressure,
/// DiskIoThrottle) require cgroups on Linux. Process faults (Kill,
/// Pause, Resume) and node faults (NodeDrain, NodeKill) work on all
/// platforms.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum FaultType {
    /// Add latency to connections to the target service.
    Delay {
        /// Delay in nanoseconds.
        delay_ns: u64,
        /// Jitter range in nanoseconds (+/- random).
        jitter_ns: u64,
    },

    /// Fail a percentage of connections with ECONNREFUSED.
    Drop {
        /// Percentage of connections to drop (0-100).
        probability: u8,
    },

    /// Return NXDOMAIN for DNS resolution of the target service.
    DnsNxdomain,

    /// Block traffic from a specific source service to the target.
    Partition {
        /// Source app name (the caller that gets blocked).
        source_app: Option<String>,
        /// Source cgroup ID (resolved at activation time, 0 = all callers).
        #[serde(default)]
        source_cgroup_id: u64,
    },

    /// Throttle bandwidth to the target service.
    Bandwidth {
        /// Maximum throughput in bytes per second.
        bytes_per_sec: u64,
    },

    /// Consume a percentage of the target's CPU quota.
    CpuStress {
        /// How much of the cgroup CPU to consume (0-100).
        percentage: u8,
        /// Optionally limit stress to this many cores.
        cores: Option<u32>,
    },

    /// Push memory usage toward the target's memory limit.
    MemoryPressure {
        /// How full to push memory (0-100).
        percentage: u8,
        /// If true, trigger an immediate OOM kill instead.
        #[serde(default)]
        oom: bool,
    },

    /// Throttle disk I/O via blkio cgroup.
    DiskIoThrottle {
        /// Read+write bandwidth limit in bytes per second.
        bytes_per_sec: u64,
        /// If true, only throttle writes.
        #[serde(default)]
        write_only: bool,
    },

    /// Send SIGKILL to target instances.
    Kill {
        /// How many instances to kill (0 = all matching).
        #[serde(default)]
        count: u32,
    },

    /// Send SIGSTOP to freeze target instances.
    Pause,

    /// Send SIGCONT to unfreeze previously paused instances.
    Resume,

    /// Simulate graceful node departure.
    NodeDrain,

    /// Simulate abrupt node failure.
    NodeKill {
        /// If true, also stop all containers on the node.
        #[serde(default)]
        kill_containers: bool,
    },
}

impl fmt::Display for FaultType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Delay {
                delay_ns,
                jitter_ns,
            } => {
                let delay_ms = *delay_ns / 1_000_000;
                if *jitter_ns > 0 {
                    let jitter_ms = *jitter_ns / 1_000_000;
                    write!(f, "delay {delay_ms}ms +/-{jitter_ms}ms")
                } else {
                    write!(f, "delay {delay_ms}ms")
                }
            }
            Self::Drop { probability } => write!(f, "drop {probability}%"),
            Self::DnsNxdomain => write!(f, "dns nxdomain"),
            Self::Partition { source_app, .. } => {
                if let Some(src) = source_app {
                    write!(f, "partition from {src}")
                } else {
                    write!(f, "partition (all callers)")
                }
            }
            Self::Bandwidth { bytes_per_sec } => {
                let mbps = *bytes_per_sec / (1024 * 1024);
                write!(f, "bandwidth {mbps}mbps")
            }
            Self::CpuStress { percentage, cores } => {
                if let Some(c) = cores {
                    write!(f, "cpu {percentage}% ({c} cores)")
                } else {
                    write!(f, "cpu {percentage}%")
                }
            }
            Self::MemoryPressure { percentage, oom } => {
                if *oom {
                    write!(f, "memory oom")
                } else {
                    write!(f, "memory {percentage}%")
                }
            }
            Self::DiskIoThrottle {
                bytes_per_sec,
                write_only,
            } => {
                let mbps = *bytes_per_sec / (1024 * 1024);
                if *write_only {
                    write!(f, "disk-io {mbps}mbps (writes only)")
                } else {
                    write!(f, "disk-io {mbps}mbps")
                }
            }
            Self::Kill { count } => {
                if *count == 0 {
                    write!(f, "kill (all)")
                } else {
                    write!(f, "kill {count}")
                }
            }
            Self::Pause => write!(f, "pause"),
            Self::Resume => write!(f, "resume"),
            Self::NodeDrain => write!(f, "node-drain"),
            Self::NodeKill { kill_containers } => {
                if *kill_containers {
                    write!(f, "node-kill (with containers)")
                } else {
                    write!(f, "node-kill")
                }
            }
        }
    }
}

impl FaultType {
    /// Whether this fault type requires eBPF (Linux only with ebpf feature).
    pub fn requires_ebpf(&self) -> bool {
        matches!(
            self,
            Self::Delay { .. }
                | Self::Drop { .. }
                | Self::DnsNxdomain
                | Self::Partition { .. }
                | Self::Bandwidth { .. }
        )
    }

    /// Whether this fault type requires Linux cgroups.
    pub fn requires_cgroups(&self) -> bool {
        matches!(
            self,
            Self::CpuStress { .. } | Self::MemoryPressure { .. } | Self::DiskIoThrottle { .. }
        )
    }
}

// ---------------------------------------------------------------------------
// FaultRule
// ---------------------------------------------------------------------------

/// A single fault rule, as stored in the agent's fault registry.
///
/// Combines the fault type with its target, timing, and provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultRule {
    /// Unique fault identifier (monotonically increasing per node).
    pub id: FaultId,
    /// What kind of fault to inject.
    pub fault_type: FaultType,
    /// Target service name (e.g. "redis", "api", "payment-service").
    pub target_service: String,
    /// Optional: target a specific instance by name (e.g. "redis-1").
    pub target_instance: Option<String>,
    /// Optional: restrict fault to a specific node.
    pub target_node: Option<String>,
    /// When this fault was activated (monotonic clock, nanoseconds).
    pub activated_at_ns: u64,
    /// When this fault expires (monotonic clock, nanoseconds).
    pub expires_at_ns: u64,
    /// Duration in nanoseconds (for display and audit).
    pub duration_ns: u64,
    /// Who injected this fault.
    pub injected_by: String,
    /// Human-readable reason (from --reason flag).
    pub reason: Option<String>,
}

impl FaultRule {
    /// Create a new fault rule with the given parameters.
    pub fn new(
        id: FaultId,
        fault_type: FaultType,
        target_service: String,
        duration: Duration,
        injected_by: String,
    ) -> Self {
        let now_ns = monotonic_now_ns();
        let duration_ns = duration.as_nanos() as u64;
        Self {
            id,
            fault_type,
            target_service,
            target_instance: None,
            target_node: None,
            activated_at_ns: now_ns,
            expires_at_ns: now_ns + duration_ns,
            duration_ns,
            injected_by,
            reason: None,
        }
    }

    /// How long until this fault expires (zero if already expired).
    pub fn remaining(&self) -> Duration {
        let now = monotonic_now_ns();
        if now >= self.expires_at_ns {
            Duration::ZERO
        } else {
            Duration::from_nanos(self.expires_at_ns - now)
        }
    }

    /// Whether this fault has expired.
    pub fn is_expired(&self) -> bool {
        monotonic_now_ns() >= self.expires_at_ns
    }
}

/// Current monotonic clock in nanoseconds.
///
/// Uses `Instant` difference from a fixed epoch. This is not wall-clock
/// time — it's only meaningful relative to other values from the same
/// process.
pub fn monotonic_now_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;

    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_nanos() as u64
}

// ---------------------------------------------------------------------------
// FaultRequest
// ---------------------------------------------------------------------------

/// A fault injection request, sent from the CLI/API to the agent.
///
/// The agent assigns an ID and converts this into a `FaultRule`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultRequest {
    /// What kind of fault to inject.
    pub fault_type: FaultType,
    /// Target service name.
    pub target_service: String,
    /// Optional: target a specific instance.
    pub target_instance: Option<String>,
    /// Optional: restrict to a specific node.
    pub target_node: Option<String>,
    /// Fault duration.
    pub duration: Duration,
    /// Who is injecting this fault.
    pub injected_by: String,
    /// Human-readable reason.
    pub reason: Option<String>,
    /// Allow targeting the cluster leader.
    #[serde(default)]
    pub include_leader: bool,
    /// Override the >50% node safety check.
    #[serde(default)]
    pub override_safety: bool,
}

// ---------------------------------------------------------------------------
// Safety types
// ---------------------------------------------------------------------------

/// Result of evaluating safety rails before approving a fault.
#[derive(Debug, Clone)]
pub struct SafetyCheck {
    /// Whether the fault passed all safety checks.
    pub approved: bool,
    /// If not approved, which safety rail was violated.
    pub violation: Option<SafetyViolation>,
    /// Current cluster state relevant to the check.
    pub context: SafetyContext,
}

/// Which safety rail was violated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyViolation {
    /// Fault would break Raft quorum.
    QuorumRisk {
        /// How many council nodes already have faults.
        current_affected: u32,
        /// Maximum allowed: `(council_size - 1) / 2`.
        max_allowed: u32,
    },

    /// Fault would kill all replicas of a service.
    ReplicaMinimum {
        service: String,
        current_replicas: u32,
        /// How many would survive after the fault.
        surviving: u32,
    },

    /// Fault targets the cluster leader without --include-leader.
    LeaderTargeted,

    /// Fault affects more than 50% of nodes without --override-safety.
    NodePercentageExceeded {
        affected_nodes: u32,
        total_nodes: u32,
    },
}

impl fmt::Display for SafetyViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QuorumRisk {
                current_affected,
                max_allowed,
            } => write!(
                f,
                "quorum risk: {current_affected} council nodes already affected, max allowed is {max_allowed}"
            ),
            Self::ReplicaMinimum {
                service,
                current_replicas,
                surviving,
            } => write!(
                f,
                "replica minimum: {service} has {current_replicas} replicas, only {surviving} would survive"
            ),
            Self::LeaderTargeted => {
                write!(f, "fault targets the cluster leader (use --include-leader)")
            }
            Self::NodePercentageExceeded {
                affected_nodes,
                total_nodes,
            } => write!(
                f,
                "affects {affected_nodes}/{total_nodes} nodes (>50%, use --override-safety)"
            ),
        }
    }
}

/// Cluster state used by safety rail evaluation.
#[derive(Debug, Clone)]
pub struct SafetyContext {
    /// Number of council (Raft voter) nodes.
    pub council_size: u32,
    /// How many council nodes currently have active faults.
    pub council_nodes_with_active_faults: u32,
    /// Node ID of the current cluster leader.
    pub leader_node_id: String,
    /// Total number of nodes in the cluster.
    pub total_nodes: u32,
    /// How many nodes currently have active faults.
    pub nodes_with_active_faults: u32,
    /// Number of replicas of the target service.
    pub target_service_replicas: u32,
    /// How many replicas of the target service already have active faults.
    pub target_service_faulted_replicas: u32,
}

// ---------------------------------------------------------------------------
// ScriptedScenario
// ---------------------------------------------------------------------------

/// A scripted multi-step chaos scenario, parsed from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptedScenario {
    /// Human-readable scenario name.
    pub name: String,
    /// Ordered list of fault steps.
    #[serde(rename = "step")]
    pub steps: Vec<ScenarioStep>,
}

/// One step in a scripted scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioStep {
    /// Human-readable description of what this step tests.
    pub description: String,
    /// Fault type string as used in the CLI (e.g. "delay", "drop", "memory").
    pub fault: String,
    /// Target service name.
    pub target: String,
    /// Fault value (e.g. "200ms", "10%", "90%", "oom", "nxdomain").
    pub value: String,
    /// Optional jitter (e.g. "50ms").
    pub jitter: Option<String>,
    /// How long this fault should remain active.
    pub duration: Option<String>,
    /// Delay before activating this step, relative to scenario start.
    pub start_after: Option<String>,
}

// ---------------------------------------------------------------------------
// Wire types for the fault API
// ---------------------------------------------------------------------------

/// Summary of an active fault, returned by the list/status API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultSummary {
    /// Fault ID.
    pub id: u64,
    /// Human-readable fault type.
    pub fault_type: String,
    /// Target service.
    pub target_service: String,
    /// Target instance, if scoped.
    pub target_instance: Option<String>,
    /// Seconds remaining before auto-expiry.
    pub remaining_secs: u64,
    /// Who injected it.
    pub injected_by: String,
}

impl From<&FaultRule> for FaultSummary {
    fn from(rule: &FaultRule) -> Self {
        Self {
            id: rule.id.0,
            fault_type: rule.fault_type.to_string(),
            target_service: rule.target_service.clone(),
            target_instance: rule.target_instance.clone(),
            remaining_secs: rule.remaining().as_secs(),
            injected_by: rule.injected_by.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fault_id_display() {
        assert_eq!(FaultId(42).to_string(), "fault-42");
    }

    #[test]
    fn fault_type_delay_display() {
        let ft = FaultType::Delay {
            delay_ns: 200_000_000,
            jitter_ns: 0,
        };
        assert_eq!(ft.to_string(), "delay 200ms");
    }

    #[test]
    fn fault_type_delay_with_jitter_display() {
        let ft = FaultType::Delay {
            delay_ns: 200_000_000,
            jitter_ns: 50_000_000,
        };
        assert_eq!(ft.to_string(), "delay 200ms +/-50ms");
    }

    #[test]
    fn fault_type_drop_display() {
        let ft = FaultType::Drop { probability: 10 };
        assert_eq!(ft.to_string(), "drop 10%");
    }

    #[test]
    fn fault_type_dns_nxdomain_display() {
        assert_eq!(FaultType::DnsNxdomain.to_string(), "dns nxdomain");
    }

    #[test]
    fn fault_type_partition_display() {
        let ft = FaultType::Partition {
            source_app: Some("web".into()),
            source_cgroup_id: 0,
        };
        assert_eq!(ft.to_string(), "partition from web");
    }

    #[test]
    fn fault_type_kill_display() {
        let ft = FaultType::Kill { count: 2 };
        assert_eq!(ft.to_string(), "kill 2");
        let ft_all = FaultType::Kill { count: 0 };
        assert_eq!(ft_all.to_string(), "kill (all)");
    }

    #[test]
    fn fault_type_requires_ebpf() {
        assert!(
            FaultType::Delay {
                delay_ns: 1,
                jitter_ns: 0
            }
            .requires_ebpf()
        );
        assert!(FaultType::Drop { probability: 10 }.requires_ebpf());
        assert!(FaultType::DnsNxdomain.requires_ebpf());
        assert!(!FaultType::Kill { count: 1 }.requires_ebpf());
        assert!(!FaultType::Pause.requires_ebpf());
        assert!(!FaultType::NodeDrain.requires_ebpf());
    }

    #[test]
    fn fault_type_requires_cgroups() {
        assert!(
            FaultType::CpuStress {
                percentage: 50,
                cores: None
            }
            .requires_cgroups()
        );
        assert!(
            FaultType::MemoryPressure {
                percentage: 90,
                oom: false
            }
            .requires_cgroups()
        );
        assert!(
            FaultType::DiskIoThrottle {
                bytes_per_sec: 1024,
                write_only: false
            }
            .requires_cgroups()
        );
        assert!(!FaultType::Kill { count: 1 }.requires_cgroups());
        assert!(
            !FaultType::Delay {
                delay_ns: 1,
                jitter_ns: 0
            }
            .requires_cgroups()
        );
    }

    #[test]
    fn fault_type_serialization_round_trip() {
        let types = vec![
            FaultType::Delay {
                delay_ns: 200_000_000,
                jitter_ns: 50_000_000,
            },
            FaultType::Drop { probability: 10 },
            FaultType::DnsNxdomain,
            FaultType::Partition {
                source_app: Some("web".into()),
                source_cgroup_id: 123,
            },
            FaultType::Bandwidth {
                bytes_per_sec: 1_000_000,
            },
            FaultType::CpuStress {
                percentage: 50,
                cores: Some(2),
            },
            FaultType::MemoryPressure {
                percentage: 90,
                oom: false,
            },
            FaultType::DiskIoThrottle {
                bytes_per_sec: 10_000_000,
                write_only: true,
            },
            FaultType::Kill { count: 3 },
            FaultType::Pause,
            FaultType::Resume,
            FaultType::NodeDrain,
            FaultType::NodeKill {
                kill_containers: true,
            },
        ];
        for ft in &types {
            let json = serde_json::to_string(ft).unwrap();
            let back: FaultType = serde_json::from_str(&json).unwrap();
            assert_eq!(*ft, back, "round-trip failed for: {json}");
        }
    }

    #[test]
    fn fault_rule_new_sets_timing() {
        let rule = FaultRule::new(
            FaultId(1),
            FaultType::Pause,
            "redis".into(),
            Duration::from_secs(30),
            "alice".into(),
        );
        assert_eq!(rule.id, FaultId(1));
        assert_eq!(rule.target_service, "redis");
        assert_eq!(rule.duration_ns, 30_000_000_000);
        assert!(rule.expires_at_ns > rule.activated_at_ns);
        assert_eq!(rule.expires_at_ns - rule.activated_at_ns, 30_000_000_000);
    }

    #[test]
    fn fault_rule_remaining_decreases() {
        let rule = FaultRule::new(
            FaultId(1),
            FaultType::Pause,
            "redis".into(),
            Duration::from_secs(60),
            "alice".into(),
        );
        let remaining = rule.remaining();
        // Should be close to 60s (within 1s tolerance for test execution time)
        assert!(remaining.as_secs() >= 59);
        assert!(!rule.is_expired());
    }

    #[test]
    fn fault_rule_expired_when_past_deadline() {
        let mut rule = FaultRule::new(
            FaultId(1),
            FaultType::Pause,
            "redis".into(),
            Duration::from_secs(0),
            "alice".into(),
        );
        // Force expiry in the past
        rule.expires_at_ns = 0;
        assert!(rule.is_expired());
        assert_eq!(rule.remaining(), Duration::ZERO);
    }

    #[test]
    fn fault_summary_from_rule() {
        let rule = FaultRule::new(
            FaultId(7),
            FaultType::Drop { probability: 25 },
            "api".into(),
            Duration::from_secs(120),
            "bob".into(),
        );
        let summary = FaultSummary::from(&rule);
        assert_eq!(summary.id, 7);
        assert_eq!(summary.fault_type, "drop 25%");
        assert_eq!(summary.target_service, "api");
        assert_eq!(summary.injected_by, "bob");
        assert!(summary.remaining_secs >= 119);
    }

    #[test]
    fn safety_violation_display() {
        let v = SafetyViolation::QuorumRisk {
            current_affected: 2,
            max_allowed: 1,
        };
        assert!(v.to_string().contains("quorum risk"));

        let v = SafetyViolation::ReplicaMinimum {
            service: "web".into(),
            current_replicas: 3,
            surviving: 0,
        };
        assert!(v.to_string().contains("replica minimum"));
        assert!(v.to_string().contains("web"));

        let v = SafetyViolation::LeaderTargeted;
        assert!(v.to_string().contains("leader"));

        let v = SafetyViolation::NodePercentageExceeded {
            affected_nodes: 4,
            total_nodes: 6,
        };
        assert!(v.to_string().contains("4/6"));
    }

    #[test]
    fn scripted_scenario_parses_from_toml() {
        let toml_str = r#"
            name = "Payment cascade failure"

            [[step]]
            description = "Database latency spike"
            fault = "delay"
            target = "pg"
            value = "500ms"
            jitter = "200ms"
            duration = "2m"

            [[step]]
            description = "Database drops connections"
            fault = "drop"
            target = "pg"
            value = "25%"
            start_after = "2m"
            duration = "3m"
        "#;
        let scenario: ScriptedScenario = toml::from_str(toml_str).unwrap();
        assert_eq!(scenario.name, "Payment cascade failure");
        assert_eq!(scenario.steps.len(), 2);
        assert_eq!(scenario.steps[0].fault, "delay");
        assert_eq!(scenario.steps[0].jitter.as_deref(), Some("200ms"));
        assert_eq!(scenario.steps[1].start_after.as_deref(), Some("2m"));
    }

    #[test]
    fn fault_request_serialization_round_trip() {
        let req = FaultRequest {
            fault_type: FaultType::Delay {
                delay_ns: 200_000_000,
                jitter_ns: 0,
            },
            target_service: "redis".into(),
            target_instance: Some("redis-1".into()),
            target_node: None,
            duration: Duration::from_secs(300),
            injected_by: "alice".into(),
            reason: Some("testing latency tolerance".into()),
            include_leader: false,
            override_safety: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: FaultRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.target_service, "redis");
        assert_eq!(back.target_instance.as_deref(), Some("redis-1"));
        assert_eq!(back.duration, Duration::from_secs(300));
    }
}

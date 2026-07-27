//! What this node actually has wired up.
//!
//! Diagnostics and the built-in test runner need to tell three cases apart:
//! a subsystem that is working, one that is switched off, and one that is
//! broken. Guessing from error responses conflates all three — a 404 from
//! `/v1/metrics` looks the same whether Mayo is disabled or crashed.
//!
//! So a node reports its own wiring. `relish test` skips a case whose
//! capability is absent (and says so, rather than passing a hollow test),
//! `relish wtf` reports an absent source as a warning rather than a crash,
//! and `relish bench` marks a suite skipped instead of measuring nothing.
//!
//! Every field is *derived*. Nothing here is hardcoded `true`, because a
//! capability report that lies is worse than no capability report: it turns
//! "this cluster can't do that" into "this test mysteriously fails".
//!
//! Naming note: `ClusterCapabilities`, not `Capabilities`. The codebase
//! already has [`crate::meat::cluster_state::NodeCapabilities`] (a
//! scheduler's view of a node's resources) and
//! [`crate::bun::supervisor::PlatformCapabilities`] (what the host OS
//! supports). Three different questions, three distinct names.

use serde::{Deserialize, Serialize};

/// Capability facts only the node's startup path knows.
///
/// These come from config and from what `bun` actually managed to wire up,
/// so they can't be recovered from `ApiState` later. Built once in
/// `src/bin/bun.rs` and handed to the router.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaticCapabilities {
    /// `[cluster] environment` — e.g. `"production"`. Chaos refuses to run
    /// against a cluster tagged production unless explicitly overridden.
    pub environment: Option<String>,
    /// `"process"`, `"runc"` or `"apple"`.
    pub container_runtime: String,
    /// The Onion eBPF programs loaded and attached.
    pub ebpf: bool,
    /// The Wrapper ingress listener is bound.
    pub ingress: bool,
    /// The nftables perimeter firewall is enabled.
    pub firewall: bool,
    /// Workload identity can actually issue certificates (the wrapping IKM
    /// is present, so CA key operations don't dead-end).
    pub identity: bool,
    /// Process workloads (`exec`/`script`) are permitted on this node.
    pub process_workloads: bool,
    /// cgroup-based faults (CPU, memory, disk I/O) can take effect — Linux
    /// with cgroup v2. Off elsewhere, which is why those faults refuse
    /// rather than pretend on macOS.
    pub cgroup_faults: bool,
}

/// The subsystems whose presence shows up as `Some(..)` on `ApiState`.
///
/// Built in the handler with one `is_some()` per field, so the derivation
/// below stays a pure function and unit-tests without constructing an
/// `ApiState` (which needs a live agent, a channel and a runtime).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WiredSubsystems {
    pub metrics: bool,
    pub logs: bool,
    pub rollups: bool,
    pub council: bool,
    pub registry: bool,
    pub events: bool,
    pub upgrade: bool,
    /// Members this node knows about. `None` when the node runs standalone
    /// (no membership table at all), which is different from a cluster of
    /// one.
    pub member_count: Option<u32>,
}

/// What a node reports about itself at `GET /v1/capabilities`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClusterCapabilities {
    pub version: String,
    pub environment: Option<String>,
    pub container_runtime: String,
    pub cluster: bool,
    pub node_count: u32,
    pub metrics: bool,
    pub logs: bool,
    pub rollups: bool,
    pub council: bool,
    pub registry: bool,
    pub events: bool,
    pub upgrade: bool,
    /// At least one kind of fault can take real effect here.
    pub fault_injection: bool,
    /// cgroup faults (CPU, memory, disk I/O) specifically.
    pub cgroup_faults: bool,
    pub ebpf: bool,
    pub ingress: bool,
    pub firewall: bool,
    pub identity: bool,
    pub process_workloads: bool,
}

impl ClusterCapabilities {
    /// Derive the report. Pure: same inputs, same output, no I/O.
    pub fn derive(statics: &StaticCapabilities, wired: &WiredSubsystems) -> Self {
        // A node is "in a cluster" if it has either half of the cluster
        // plane — a council seat or a membership table. A worker has the
        // second without the first, and it is emphatically clustered.
        let cluster = wired.council || wired.member_count.is_some();
        Self {
            version: crate::upgrade::version::compiled_version().to_string(),
            environment: statics.environment.clone(),
            container_runtime: statics.container_runtime.clone(),
            cluster,
            // Standalone is a cluster of one, not of zero: something is
            // running here.
            node_count: wired.member_count.unwrap_or(1).max(1),
            metrics: wired.metrics,
            logs: wired.logs,
            rollups: wired.rollups,
            council: wired.council,
            registry: wired.registry,
            events: wired.events,
            upgrade: wired.upgrade,
            // Derived, not asserted: the Smoker API is always mounted, so a
            // bare `true` here would tell a caller nothing. What they need
            // to know is whether any fault can actually *do* something —
            // cgroup faults need Linux, network faults need eBPF, and
            // node-level faults need a cluster plane to quiesce.
            fault_injection: statics.cgroup_faults || statics.ebpf || cluster,
            cgroup_faults: statics.cgroup_faults,
            ebpf: statics.ebpf,
            ingress: statics.ingress,
            firewall: statics.firewall,
            identity: statics.identity,
            process_workloads: statics.process_workloads,
        }
    }

    /// Whether this node satisfies a capability a test asks for.
    pub fn has(&self, capability: Capability) -> bool {
        match capability {
            Capability::Cluster => self.cluster,
            Capability::MultiNode => self.node_count >= 3,
            Capability::ProcessRuntime => self.container_runtime == "process",
            Capability::Metrics => self.metrics,
            Capability::Logs => self.logs,
            Capability::Council => self.council,
            Capability::Registry => self.registry,
            Capability::Events => self.events,
            Capability::FaultInjection => self.fault_injection,
            Capability::CgroupFaults => self.cgroup_faults,
            Capability::Ebpf => self.ebpf,
            Capability::Ingress => self.ingress,
            Capability::Firewall => self.firewall,
            Capability::Identity => self.identity,
            Capability::ProcessWorkloads => self.process_workloads,
        }
    }

    /// Which of `wanted` this node lacks. Empty means "run the test".
    pub fn missing(&self, wanted: &[Capability]) -> Vec<Capability> {
        wanted
            .iter()
            .copied()
            .filter(|capability| !self.has(*capability))
            .collect()
    }

    /// Whether this cluster is tagged as production.
    ///
    /// Compared case-insensitively: an operator who writes `"Production"`
    /// means the same thing as one who writes `"production"`, and a chaos
    /// guard that misses the difference is a guard that failed.
    pub fn is_production(&self) -> bool {
        self.environment
            .as_deref()
            .is_some_and(|environment| environment.eq_ignore_ascii_case("production"))
    }
}

/// A capability a test, bench suite or diagnostic requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Cluster,
    /// Three or more nodes — enough for a council and for chaos to have
    /// somewhere to fail over to.
    MultiNode,
    /// The node runs the `process` runtime (ProcessGrill), so the `testapp`
    /// workload can be launched directly from the installed `bun` binary
    /// without a container image. Cases built on `testapp_spec` need this;
    /// they skip on a runc/apple cluster.
    ProcessRuntime,
    Metrics,
    Logs,
    Council,
    Registry,
    Events,
    FaultInjection,
    CgroupFaults,
    Ebpf,
    Ingress,
    Firewall,
    Identity,
    ProcessWorkloads,
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // serde's snake_case is the wire form; reuse it so a skip reason
        // and a JSON report never disagree about what a capability is
        // called.
        let name = serde_json::to_string(self).unwrap_or_default();
        write!(f, "{}", name.trim_matches('"'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statics() -> StaticCapabilities {
        StaticCapabilities {
            container_runtime: "process".to_string(),
            ..StaticCapabilities::default()
        }
    }

    #[test]
    fn wired_subsystems_are_reported_individually() {
        let wired = WiredSubsystems {
            metrics: true,
            logs: true,
            council: false,
            ..WiredSubsystems::default()
        };
        let capabilities = ClusterCapabilities::derive(&statics(), &wired);
        assert!(capabilities.metrics);
        assert!(capabilities.logs);
        assert!(!capabilities.council);
        assert!(!capabilities.registry);
    }

    #[test]
    fn the_environment_tag_is_carried_through() {
        let statics = StaticCapabilities {
            environment: Some("production".to_string()),
            ..statics()
        };
        let capabilities = ClusterCapabilities::derive(&statics, &WiredSubsystems::default());
        assert_eq!(capabilities.environment.as_deref(), Some("production"));
        assert!(capabilities.is_production());
    }

    #[test]
    fn no_environment_tag_is_not_production() {
        let capabilities = ClusterCapabilities::derive(&statics(), &WiredSubsystems::default());
        assert_eq!(capabilities.environment, None);
        assert!(!capabilities.is_production());
    }

    /// A guard that only matches one spelling is a guard that failed.
    #[test]
    fn production_is_matched_case_insensitively() {
        for spelling in ["production", "Production", "PRODUCTION"] {
            let statics = StaticCapabilities {
                environment: Some(spelling.to_string()),
                ..statics()
            };
            let capabilities = ClusterCapabilities::derive(&statics, &WiredSubsystems::default());
            assert!(capabilities.is_production(), "{spelling} must count");
        }
        let statics = StaticCapabilities {
            environment: Some("staging".to_string()),
            ..statics()
        };
        let capabilities = ClusterCapabilities::derive(&statics, &WiredSubsystems::default());
        assert!(!capabilities.is_production());
    }

    /// A worker has a membership table and no council seat, and is very much
    /// part of a cluster.
    #[test]
    fn a_worker_without_a_council_seat_is_still_clustered() {
        let wired = WiredSubsystems {
            council: false,
            member_count: Some(5),
            ..WiredSubsystems::default()
        };
        let capabilities = ClusterCapabilities::derive(&statics(), &wired);
        assert!(capabilities.cluster);
        assert_eq!(capabilities.node_count, 5);
        assert!(capabilities.has(Capability::MultiNode));
    }

    #[test]
    fn a_standalone_node_is_a_cluster_of_one_not_zero() {
        let capabilities = ClusterCapabilities::derive(&statics(), &WiredSubsystems::default());
        assert!(!capabilities.cluster);
        assert_eq!(capabilities.node_count, 1);
        assert!(!capabilities.has(Capability::MultiNode));
    }

    #[test]
    fn multi_node_needs_three() {
        for (members, expected) in [(1, false), (2, false), (3, true), (9, true)] {
            let wired = WiredSubsystems {
                member_count: Some(members),
                ..WiredSubsystems::default()
            };
            let capabilities = ClusterCapabilities::derive(&statics(), &wired);
            assert_eq!(
                capabilities.has(Capability::MultiNode),
                expected,
                "{members} members"
            );
        }
    }

    /// `fault_injection` is derived rather than asserted: the Smoker API is
    /// always mounted, so a bare `true` would tell a caller nothing about
    /// whether a fault can actually do anything.
    #[test]
    fn fault_injection_reflects_what_can_actually_take_effect() {
        let bare = ClusterCapabilities::derive(&statics(), &WiredSubsystems::default());
        assert!(
            !bare.fault_injection,
            "a standalone non-Linux node without eBPF can apply no fault with a real effect"
        );

        let with_cgroups = ClusterCapabilities::derive(
            &StaticCapabilities {
                cgroup_faults: true,
                ..statics()
            },
            &WiredSubsystems::default(),
        );
        assert!(with_cgroups.fault_injection);

        let clustered = ClusterCapabilities::derive(
            &statics(),
            &WiredSubsystems {
                council: true,
                ..WiredSubsystems::default()
            },
        );
        assert!(
            clustered.fault_injection,
            "node-level faults quiesce the cluster plane, so a cluster is enough"
        );
    }

    #[test]
    fn missing_lists_only_absent_capabilities() {
        let wired = WiredSubsystems {
            metrics: true,
            member_count: Some(3),
            ..WiredSubsystems::default()
        };
        let capabilities = ClusterCapabilities::derive(&statics(), &wired);
        let missing = capabilities.missing(&[
            Capability::Metrics,
            Capability::MultiNode,
            Capability::Ebpf,
            Capability::Ingress,
        ]);
        assert_eq!(missing, vec![Capability::Ebpf, Capability::Ingress]);
        assert!(capabilities.missing(&[Capability::Metrics]).is_empty());
    }

    /// Skip reasons and JSON reports must call a capability the same thing.
    #[test]
    fn capability_display_matches_its_wire_form() {
        assert_eq!(
            Capability::ProcessWorkloads.to_string(),
            "process_workloads"
        );
        assert_eq!(Capability::MultiNode.to_string(), "multi_node");
        assert_eq!(Capability::Ebpf.to_string(), "ebpf");
    }

    #[test]
    fn capabilities_round_trip_through_json() {
        let capabilities = ClusterCapabilities::derive(
            &StaticCapabilities {
                environment: Some("staging".to_string()),
                container_runtime: "runc".to_string(),
                ebpf: true,
                cgroup_faults: true,
                ..StaticCapabilities::default()
            },
            &WiredSubsystems {
                metrics: true,
                council: true,
                member_count: Some(3),
                ..WiredSubsystems::default()
            },
        );
        let json = serde_json::to_string(&capabilities).unwrap();
        let decoded: ClusterCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, capabilities);
    }
}

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

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

/// Current wire schema for `GET /v1/capabilities`.
///
/// Version three adds explicit Available/Unavailable/Unknown evidence,
/// reproducibility fingerprints and server policy without removing the
/// version-two summary fields.
pub const CAPABILITY_SCHEMA_VERSION: u32 = 3;
/// Maximum age a Phase 15 caller may trust one live snapshot.
pub const CAPABILITY_TTL_MILLIS: u64 = 15_000;
/// Shared ceiling for an authenticated cluster-wide collection.
pub const CLUSTER_COLLECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_CAPABILITY_RESPONSE_BYTES: usize = 1024 * 1024;

/// Capability facts only the node's startup path knows.
///
/// These come from config and from what `bun` actually managed to wire up,
/// so they can't be recovered from `ApiState` later. Built once in
/// `src/bin/bun.rs` and handed to the router.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaticCapabilities {
    /// Stable gossip/node identity.
    pub node_id: String,
    /// Stable display name and SPIFFE trust domain.
    pub cluster_name: String,
    /// Whether Bun started its cluster runtime.
    pub cluster_mode: bool,
    /// Descriptive `[cluster] environment` metadata. Safety decisions use
    /// `test_policy`, never this free-form string.
    pub environment: Option<String>,
    /// `"process"`, `"runc"` or `"apple"`.
    pub container_runtime: String,
    /// Runtime version reported by the selected implementation.
    pub runtime_version: Option<String>,
    /// Whether the selected runtime operates without host root.
    pub rootless: bool,
    /// Kernel or host OS release.
    pub kernel: String,
    /// Host architecture.
    pub architecture: String,
    /// The Onion eBPF programs loaded and attached.
    pub ebpf: bool,
    /// The Wrapper ingress listener is bound.
    pub ingress: bool,
    /// Wrapper has a usable cluster Ingress CA resolver.
    pub ingress_cluster_tls: bool,
    /// Wrapper loaded an operator certificate/key pair.
    pub ingress_explicit_tls: bool,
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
    /// Registry image policy requires trusted signatures.
    pub registry_signatures_required: bool,
    /// Server-owned diagnostic policy. A client can inspect but not expand it.
    pub test_policy: crate::testkit::safety::ClusterTestPolicy,
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

/// Fresh facts which aren't static wiring and don't reduce to `Some(..)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityObservations {
    /// Critical long-lived task evidence.
    pub readiness: Option<crate::bun::readiness::NodeReadinessEvidence>,
    /// Placement data-path evidence.
    pub placement: Option<crate::bun::readiness::NodeCapabilityEvidence>,
    /// Root-CA-derived cluster identity, when it can be established.
    pub cluster_id: Option<String>,
    /// Fresh council quorum decision. `None` means no conclusive observation.
    pub council_quorum: Option<bool>,
}

/// Whether fresh evidence proves a capability usable or unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Available,
    Unavailable,
    Unknown,
}

/// One time-bounded capability statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityEvidence {
    pub capability: Capability,
    pub state: CapabilityState,
    pub observed_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub details: Vec<String>,
}

/// Binary identity needed to reproduce or compare a result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildFingerprint {
    pub version: String,
    pub git_sha: Option<String>,
    pub target: String,
    pub profile: String,
}

/// Runtime and host identity for the node producing evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeFingerprint {
    pub node_id: String,
    pub name: String,
    pub version: Option<String>,
    pub rootless: bool,
    pub kernel: String,
    pub architecture: String,
}

/// Server policy for one Phase 15 operation class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationPolicyEvidence {
    pub operation: crate::testkit::safety::OperationPermission,
    pub server_allowed: bool,
    pub protected_cluster_allowed: bool,
    pub minimum_role: String,
    pub acknowledgement_required: bool,
}

/// What a node reports about itself at `GET /v1/capabilities`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClusterCapabilities {
    /// Wire schema version.
    pub schema_version: u32,
    /// Stable identity of the node which assembled this report.
    pub node_id: String,
    /// Stable cluster display/trust-domain name.
    pub cluster_name: String,
    /// Root-CA-derived cluster identity, when available.
    pub cluster_id: Option<String>,
    /// Time the live evidence was assembled.
    pub observed_at_unix_ms: u64,
    /// Time after which a caller must treat the evidence as unknown.
    pub expires_at_unix_ms: u64,
    /// Critical long-lived task evidence, when collected by a live node.
    pub readiness: Option<crate::bun::readiness::NodeReadinessEvidence>,
    /// Live placement data-path evidence, when collected by a live node.
    pub placement: Option<crate::bun::readiness::NodeCapabilityEvidence>,
    pub build: BuildFingerprint,
    pub runtime: RuntimeFingerprint,
    pub operations: Vec<OperationPolicyEvidence>,
    /// Stable, explicit evidence used by strict Phase 15 consumers.
    pub capabilities: Vec<CapabilityEvidence>,
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
    /// Permissions and limits the server will enforce for diagnostics.
    pub test_policy: crate::testkit::safety::ClusterTestPolicy,
}

/// One explicit result in a bounded cross-node collection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum CollectedNodeCapability {
    Evidence {
        node_id: String,
        address: String,
        report: Box<ClusterCapabilities>,
    },
    Unknown {
        node_id: String,
        address: String,
        reason: String,
    },
}

/// Authenticated cross-node capability collection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterCapabilityReport {
    pub schema_version: u32,
    pub collected_by: String,
    pub observed_at_unix_ms: u64,
    pub deadline_at_unix_ms: u64,
    pub nodes: Vec<CollectedNodeCapability>,
}

/// Fetch one peer's local report before the shared collection deadline.
///
/// The collector always presents the cluster service token and refuses an
/// unauthenticated fallback.
pub async fn collect_peer_capability(
    cluster_http: &crate::cluster::ClusterHttp,
    service_token: Option<&str>,
    node_id: &str,
    address: std::net::SocketAddr,
    deadline: tokio::time::Instant,
) -> CollectedNodeCapability {
    let unknown = |reason: String| CollectedNodeCapability::Unknown {
        node_id: node_id.to_string(),
        address: address.to_string(),
        reason,
    };
    let Some(service_token) = service_token else {
        return unknown(
            "cluster service token unavailable; unauthenticated fallback refused".to_string(),
        );
    };
    let request = cluster_http
        .client()
        .get(cluster_http.url(&address.to_string(), "/v1/capabilities"))
        .bearer_auth(service_token)
        .send();
    let response = match tokio::time::timeout_at(deadline, request).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return unknown(format!("peer request failed: {error}")),
        Err(_) => return unknown("shared collection deadline expired".to_string()),
    };
    if !response.status().is_success() {
        return unknown(format!("peer returned HTTP {}", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CAPABILITY_RESPONSE_BYTES as u64)
    {
        return unknown("peer response exceeded the 1 MiB limit".to_string());
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let next = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(next) => next,
            Err(_) => return unknown("shared collection deadline expired".to_string()),
        };
        let Some(chunk) = next else { break };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => return unknown(format!("peer response failed: {error}")),
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_CAPABILITY_RESPONSE_BYTES {
            return unknown("peer response exceeded the 1 MiB limit".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }

    let report: ClusterCapabilities = match serde_json::from_slice(&bytes) {
        Ok(report) => report,
        Err(error) => return unknown(format!("peer returned invalid capability JSON: {error}")),
    };
    if report.schema_version != CAPABILITY_SCHEMA_VERSION {
        return unknown(format!(
            "peer schema {} is incompatible with {}",
            report.schema_version, CAPABILITY_SCHEMA_VERSION
        ));
    }
    if report.node_id != node_id {
        return unknown(format!(
            "peer identity mismatch: expected {node_id}, received {}",
            report.node_id
        ));
    }
    if report.expires_at_unix_ms <= unix_millis(std::time::SystemTime::now()) {
        return unknown("peer capability evidence is stale".to_string());
    }

    CollectedNodeCapability::Evidence {
        node_id: node_id.to_string(),
        address: address.to_string(),
        report: Box::new(report),
    }
}

impl ClusterCapabilities {
    /// Derive the report. Pure: same inputs, same output, no I/O.
    pub fn derive(statics: &StaticCapabilities, wired: &WiredSubsystems) -> Self {
        Self::derive_with_observations(statics, wired, CapabilityObservations::default())
    }

    /// Compatibility wrapper for callers which only have readiness evidence.
    pub fn derive_with_evidence(
        statics: &StaticCapabilities,
        wired: &WiredSubsystems,
        readiness: Option<crate::bun::readiness::NodeReadinessEvidence>,
        placement: Option<crate::bun::readiness::NodeCapabilityEvidence>,
    ) -> Self {
        Self::derive_with_observations(
            statics,
            wired,
            CapabilityObservations {
                readiness,
                placement,
                ..CapabilityObservations::default()
            },
        )
    }

    /// Derive a timestamped report without guessing unavailable telemetry.
    pub fn derive_with_observations(
        statics: &StaticCapabilities,
        wired: &WiredSubsystems,
        observations: CapabilityObservations,
    ) -> Self {
        let observed_at_unix_ms = unix_millis(std::time::SystemTime::now());
        // A node is "in a cluster" if it has either half of the cluster
        // plane — a council seat or a membership table. A worker has the
        // second without the first, and it is emphatically clustered.
        let cluster = statics.cluster_mode || wired.council;
        let fault_injection = statics.cgroup_faults || statics.ebpf || cluster;
        let build = BuildFingerprint {
            version: crate::upgrade::version::compiled_version().to_string(),
            git_sha: option_env!("RELIABURGER_GIT_SHA").map(str::to_string),
            target: option_env!("RELIABURGER_TARGET")
                .unwrap_or(std::env::consts::ARCH)
                .to_string(),
            profile: if cfg!(debug_assertions) {
                "debug".to_string()
            } else {
                "release".to_string()
            },
        };
        let runtime = RuntimeFingerprint {
            node_id: statics.node_id.clone(),
            name: statics.container_runtime.clone(),
            version: statics.runtime_version.clone(),
            rootless: statics.rootless,
            kernel: statics.kernel.clone(),
            architecture: statics.architecture.clone(),
        };
        let operations = operation_policy(&statics.test_policy);
        let mut report = Self {
            schema_version: CAPABILITY_SCHEMA_VERSION,
            node_id: statics.node_id.clone(),
            cluster_name: statics.cluster_name.clone(),
            cluster_id: observations.cluster_id,
            observed_at_unix_ms,
            expires_at_unix_ms: observed_at_unix_ms.saturating_add(CAPABILITY_TTL_MILLIS),
            readiness: observations.readiness,
            placement: observations.placement,
            build,
            runtime,
            operations,
            capabilities: Vec::new(),
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
            fault_injection,
            cgroup_faults: statics.cgroup_faults,
            ebpf: statics.ebpf,
            ingress: statics.ingress,
            firewall: statics.firewall,
            identity: statics.identity,
            process_workloads: statics.process_workloads,
            test_policy: statics.test_policy.clone(),
        };
        report.capabilities =
            report.build_capability_evidence(statics, wired, observations.council_quorum);
        report
    }

    fn build_capability_evidence(
        &self,
        statics: &StaticCapabilities,
        wired: &WiredSubsystems,
        council_quorum: Option<bool>,
    ) -> Vec<CapabilityEvidence> {
        let mut evidence = Vec::new();
        let mut add = |capability: Capability, state: CapabilityState, detail: String| {
            evidence.push(CapabilityEvidence {
                capability,
                state,
                observed_at_unix_ms: self.observed_at_unix_ms,
                expires_at_unix_ms: self.expires_at_unix_ms,
                details: vec![detail],
            });
        };

        add(
            Capability::Cluster,
            known(self.cluster),
            if self.cluster {
                "cluster runtime is active".to_string()
            } else {
                "standalone mode".to_string()
            },
        );
        let multi_node = match (self.cluster, wired.member_count) {
            (false, None) => CapabilityState::Unavailable,
            (_, Some(count)) => known(count >= 3),
            (true, None) => CapabilityState::Unknown,
        };
        add(
            Capability::MultiNode,
            multi_node,
            match wired.member_count {
                Some(count) => format!("fresh membership contains {count} node(s)"),
                None => "fresh membership evidence is unavailable".to_string(),
            },
        );

        for (capability, runtime) in [
            (Capability::RuntimeRunc, "runc"),
            (Capability::RuntimeApple, "apple"),
            (Capability::RuntimeProcessGrill, "process"),
        ] {
            let selected = self.container_runtime == runtime;
            let usable = if runtime == "process" {
                selected && statics.process_workloads
            } else {
                selected
            };
            add(
                capability,
                known(usable),
                format!("selected runtime is {}", self.container_runtime),
            );
        }
        add(
            Capability::ProcessRuntime,
            known(self.container_runtime == "process" && statics.process_workloads),
            "ProcessGrill selected with an executable allowlist".to_string(),
        );
        add(
            Capability::ContainerRuntime,
            known(matches!(self.container_runtime.as_str(), "runc" | "apple")),
            format!("selected runtime is {}", self.container_runtime),
        );
        add(
            Capability::Rootful,
            known(!statics.rootless),
            if statics.rootless {
                "runtime is rootless".to_string()
            } else {
                "runtime is rootful".to_string()
            },
        );

        let placement = self.placement.as_ref();
        let egress = placement.map(|snapshot| snapshot.egress);
        add(
            Capability::EgressIpv4,
            egress
                .map(|value| known(value.connect_ipv4 && value.udp_ipv4 && value.pre_start))
                .unwrap_or(CapabilityState::Unknown),
            "connect4/sendmsg4 hooks plus pre-start placement".to_string(),
        );
        add(
            Capability::EgressIpv6,
            egress
                .map(|value| known(value.connect_ipv6 && value.udp_ipv6 && value.pre_start))
                .unwrap_or(CapabilityState::Unknown),
            "connect6/sendmsg6 hooks plus pre-start placement".to_string(),
        );
        let dns = placement.map(|snapshot| snapshot.dns);
        let dns_state = dns
            .map(|value| known(value.can_resolve_internal()))
            .unwrap_or(CapabilityState::Unknown);
        add(
            Capability::DnsUdp,
            dns_state,
            "userspace DNS UDP and workload reachability".to_string(),
        );
        add(
            Capability::DnsTcp,
            dns_state,
            "userspace DNS TCP and workload reachability".to_string(),
        );

        add(
            Capability::Metrics,
            known(wired.metrics),
            "metrics store wiring".to_string(),
        );
        add(
            Capability::MetricsFresh,
            if wired.metrics {
                CapabilityState::Unknown
            } else {
                CapabilityState::Unavailable
            },
            "sample freshness is not published yet".to_string(),
        );
        add(
            Capability::Logs,
            known(wired.logs),
            "log store wiring".to_string(),
        );
        add(
            Capability::LogsFresh,
            if wired.logs {
                CapabilityState::Unknown
            } else {
                CapabilityState::Unavailable
            },
            "log-entry freshness is not published yet".to_string(),
        );
        add(
            Capability::Events,
            known(wired.events),
            "event store wiring".to_string(),
        );
        add(
            Capability::EventsFresh,
            if wired.events {
                CapabilityState::Unknown
            } else {
                CapabilityState::Unavailable
            },
            "cluster event freshness is not published yet".to_string(),
        );
        add(
            Capability::Council,
            known(wired.council),
            "council handle wiring".to_string(),
        );
        add(
            Capability::CouncilQuorum,
            if wired.council {
                optional_state(council_quorum)
            } else {
                CapabilityState::Unavailable
            },
            "fresh Raft membership and leader evidence".to_string(),
        );

        let registry = placement.map(|snapshot| snapshot.registry);
        let registry_available = registry.map(|value| value.ready && value.peer_reachable);
        add(
            Capability::Registry,
            registry_available.map(known).unwrap_or(if wired.registry {
                CapabilityState::Unknown
            } else {
                CapabilityState::Unavailable
            }),
            "registry listener readiness and peer reachability".to_string(),
        );
        add(
            Capability::RegistryRedundant,
            registry
                .map(|value| {
                    known(
                        value.ready
                            && value.peer_reachable
                            && value.redundancy_possible
                            && value.under_replicated_layers == 0,
                    )
                })
                .unwrap_or(CapabilityState::Unknown),
            match registry {
                Some(value) => format!(
                    "target {}, known nodes {}, under-replicated layers {}",
                    value.redundancy_target, value.known_nodes, value.under_replicated_layers
                ),
                None => "registry redundancy evidence is unavailable".to_string(),
            },
        );
        add(
            Capability::RegistrySigned,
            registry_available
                .map(|available| known(available && statics.registry_signatures_required))
                .unwrap_or(CapabilityState::Unknown),
            "registry reachability plus required signature policy".to_string(),
        );

        add(
            Capability::Ebpf,
            known(statics.ebpf),
            "Onion eBPF programs loaded and attached".to_string(),
        );
        add(
            Capability::Ingress,
            known(statics.ingress),
            "Wrapper listener is bound".to_string(),
        );
        add(
            Capability::IngressClusterTls,
            known(statics.ingress && statics.ingress_cluster_tls),
            "Wrapper has a cluster Ingress CA resolver".to_string(),
        );
        add(
            Capability::IngressExplicitTls,
            known(statics.ingress && statics.ingress_explicit_tls),
            "Wrapper loaded an operator certificate/key pair".to_string(),
        );
        add(
            Capability::Firewall,
            known(statics.firewall),
            "nftables perimeter enforcement".to_string(),
        );
        add(
            Capability::Identity,
            known(statics.identity),
            "workload identity signing material".to_string(),
        );
        add(
            Capability::ProcessWorkloads,
            known(statics.process_workloads),
            "host executable allowlist is non-empty".to_string(),
        );
        add(
            Capability::CgroupFaults,
            known(statics.cgroup_faults),
            "Linux cgroup-v2 fault primitives".to_string(),
        );
        add(
            Capability::FaultInjection,
            known(self.fault_injection),
            "at least one fault primitive can affect live state".to_string(),
        );
        add(
            Capability::WorkloadFaults,
            if self.fault_injection {
                CapabilityState::Unknown
            } else {
                CapabilityState::Unavailable
            },
            "per-primitive Smoker evidence is not published yet".to_string(),
        );

        for (capability, detail) in [
            (
                Capability::Secrets,
                "secret delivery freshness is not published yet",
            ),
            (
                Capability::VolumePersistent,
                "persistent volume semantics are not published yet",
            ),
            (
                Capability::VolumeQuota,
                "volume quota enforcement is not published yet",
            ),
        ] {
            add(capability, CapabilityState::Unknown, detail.to_string());
        }
        for capability in [
            Capability::NodeDrain,
            Capability::NodeKill,
            Capability::NodePressure,
        ] {
            add(
                capability,
                CapabilityState::Unavailable,
                "primitive is not implemented".to_string(),
            );
        }

        evidence
    }

    /// Fresh availability of one capability.
    pub fn state(&self, capability: Capability) -> CapabilityState {
        if self.expires_at_unix_ms <= unix_millis(std::time::SystemTime::now()) {
            return CapabilityState::Unknown;
        }
        self.capabilities
            .iter()
            .find(|item| item.capability == capability)
            .map(|item| item.state)
            .unwrap_or(CapabilityState::Unknown)
    }

    /// Whether fresh evidence proves a capability usable.
    pub fn has(&self, capability: Capability) -> bool {
        self.state(capability) == CapabilityState::Available
    }

    /// Which of `wanted` this node lacks. Empty means "run the test".
    pub fn missing(&self, wanted: &[Capability]) -> Vec<Capability> {
        wanted
            .iter()
            .copied()
            .filter(|capability| !self.has(*capability))
            .collect()
    }

    /// Whether the descriptive environment tag says production.
    ///
    /// Retained for compatibility and display. It is not an authorisation
    /// decision; [`ClusterTestPolicy`](crate::testkit::safety::ClusterTestPolicy)
    /// owns that boundary.
    pub fn is_production(&self) -> bool {
        self.environment
            .as_deref()
            .is_some_and(|environment| environment.eq_ignore_ascii_case("production"))
    }
}

fn known(available: bool) -> CapabilityState {
    if available {
        CapabilityState::Available
    } else {
        CapabilityState::Unavailable
    }
}

fn optional_state(available: Option<bool>) -> CapabilityState {
    available.map(known).unwrap_or(CapabilityState::Unknown)
}

fn operation_policy(
    policy: &crate::testkit::safety::ClusterTestPolicy,
) -> Vec<OperationPolicyEvidence> {
    use crate::testkit::safety::OperationPermission;

    [
        OperationPermission::ReadDiagnostics,
        OperationPermission::ProvisionIsolatedWorkloads,
        OperationPermission::InjectWorkloadFaults,
        OperationPermission::AlterNodeState,
        OperationPermission::SaturateCapacity,
        OperationPermission::ProbeExternalDestination,
    ]
    .into_iter()
    .map(|operation| {
        let mutates_or_probes = operation != OperationPermission::ReadDiagnostics;
        OperationPolicyEvidence {
            operation,
            server_allowed: !mutates_or_probes || policy.allowed_operations.contains(&operation),
            protected_cluster_allowed: !mutates_or_probes
                || !policy.safety_class.is_protected()
                || policy.allow_protected_mutation,
            minimum_role: operation.minimum_role().to_string(),
            acknowledgement_required: matches!(
                operation,
                OperationPermission::InjectWorkloadFaults
                    | OperationPermission::AlterNodeState
                    | OperationPermission::SaturateCapacity
            ),
        }
    })
    .collect()
}

fn unix_millis(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// A capability a test, bench suite or diagnostic requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Cluster,
    /// Three or more nodes — enough for a council and for chaos to have
    /// somewhere to fail over to.
    MultiNode,
    RuntimeRunc,
    RuntimeApple,
    RuntimeProcessGrill,
    Rootful,
    /// The node runs the `process` runtime (ProcessGrill), so the `testapp`
    /// workload can be launched directly from the installed `bun` binary
    /// without a container image. Cases built on `testapp_spec` need this;
    /// they skip on a runc/apple cluster.
    ProcessRuntime,
    /// The node runs a *container* runtime (runc/apple), so cases that need a
    /// real container — mount namespaces for volumes, eBPF for firewall
    /// enforcement, an ingress listener behind a proxy — can run. They deploy
    /// a stock public image (`busybox`) and skip on a process cluster.
    ContainerRuntime,
    EgressIpv4,
    EgressIpv6,
    DnsUdp,
    DnsTcp,
    Metrics,
    MetricsFresh,
    Logs,
    LogsFresh,
    Council,
    CouncilQuorum,
    Registry,
    RegistryRedundant,
    RegistrySigned,
    Events,
    EventsFresh,
    FaultInjection,
    WorkloadFaults,
    CgroupFaults,
    Ebpf,
    Ingress,
    IngressClusterTls,
    IngressExplicitTls,
    Firewall,
    Identity,
    Secrets,
    VolumePersistent,
    VolumeQuota,
    ProcessWorkloads,
    NodeDrain,
    NodeKill,
    NodePressure,
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
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::{Json, Router};

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
    fn capability_report_identifies_its_node_and_expires() {
        let statics = StaticCapabilities {
            node_id: "node-a".to_string(),
            ..statics()
        };
        let report = ClusterCapabilities::derive(&statics, &WiredSubsystems::default());
        assert_eq!(report.schema_version, CAPABILITY_SCHEMA_VERSION);
        assert_eq!(report.node_id, "node-a");
        assert_eq!(
            report.expires_at_unix_ms - report.observed_at_unix_ms,
            CAPABILITY_TTL_MILLIS
        );
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
        let capabilities = ClusterCapabilities::derive(
            &StaticCapabilities {
                cluster_mode: true,
                ..statics()
            },
            &wired,
        );
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

    #[test]
    fn capability_evidence_distinguishes_absent_from_unknown() {
        let capabilities = ClusterCapabilities::derive_with_observations(
            &StaticCapabilities {
                cluster_mode: true,
                container_runtime: "runc".to_string(),
                ingress: true,
                ingress_cluster_tls: true,
                ..StaticCapabilities::default()
            },
            &WiredSubsystems {
                metrics: true,
                registry: true,
                member_count: None,
                ..WiredSubsystems::default()
            },
            CapabilityObservations::default(),
        );

        assert_eq!(
            capabilities.state(Capability::RuntimeRunc),
            CapabilityState::Available
        );
        assert_eq!(
            capabilities.state(Capability::RuntimeApple),
            CapabilityState::Unavailable
        );
        assert_eq!(
            capabilities.state(Capability::MetricsFresh),
            CapabilityState::Unknown
        );
        assert_eq!(
            capabilities.state(Capability::MultiNode),
            CapabilityState::Unknown
        );
        assert_eq!(
            capabilities.state(Capability::NodeKill),
            CapabilityState::Unavailable
        );
    }

    #[test]
    fn stale_capability_evidence_is_unknown() {
        let mut capabilities = ClusterCapabilities::derive(&statics(), &WiredSubsystems::default());
        capabilities.expires_at_unix_ms = 0;

        assert_eq!(
            capabilities.state(Capability::ProcessRuntime),
            CapabilityState::Unknown
        );
    }

    #[test]
    fn capability_report_contains_reproduction_and_policy_fingerprints() {
        let capabilities = ClusterCapabilities::derive(
            &StaticCapabilities {
                node_id: "node-a".to_string(),
                cluster_name: "payments.prod".to_string(),
                runtime_version: Some("runc 1.4.0".to_string()),
                kernel: "Linux 6.8.0".to_string(),
                architecture: "aarch64".to_string(),
                rootless: false,
                ..statics()
            },
            &WiredSubsystems::default(),
        );

        assert_eq!(capabilities.cluster_name, "payments.prod");
        assert_eq!(capabilities.runtime.node_id, "node-a");
        assert_eq!(capabilities.runtime.name, "process");
        assert_eq!(capabilities.runtime.version.as_deref(), Some("runc 1.4.0"));
        assert_eq!(capabilities.runtime.kernel, "Linux 6.8.0");
        assert_eq!(capabilities.runtime.architecture, "aarch64");
        assert!(!capabilities.build.target.is_empty());
        assert!(!capabilities.operations.is_empty());
    }

    async fn authenticated_report(
        State(report): State<ClusterCapabilities>,
        headers: HeaderMap,
    ) -> Response {
        if headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            != Some("Bearer cluster-secret")
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Json(report).into_response()
    }

    #[tokio::test]
    async fn peer_collection_uses_service_auth_and_validates_identity() {
        let report = ClusterCapabilities::derive(
            &StaticCapabilities {
                node_id: "node-1".to_string(),
                ..statics()
            },
            &WiredSubsystems::default(),
        );
        let app = Router::new()
            .route("/v1/capabilities", get(authenticated_report))
            .with_state(report);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let collected = collect_peer_capability(
            &crate::cluster::ClusterHttp::plaintext(),
            Some("cluster-secret"),
            "node-1",
            address,
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .await;
        assert!(matches!(
            collected,
            CollectedNodeCapability::Evidence { ref node_id, .. } if node_id == "node-1"
        ));
        server.abort();
    }

    #[tokio::test]
    async fn peer_collection_never_falls_back_without_service_auth() {
        let collected = collect_peer_capability(
            &crate::cluster::ClusterHttp::plaintext(),
            None,
            "node-2",
            "127.0.0.1:9".parse().unwrap(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .await;
        assert!(matches!(
            collected,
            CollectedNodeCapability::Unknown { reason, .. }
                if reason.contains("unauthenticated fallback refused")
        ));
    }
}

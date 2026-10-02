//! What the runtime says about an instance it has just created or started,
//! read by whoever drove the runtime, off the agent loop (#351, stage 3).
//!
//! The loop records a started instance for adoption (its pid, its log files,
//! its rootless port forward) and routes to its address. It used to ask the
//! runtime for each of those itself, inside the turn, under the instance's
//! lifecycle lock. A deploy worker or a restart step has just talked to the
//! runtime anyway and can wait as long as it likes, so it reads them and
//! hands them over with the step that needs them.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;

use super::{AppSpec, Grill, InstanceId, RUNTIME_INVENTORY_TIMEOUT};
use crate::grill::records::{RootlessNetworkRecord, RuntimeKind};
use crate::grill::runc_intent::NetworkReference;
use crate::grill::{RuntimeGeneration, RuntimeLaunch};

/// A started instance's runtime facts, as the runtime reported them.
#[derive(Debug, Clone, Default)]
pub(super) struct LaunchEvidence {
    /// The runtime's process for the instance. Apple workloads live in VMs,
    /// so their records name the launcher instead and this stays `None`.
    pub(super) pid: Option<u32>,
    /// Base path of the instance's captured logs, when they go to files.
    pub(super) log_stem: Option<PathBuf>,
    /// Rootless userspace-network ownership, for the adoption record.
    pub(super) rootless_network: Option<RootlessNetworkRecord>,
    /// The address the workload listens on, when it has its own.
    pub(super) container_ip: Option<Ipv4Addr>,
    /// Which execution the runtime ran, for the discovery journal.
    pub(super) execution: LaunchExecution,
}

impl LaunchEvidence {
    /// Ask the runtime about a started instance.
    pub(super) async fn read<G: Grill>(grill: &G, id: &InstanceId) -> Self {
        let pid = match grill.runtime_kind() {
            RuntimeKind::Apple => None,
            // A pid the runtime couldn't read is left out of the record, as
            // one it doesn't report is: adoption then relies on the runtime.
            _ => grill.pid(id).await.ok().flatten(),
        };
        Self {
            pid,
            log_stem: grill.log_stem(id).await,
            rootless_network: grill.rootless_network_record(id).await,
            container_ip: grill.container_ip(id).await,
            execution: LaunchExecution::read(grill, id).await,
        }
    }
}

/// What the runtime's launch inventory says about a started instance's
/// execution. The discovery journal records it for every backend it
/// publishes that holds no address of its own, so recovery can tell that
/// execution from a later one (#419).
///
/// It used to read the whole inventory itself, inside the turn, on every
/// publication. The instance's generation only changes when something
/// starts it again, and whatever starts it reads this, so the loop keeps
/// what it's handed instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum LaunchExecution {
    /// Nothing to record: the runtime can't enumerate its launches, or this
    /// launch holds its address and the journal's reference covers it.
    #[default]
    Unrecorded,
    /// The generation the runtime ran.
    Generation(RuntimeGeneration),
    /// The inventory couldn't be read, for this reason.
    Unknown(String),
}

impl LaunchExecution {
    /// Read `id`'s execution from the runtime's launch inventory, waiting at
    /// most [`RUNTIME_INVENTORY_TIMEOUT`].
    pub(super) async fn read<G: Grill>(grill: &G, id: &InstanceId) -> Self {
        let inventory =
            tokio::time::timeout(RUNTIME_INVENTORY_TIMEOUT, grill.launch_inventory()).await;
        match inventory {
            Err(_) => Self::Unknown("runtime inventory timed out".into()),
            Ok(Err(error)) => Self::Unknown(error.to_string()),
            Ok(Ok(launches)) => Self::of(launches.as_deref().unwrap_or_default(), id),
        }
    }

    /// `id`'s execution in a complete launch inventory.
    pub(super) fn of(launches: &[RuntimeLaunch], id: &InstanceId) -> Self {
        launches
            .iter()
            .find(|launch| launch.instance_id == *id && launch.network_reference.is_none())
            .map_or(Self::Unrecorded, |launch| {
                Self::Generation(launch.generation.clone())
            })
    }
}

/// How long a pre-start waits for its egress allowlist's DNS before the
/// instance starts deny-all, for the re-resolution loop to repair.
const EGRESS_DNS_PATIENCE: Duration = Duration::from_secs(5);

/// An instance's egress allowlist, resolved off the agent loop by whoever
/// prepares its start (#419), for the loop to program before the start.
#[derive(Debug, Clone, Default)]
#[cfg_attr(not(all(feature = "ebpf", target_os = "linux")), allow(dead_code))]
pub(super) struct EgressResolution {
    /// The allowlist that was resolved.
    pub(super) allow: Vec<String>,
    /// What it resolved to. Empty when DNS failed or took longer than
    /// [`EGRESS_DNS_PATIENCE`]: the instance then starts deny-all, and the
    /// re-resolution loop fills the allowlist in later.
    pub(super) destinations: Vec<crate::sesame::egress::EgressDestination>,
}

/// Resolves egress allowlists for a deploy worker or a restart step.
#[derive(Debug, Clone, Default)]
pub(super) struct EgressResolver {
    /// Lets the starvation harness slow the lookups down.
    #[cfg(test)]
    pub(super) stalls: std::sync::Arc<super::LoopStalls>,
}

impl EgressResolver {
    /// Resolve `spec`'s egress allowlist, waiting at most
    /// [`EGRESS_DNS_PATIENCE`]. A spec without one resolves to nothing,
    /// without a lookup.
    pub(super) async fn resolve(&self, spec: Option<&AppSpec>) -> EgressResolution {
        let allow = spec
            .and_then(|spec| spec.egress.as_ref())
            .map(|policy| policy.allow.clone())
            .unwrap_or_default();
        let destinations = self.resolve_allowlist(&allow).await;
        EgressResolution {
            allow,
            destinations,
        }
    }

    /// Resolve an allowlist, waiting at most [`EGRESS_DNS_PATIENCE`], and
    /// deny every destination when DNS fails or is slower than that.
    pub(super) async fn resolve_allowlist(
        &self,
        allow: &[String],
    ) -> Vec<crate::sesame::egress::EgressDestination> {
        if allow.is_empty() {
            return Vec::new();
        }
        #[cfg(test)]
        self.stalls.hold(super::LoopStall::EgressDns).await;
        let entries = allow.to_vec();
        let lookup = tokio::task::spawn_blocking(move || {
            crate::sesame::egress::resolve_egress_entries(&entries)
        });
        match tokio::time::timeout(EGRESS_DNS_PATIENCE, lookup).await {
            Ok(Ok(Ok(destinations))) => destinations,
            _ => {
                eprintln!(
                    "sesame: egress resolution unavailable; retaining deny-all until re-resolution"
                );
                Vec::new()
            }
        }
    }
}

/// Whether an instance of `spec` holds its address for discovery, so its
/// network reference must be retained before it starts.
pub(super) fn retains_network(spec: Option<&AppSpec>) -> bool {
    spec.is_some_and(|spec| spec.port.is_some())
}

/// Retain a created instance's network reference, for an instance that
/// publishes an address. The loop checks and records what comes back.
pub(super) async fn retain_network<G: Grill>(
    grill: &G,
    id: &InstanceId,
    spec: Option<&AppSpec>,
) -> Result<Option<NetworkReference>, crate::grill::GrillError> {
    if !retains_network(spec) {
        return Ok(None);
    }
    grill.retain_network_reference(id).await
}

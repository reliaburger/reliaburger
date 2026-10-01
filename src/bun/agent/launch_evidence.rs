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

use super::{AppSpec, Grill, InstanceId};
use crate::grill::records::{RootlessNetworkRecord, RuntimeKind};
use crate::grill::runc_intent::NetworkReference;

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

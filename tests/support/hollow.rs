//! Hollow Buns: real agents with the container runtime taken out (#1401).
//!
//! A hollow node runs what a worker's `bun --cluster` runs for the council
//! to see: gossip, the Raft endpoint every node keeps, the reporting worker,
//! a real `BunAgent` and its placement reconciler, which pulls placements
//! from the leader. Only the runtime is fake. [`HollowGrill`] "runs" every
//! workload the moment it's asked to, and each node reports whatever
//! capacity the harness gives it. Hundreds of them share one process, so a
//! laptop can put a real council in front of a fleet it couldn't afford.
//!
//! Kubernetes calls the same idea Kubemark. Nothing here is compiled into
//! the library or the release binary: the module lives under `tests/`, so
//! only the test binaries that include it ever build it.
//!
//! Included with `#[path = "support/hollow.rs"] mod hollow;`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Mutex, mpsc, watch};
use tokio_util::sync::CancellationToken;

use reliaburger::bun::agent::{BunAgent, StatusReader};
use reliaburger::cluster::orchestrate::spawn_placement_reconciler;
use reliaburger::cluster::runtime::{self, ClusterParams, ClusterRuntime};
use reliaburger::config::node::ReportingTreeSection;
use reliaburger::grill::oci::OciSpec;
use reliaburger::grill::port::PortAllocator;
use reliaburger::grill::state::ContainerState;
use reliaburger::grill::{Grill, GrillError, InstanceId};
use reliaburger::mustard::membership::MembershipSnapshot;

// The includer brings `task_harness` in at its crate root (through
// `cluster.rs`), so the crate compiles one `TestTasks`, not two.
use crate::task_harness::TestTasks;

/// Ports per node, in this order: gossip, Raft, reporting, API.
pub const PORTS_PER_NODE: u16 = 4;

/// Counts shared by every hollow runtime in a fleet.
#[derive(Debug, Default)]
pub struct FleetCounters {
    /// Workloads started and not yet stopped, across the whole fleet.
    pub running: AtomicUsize,
    /// Every `start` call the fleet has seen.
    pub starts: AtomicUsize,
}

/// A container runtime that runs nothing.
///
/// `create` and `start` succeed at once and the workload counts as running
/// until `stop` or `kill`. It claims to run containers, so the scheduler
/// places image apps on it, and it never pulls an image. State answers
/// follow the shared `MockGrill`: an instance this runtime never stopped is
/// running, which is how a live process looks to the agent.
#[derive(Clone)]
pub struct HollowGrill {
    /// What each instance last did. A tokio mutex, because every caller is
    /// async; no lock is held across an `.await`.
    instances: Arc<Mutex<HashMap<InstanceId, ContainerState>>>,
    counters: Arc<FleetCounters>,
}

impl HollowGrill {
    /// A runtime whose starts and stops count towards `counters`.
    pub fn new(counters: Arc<FleetCounters>) -> Self {
        Self {
            instances: Arc::new(Mutex::new(HashMap::new())),
            counters,
        }
    }

    /// How many workloads this runtime is running now.
    pub async fn running(&self) -> usize {
        self.instances
            .lock()
            .await
            .values()
            .filter(|state| **state == ContainerState::Running)
            .count()
    }

    /// Mark `instance` stopped, keeping the fleet count right.
    async fn halt(&self, instance: &InstanceId) {
        let previous = self
            .instances
            .lock()
            .await
            .insert(instance.clone(), ContainerState::Stopped);
        if previous == Some(ContainerState::Running) {
            self.counters.running.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Grill for HollowGrill {
    fn runs_containers(&self) -> bool {
        true
    }

    async fn create(&self, instance: &InstanceId, _spec: &OciSpec) -> Result<(), GrillError> {
        let previous = self
            .instances
            .lock()
            .await
            .insert(instance.clone(), ContainerState::Starting);
        if previous == Some(ContainerState::Running) {
            self.counters.running.fetch_sub(1, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let previous = self
            .instances
            .lock()
            .await
            .insert(instance.clone(), ContainerState::Running);
        self.counters.starts.fetch_add(1, Ordering::Relaxed);
        if previous != Some(ContainerState::Running) {
            self.counters.running.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn stop(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.halt(instance).await;
        Ok(())
    }

    async fn kill(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.halt(instance).await;
        Ok(())
    }

    async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
        Ok(self
            .instances
            .lock()
            .await
            .get(instance)
            .copied()
            .unwrap_or(ContainerState::Running))
    }
}

/// How to start one hollow node.
pub struct HollowSpec {
    pub name: String,
    /// First port of the node's block: gossip, +1 Raft, +2 reporting, +3 API.
    pub gossip_port: u16,
    pub seeds: Vec<SocketAddr>,
    /// The node keeps its data in `data_root/name`.
    pub data_root: PathBuf,
    /// The capacity the node reports: millicores and MiB.
    pub cpu_millicores: u32,
    pub memory_mb: u32,
    pub labels: std::collections::BTreeMap<String, String>,
    pub counters: Arc<FleetCounters>,
    /// How often the node reports to the leader. Bun's default is five
    /// seconds.
    pub report_interval_secs: u64,
}

/// A running hollow node.
pub struct HollowNode {
    pub name: String,
    pub gossip_port: u16,
    /// Live gossip members as this node sees them.
    pub membership_rx: watch::Receiver<Vec<MembershipSnapshot>>,
    pub status: StatusReader,
    pub grill: HollowGrill,
    /// Quiescing this gate drops all of the node's cluster traffic, as if
    /// its host lost power: no gossip goodbye, no last report.
    pub gate: reliaburger::smoker::node_fault::NodeTransportGate,
    /// Cancelling this token stops the node.
    pub shutdown: CancellationToken,
    _runtime: ClusterRuntime,
    _tasks: TestTasks,
}

impl HollowNode {
    /// How many other members this node's gossip shows alive.
    pub fn alive_peers(&self) -> usize {
        self.membership_rx
            .borrow()
            .iter()
            .filter(|member| member.node_id.0 != self.name)
            .count()
    }
}

/// Start one hollow node. A bind failure comes back as an error, so a
/// harness can say which port another process took.
pub async fn start_hollow_node(spec: HollowSpec) -> std::io::Result<HollowNode> {
    let HollowSpec {
        name,
        gossip_port,
        seeds,
        data_root,
        cpu_millicores,
        memory_mb,
        labels,
        counters,
        report_interval_secs,
    } = spec;
    let shutdown = CancellationToken::new();
    let data_dir = data_root.join(&name);
    std::fs::create_dir_all(&data_dir)?;
    // The scheduler places only on nodes whose reports say they're ready,
    // and a node is ready once its agent is.
    let readiness = reliaburger::bun::readiness::ReadinessTracker::new();
    readiness.register("agent", true).await;

    let (handle, cluster_runtime) = runtime::start(
        ClusterParams {
            node_name: name.clone(),
            gossip_addr: SocketAddr::from(([127, 0, 0, 1], gossip_port)),
            raft_port: gossip_port + 1,
            reporting_port: gossip_port + 2,
            api_port: gossip_port + 3,
            reporting_config: ReportingTreeSection {
                report_interval_secs,
                ..Default::default()
            },
            seeds,
            wrapping_ikm: None,
            bootstrap_security_state: None,
            data_dir: data_dir.clone(),
            mayo: None,
            rollup_interval: std::time::Duration::from_secs(60),
            identity: None,
            backup: Default::default(),
            labels,
            self_disk_pressured_rx: None,
            job_capabilities_rx: None,
            readiness: Some(readiness.clone()),
            recovery_probe_window: None,
            council_size_cap: None,
        },
        shutdown.clone(),
    )
    .await?;
    let membership_rx = handle.membership_rx.clone();
    let metrics_rx = handle
        .raft_metrics_rx
        .clone()
        .ok_or_else(|| std::io::Error::other("cluster mode has no Raft metrics"))?;
    let directory_rx = cluster_runtime.directory_rx.clone();
    let gate = handle.partition_blocklists.node_gate.clone();

    let grill = HollowGrill::new(counters);
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    // Nothing binds a workload port on a hollow node, so every node can
    // hand out the same range.
    let mut agent = BunAgent::with_cluster(
        grill.clone(),
        PortAllocator::new(50_000, 51_000),
        cmd_rx,
        shutdown.clone(),
        handle,
        "default".to_string(),
    );
    agent.set_volumes_dir(data_dir.join("volumes"));
    agent.set_node_capacity(cpu_millicores, memory_mb);
    agent.set_perimeter_enabled(false);
    agent.set_readiness_tracker(readiness.clone());
    let status = agent.status_reader();
    let agent_task = reliaburger::bun::readiness::spawn_owned(
        "agent",
        true,
        readiness,
        shutdown.clone(),
        move |ready| async move { agent.run_with_readiness(ready).await },
    );

    let reconciler = spawn_placement_reconciler(
        name.clone(),
        metrics_rx,
        directory_rx,
        2, // API = Raft + 2 in this port block
        None,
        cmd_tx,
        shutdown.clone(),
        reliaburger::cluster::ClusterHttp::plaintext(),
        Some(data_dir),
        reliaburger::config::node::RuntimeSection::default().stop_confirmation_timeout(),
    );

    Ok(HollowNode {
        name,
        gossip_port,
        membership_rx,
        status,
        grill,
        gate,
        shutdown: shutdown.clone(),
        _runtime: cluster_runtime,
        _tasks: TestTasks::new(shutdown, vec![agent_task, reconciler]),
    })
}

/// Whether every port of the block at `base` is free on loopback now.
pub fn block_is_free(base: u16) -> bool {
    (base..base + PORTS_PER_NODE).all(|port| {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
            && std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok()
    })
}

/// `count` free port blocks between `start` and `end`, skipping any block
/// another process holds. Hollow fleets need more ports than the shared
/// test range has, so they take theirs from a range of their own.
pub fn free_blocks(count: usize, start: u16, end: u16) -> Vec<u16> {
    let mut blocks = Vec::with_capacity(count);
    let mut base = start;
    while blocks.len() < count && base + PORTS_PER_NODE <= end {
        if block_is_free(base) {
            blocks.push(base);
        }
        base += PORTS_PER_NODE;
    }
    blocks
}

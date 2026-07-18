/// Worker-side report sender.
///
/// Runs on each non-council node as a spawned task. Periodically
/// collects state from the local agent and sends a `StateReport`
/// to the assigned council member.
use std::net::SocketAddr;
use std::time::{Duration, SystemTime};

use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::config::node::ReportingTreeSection;
use crate::grill::state::ContainerState;
use crate::meat::NodeId;

use super::assignment::assign_parent;
use super::transport::ReportingTransport;
use super::types::{
    AppResourceUsage, EgressAffectedWorkload, EgressEnforcementEvidence, EgressEnforcementStatus,
    NodeCapabilityReport, ReportHealthStatus, ReportingMessage, ResourceUsage, RunningApp,
    StateReport,
};

/// Snapshot of a single workload instance, provided by the agent.
///
/// This is the data the agent extracts from `WorkloadSupervisor`
/// without exposing the full supervisor type. The worker maps this
/// to `RunningApp` for the StateReport.
#[derive(Debug, Clone)]
pub struct InstanceSnapshot {
    /// App name.
    pub app_name: String,
    /// Namespace the app belongs to.
    pub namespace: String,
    /// Instance index (parsed from InstanceId, e.g. "web-0" -> 0).
    pub instance_id: u32,
    /// OCI image reference (or empty for process workloads).
    pub image: String,
    /// Allocated host port.
    pub port: Option<u16>,
    /// Current container state.
    pub container_state: ContainerState,
    /// Consecutive unhealthy probe count.
    pub consecutive_unhealthy: u32,
    /// When the instance was created (for uptime calculation).
    pub uptime: Duration,
    /// CPU requested by the instance's spec (millicores).
    pub cpu_request_millicores: u32,
    /// Memory requested by the instance's spec (MB).
    pub memory_request_mb: u32,
    /// Live egress policy evidence for this instance.
    pub egress_enforcement: EgressEnforcementStatus,
}

/// Full snapshot of the agent's state for building a StateReport.
#[derive(Debug, Clone)]
pub struct AgentSnapshot {
    /// All running instances.
    pub instances: Vec<InstanceSnapshot>,
    /// Allocated ports across all instances.
    pub allocated_ports: Vec<u16>,
    /// Schedulable CPU capacity (system total minus reserved).
    pub capacity_cpu_millicores: u32,
    /// Schedulable memory capacity (system total minus reserved).
    pub capacity_memory_mb: u32,
    /// Live capabilities used by the scheduler for placement.
    pub capabilities: crate::meat::cluster_state::NodeCapabilities,
    /// Whether a live egress enforcement incident is still active.
    pub egress_degraded: bool,
    /// Workloads fenced during the active incident.
    pub egress_affected_workloads: Vec<EgressAffectedWorkload>,
}

/// Request sent to the agent to collect a state snapshot.
///
/// The agent handles this in its event loop and responds with an
/// `AgentSnapshot` via the oneshot channel.
pub struct CollectSnapshotRequest {
    pub response: oneshot::Sender<AgentSnapshot>,
}

/// Periodically sends state reports to the assigned council member.
pub struct ReportWorker<T: ReportingTransport> {
    node_id: NodeId,
    transport: T,
    config: ReportingTreeSection,
    /// Address of the current parent council member.
    parent_address: Option<SocketAddr>,
    /// Channel to request state snapshots from the agent.
    snapshot_tx: mpsc::Sender<CollectSnapshotRequest>,
    /// Receives council membership updates as `(NodeId, SocketAddr)` pairs.
    council_rx: watch::Receiver<Vec<(NodeId, SocketAddr)>>,
    shutdown: CancellationToken,
    /// Whether `buildah` is on PATH, probed once at construction and
    /// carried in every report (build routing, Phase 12 F2).
    has_buildah: bool,
}

impl<T: ReportingTransport> ReportWorker<T> {
    /// Create a new report worker.
    pub fn new(
        node_id: NodeId,
        transport: T,
        config: ReportingTreeSection,
        snapshot_tx: mpsc::Sender<CollectSnapshotRequest>,
        council_rx: watch::Receiver<Vec<(NodeId, SocketAddr)>>,
        shutdown: CancellationToken,
    ) -> Self {
        // Compute initial parent from current council membership
        let parent_address = Self::compute_parent(&node_id, &council_rx.borrow());
        // One synchronous probe at startup; buildah appearing later
        // needs an agent restart, which is fine for a build host.
        let has_buildah = std::process::Command::new("buildah")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        Self {
            node_id,
            transport,
            config,
            parent_address,
            snapshot_tx,
            council_rx,
            shutdown,
            has_buildah,
        }
    }

    /// Run the worker event loop until shutdown.
    pub async fn run(&mut self) {
        let interval_duration = Duration::from_secs(self.config.report_interval_secs);
        let mut interval = tokio::time::interval(interval_duration);
        // Skip the first tick (fires immediately)
        interval.tick().await;

        // Set to false when the leader-target watch closes. Polling a closed
        // watch resolves instantly with Err on every iteration — a hot spin
        // (CP10) — so the guard drops that select arm instead of the worker:
        // reporting must outlive the maintainer, degraded to the last known
        // target, and only the shutdown token ends this task.
        let mut watch_open = true;

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = interval.tick() => {
                    self.send_report().await;
                }
                result = self.council_rx.changed(), if watch_open => {
                    match result {
                        Ok(()) => {
                            // Re-point immediately on a leader change so a
                            // fresh leader's aggregator fills without waiting
                            // out the report interval.
                            if self.update_parent() {
                                self.send_report().await;
                            }
                        }
                        Err(_) => {
                            watch_open = false;
                            if !self.shutdown.is_cancelled() {
                                eprintln!(
                                    "report worker: leader-target channel closed; \
                                     reporting to the last known target"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Recompute the parent assignment from the current council membership.
    /// Returns `true` if the parent actually changed.
    fn update_parent(&mut self) -> bool {
        let council = self.council_rx.borrow().clone();
        let new_parent = Self::compute_parent(&self.node_id, &council);
        let changed = new_parent != self.parent_address;
        self.parent_address = new_parent;
        changed
    }

    /// Determine the parent address from a council membership list.
    fn compute_parent(node_id: &NodeId, council: &[(NodeId, SocketAddr)]) -> Option<SocketAddr> {
        let council_ids: Vec<NodeId> = council.iter().map(|(id, _)| id.clone()).collect();
        let parent_id = assign_parent(node_id, &council_ids)?;
        council
            .iter()
            .find(|(id, _)| *id == parent_id)
            .map(|(_, addr)| *addr)
    }

    /// Collect state and send a report to the parent.
    async fn send_report(&self) {
        let parent = match self.parent_address {
            Some(addr) => addr,
            None => return, // no council — nothing to report to
        };

        let snapshot = match self.collect_snapshot().await {
            Some(s) => s,
            None => return, // agent didn't respond
        };

        let capability_report = self.build_capability_report(&snapshot);
        let report = self.build_report(snapshot);
        let _ = self
            .transport
            .send(parent, &ReportingMessage::Report(report))
            .await;
        let _ = self
            .transport
            .send(
                parent,
                &ReportingMessage::CapabilityReport(capability_report),
            )
            .await;
    }

    /// Request a snapshot from the agent via the command channel.
    async fn collect_snapshot(&self) -> Option<AgentSnapshot> {
        let (tx, rx) = oneshot::channel();
        let request = CollectSnapshotRequest { response: tx };

        self.snapshot_tx.send(request).await.ok()?;

        // Use a short timeout so we don't block the reporting loop
        // if the agent is busy.
        tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .ok()?
            .ok()
    }

    /// Build a StateReport from an agent snapshot.
    ///
    /// Terminal instances (stopped, failed, completed jobs) are excluded
    /// from `running_apps` and from the resource-usage sums: they hold no
    /// resources any more, and counting them made the leader see phantom
    /// capacity commitments (CP6).
    fn build_report(&self, snapshot: AgentSnapshot) -> StateReport {
        let active: Vec<&InstanceSnapshot> = snapshot
            .instances
            .iter()
            .filter(|inst| !is_terminal_state(inst.container_state))
            .collect();

        let running_apps = active
            .iter()
            .map(|inst| {
                let health_status = match inst.container_state {
                    ContainerState::Running => ReportHealthStatus::Healthy,
                    ContainerState::HealthWait | ContainerState::Starting => {
                        ReportHealthStatus::Starting
                    }
                    ContainerState::Unhealthy => ReportHealthStatus::Unhealthy {
                        consecutive_failures: inst.consecutive_unhealthy,
                    },
                    _ => ReportHealthStatus::Unknown,
                };

                RunningApp {
                    app_name: inst.app_name.clone(),
                    namespace: inst.namespace.clone(),
                    instance_id: inst.instance_id,
                    image: inst.image.clone(),
                    port: inst.port,
                    health_status,
                    uptime: inst.uptime,
                    resource_usage: AppResourceUsage {
                        cpu_millicores: inst.cpu_request_millicores,
                        memory_mb: inst.memory_request_mb,
                    },
                }
            })
            .collect();

        // "Used" is the sum of requests over ACTIVE instances — the
        // commitments the scheduler must respect, not measured load.
        let cpu_used: u32 = active.iter().map(|i| i.cpu_request_millicores).sum();
        let memory_used: u32 = active.iter().map(|i| i.memory_request_mb).sum();

        StateReport {
            node_id: self.node_id.clone(),
            timestamp: SystemTime::now(),
            running_apps,
            cached_specs: vec![],
            resource_usage: ResourceUsage {
                cpu_used_millicores: cpu_used,
                memory_used_mb: memory_used,
                disk_used_mb: 0,
                gpu_used: 0,
                allocated_ports: snapshot.allocated_ports,
                cpu_total_millicores: snapshot.capacity_cpu_millicores,
                memory_total_mb: snapshot.capacity_memory_mb,
            },
            event_log: vec![],
            has_buildah: self.has_buildah,
        }
    }

    /// Build the additive capability message. An old peer may reject this
    /// separate extension frame, but it still accepts the preceding legacy
    /// `StateReport` frame.
    fn build_capability_report(&self, snapshot: &AgentSnapshot) -> NodeCapabilityReport {
        NodeCapabilityReport {
            node_id: self.node_id.clone(),
            capabilities: snapshot.capabilities,
            egress_enforcement: snapshot
                .instances
                .iter()
                .filter(|instance| {
                    instance.egress_enforcement != EgressEnforcementStatus::NotRequested
                })
                .map(|instance| EgressEnforcementEvidence {
                    app_name: instance.app_name.clone(),
                    namespace: instance.namespace.clone(),
                    instance_id: instance.instance_id,
                    status: instance.egress_enforcement,
                })
                .collect(),
            egress_degraded: snapshot.egress_degraded,
            egress_affected_workloads: snapshot.egress_affected_workloads.clone(),
        }
    }
}

/// Whether an instance state is terminal — no process, no held resources.
///
/// `Stopping` is deliberately NOT terminal: a draining instance still
/// occupies its port and memory until it finishes.
fn is_terminal_state(state: ContainerState) -> bool {
    matches!(state, ContainerState::Stopped | ContainerState::Failed)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::transport::InMemoryReportingNetwork;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn test_config() -> ReportingTreeSection {
        ReportingTreeSection {
            report_interval_secs: 1,
            max_events_per_report: 100,
            stale_report_timeout_secs: 30,
        }
    }

    /// Helper: spawn a fake agent that responds to snapshot requests.
    fn spawn_fake_agent(
        mut rx: mpsc::Receiver<CollectSnapshotRequest>,
        shutdown: CancellationToken,
    ) {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    req = rx.recv() => {
                        if let Some(req) = req {
                            let snapshot = AgentSnapshot {
                                capabilities: Default::default(),
                                egress_degraded: true,
                                egress_affected_workloads: vec![EgressAffectedWorkload {
                                    app_name: "web".to_string(),
                                    namespace: "default".to_string(),
                                }],
                                instances: vec![InstanceSnapshot {
                                    app_name: "web".to_string(),
                                    namespace: "default".to_string(),
                                    instance_id: 0,
                                    image: "nginx:latest".to_string(),
                                    port: Some(8080),
                                    container_state: ContainerState::Running,
                                    consecutive_unhealthy: 0,
                                    uptime: Duration::from_secs(120),
                                    cpu_request_millicores: 250,
                                    memory_request_mb: 128,
                                    egress_enforcement: Default::default(),
                                }],
                                allocated_ports: vec![8080],
                                capacity_cpu_millicores: 7500,
                                capacity_memory_mb: 15_872,
                            };
                            let _ = req.response.send(snapshot);
                        } else {
                            break;
                        }
                    }
                }
            }
        });
    }

    #[tokio::test]
    async fn sends_report_at_interval() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let council_transport = net.register(addr(2)).await;
        let shutdown = CancellationToken::new();

        let (snapshot_tx, snapshot_rx) = mpsc::channel(16);
        spawn_fake_agent(snapshot_rx, shutdown.clone());

        let council = vec![(NodeId::new("c1"), addr(2))];
        let (_council_tx, council_rx) = watch::channel(council);

        let mut worker = ReportWorker::new(
            NodeId::new("w1"),
            worker_transport,
            test_config(),
            snapshot_tx,
            council_rx,
            shutdown.clone(),
        );

        let handle = tokio::spawn(async move { worker.run().await });

        // The council transport should receive a report within 2 seconds
        let result = tokio::time::timeout(Duration::from_secs(2), council_transport.recv()).await;
        assert!(result.is_ok(), "should receive a report");

        let (from, msg) = result.unwrap().unwrap();
        assert_eq!(from, addr(1));
        match msg {
            ReportingMessage::Report(r) => {
                assert_eq!(r.node_id, NodeId::new("w1"));
                assert_eq!(r.running_apps.len(), 1);
                assert_eq!(r.running_apps[0].app_name, "web");
                assert_eq!(r.running_apps[0].health_status, ReportHealthStatus::Healthy);
                // L6: reports must carry real capacity and commitments,
                // not the zeroed placeholders they used to.
                assert_eq!(r.resource_usage.cpu_used_millicores, 250);
                assert_eq!(r.resource_usage.memory_used_mb, 128);
                assert_eq!(r.resource_usage.cpu_total_millicores, 7500);
                assert_eq!(r.resource_usage.memory_total_mb, 15_872);
                assert_eq!(r.running_apps[0].resource_usage.cpu_millicores, 250);
            }
            _ => panic!("expected Report"),
        }

        let (_, msg) = tokio::time::timeout(Duration::from_secs(1), council_transport.recv())
            .await
            .expect("should receive capability evidence after the state report")
            .unwrap();
        let ReportingMessage::CapabilityReport(capability) = msg else {
            panic!("expected CapabilityReport");
        };
        assert_eq!(capability.node_id, NodeId::new("w1"));
        assert!(capability.egress_degraded);
        assert_eq!(
            capability.egress_affected_workloads,
            vec![EgressAffectedWorkload {
                app_name: "web".to_string(),
                namespace: "default".to_string(),
            }]
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    fn instance(name: &str, state: ContainerState, cpu: u32, memory: u32) -> InstanceSnapshot {
        InstanceSnapshot {
            app_name: name.to_string(),
            namespace: "default".to_string(),
            instance_id: 0,
            image: "proc-grill:image-ignored".to_string(),
            port: None,
            container_state: state,
            consecutive_unhealthy: 0,
            uptime: Duration::from_secs(10),
            cpu_request_millicores: cpu,
            memory_request_mb: memory,
            egress_enforcement: Default::default(),
        }
    }

    #[tokio::test]
    async fn terminal_instances_are_excluded_from_running_and_capacity() {
        // CP6: 2 running + 3 terminal (stopped/failed/completed job) must
        // report 2 running apps and only THEIR resource commitments.
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();
        let (snapshot_tx, _snapshot_rx) = mpsc::channel(16);
        let (_council_tx, council_rx) = watch::channel(vec![(NodeId::new("c1"), addr(2))]);

        let worker = ReportWorker::new(
            NodeId::new("w1"),
            worker_transport,
            test_config(),
            snapshot_tx,
            council_rx,
            shutdown,
        );

        let snapshot = AgentSnapshot {
            capabilities: Default::default(),
            egress_degraded: false,
            egress_affected_workloads: Vec::new(),
            instances: vec![
                instance("web", ContainerState::Running, 250, 128),
                instance("api", ContainerState::Running, 300, 256),
                instance("done-job-1", ContainerState::Stopped, 500, 512),
                instance("done-job-2", ContainerState::Stopped, 500, 512),
                instance("crashed", ContainerState::Failed, 500, 512),
            ],
            allocated_ports: vec![],
            capacity_cpu_millicores: 8000,
            capacity_memory_mb: 16384,
        };
        let report = worker.build_report(snapshot);

        assert_eq!(report.running_apps.len(), 2);
        let names: Vec<&str> = report
            .running_apps
            .iter()
            .map(|a| a.app_name.as_str())
            .collect();
        assert!(names.contains(&"web") && names.contains(&"api"));
        assert_eq!(report.resource_usage.cpu_used_millicores, 550);
        assert_eq!(report.resource_usage.memory_used_mb, 384);
    }

    #[tokio::test]
    async fn draining_instances_still_count_as_running() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();
        let (snapshot_tx, _snapshot_rx) = mpsc::channel(16);
        let (_council_tx, council_rx) = watch::channel(vec![(NodeId::new("c1"), addr(2))]);

        let worker = ReportWorker::new(
            NodeId::new("w1"),
            worker_transport,
            test_config(),
            snapshot_tx,
            council_rx,
            shutdown,
        );

        let snapshot = AgentSnapshot {
            capabilities: Default::default(),
            egress_degraded: false,
            egress_affected_workloads: Vec::new(),
            instances: vec![instance("web", ContainerState::Stopping, 250, 128)],
            allocated_ports: vec![],
            capacity_cpu_millicores: 8000,
            capacity_memory_mb: 16384,
        };
        let report = worker.build_report(snapshot);
        // A draining instance still holds its resources.
        assert_eq!(report.running_apps.len(), 1);
        assert_eq!(report.resource_usage.cpu_used_millicores, 250);
    }

    #[tokio::test]
    async fn worker_reports_to_last_known_target_after_the_watch_closes() {
        // CP10: a dropped leader-target sender used to make `changed()`
        // resolve instantly with Err on every iteration — a hot spin. The
        // worker must keep reporting to the last known target without
        // spinning; only the shutdown token ends it.
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let c1_transport = net.register(addr(2)).await;
        let shutdown = CancellationToken::new();
        let (snapshot_tx, snapshot_rx) = mpsc::channel(16);
        spawn_fake_agent(snapshot_rx, shutdown.clone());
        let (council_tx, council_rx) = watch::channel(vec![(NodeId::new("c1"), addr(2))]);

        let mut worker = ReportWorker::new(
            NodeId::new("w1"),
            worker_transport,
            test_config(),
            snapshot_tx,
            council_rx,
            shutdown.clone(),
        );
        let handle = tokio::spawn(async move { worker.run().await });

        // Close the watch before the first report interval elapses.
        drop(council_tx);

        // The worker must still deliver a report to the last known target.
        let result = tokio::time::timeout(Duration::from_secs(5), c1_transport.recv()).await;
        assert!(
            result.is_ok(),
            "worker must keep reporting after the council watch closes"
        );

        // And it must still exit on shutdown (not spin, not hang).
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("worker must exit on shutdown")
            .unwrap();
    }

    #[tokio::test]
    async fn updates_parent_on_council_change() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let _c1_transport = net.register(addr(2)).await;
        let c2_transport = net.register(addr(3)).await;
        let shutdown = CancellationToken::new();

        let (snapshot_tx, snapshot_rx) = mpsc::channel(16);
        spawn_fake_agent(snapshot_rx, shutdown.clone());

        // Start with council member c1 only
        let initial_council = vec![(NodeId::new("c1"), addr(2))];
        let (council_tx, council_rx) = watch::channel(initial_council);

        let mut worker = ReportWorker::new(
            NodeId::new("w1"),
            worker_transport,
            test_config(),
            snapshot_tx,
            council_rx,
            shutdown.clone(),
        );

        let handle = tokio::spawn(async move { worker.run().await });

        // Change council to c2 only
        council_tx.send(vec![(NodeId::new("c2"), addr(3))]).unwrap();

        // c2 should receive a report
        let result = tokio::time::timeout(Duration::from_secs(3), c2_transport.recv()).await;
        assert!(
            result.is_ok(),
            "c2 should receive a report after council change"
        );

        let (_, msg) = result.unwrap().unwrap();
        match msg {
            ReportingMessage::Report(r) => assert_eq!(r.node_id, NodeId::new("w1")),
            _ => panic!("expected Report"),
        }

        shutdown.cancel();
        let _ = handle.await;
    }
}

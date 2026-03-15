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
    AppResourceUsage, ReportHealthStatus, ReportingMessage, ResourceUsage, RunningApp, StateReport,
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
}

/// Full snapshot of the agent's state for building a StateReport.
#[derive(Debug, Clone)]
pub struct AgentSnapshot {
    /// All running instances.
    pub instances: Vec<InstanceSnapshot>,
    /// Allocated ports across all instances.
    pub allocated_ports: Vec<u16>,
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
        Self {
            node_id,
            transport,
            config,
            parent_address,
            snapshot_tx,
            council_rx,
            shutdown,
        }
    }

    /// Run the worker event loop until shutdown.
    pub async fn run(&mut self) {
        let interval_duration = Duration::from_secs(self.config.report_interval_secs);
        let mut interval = tokio::time::interval(interval_duration);
        // Skip the first tick (fires immediately)
        interval.tick().await;

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = interval.tick() => {
                    self.send_report().await;
                }
                result = self.council_rx.changed() => {
                    if result.is_ok() {
                        self.update_parent();
                    }
                }
            }
        }
    }

    /// Recompute the parent assignment from the current council membership.
    fn update_parent(&mut self) {
        let council = self.council_rx.borrow().clone();
        self.parent_address = Self::compute_parent(&self.node_id, &council);
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

        let report = self.build_report(snapshot);
        let _ = self
            .transport
            .send(parent, &ReportingMessage::Report(report))
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
    fn build_report(&self, snapshot: AgentSnapshot) -> StateReport {
        let running_apps = snapshot
            .instances
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
                    resource_usage: AppResourceUsage::default(),
                }
            })
            .collect();

        StateReport {
            node_id: self.node_id.clone(),
            timestamp: SystemTime::now(),
            running_apps,
            cached_specs: vec![],
            resource_usage: ResourceUsage {
                cpu_used_millicores: 0,
                memory_used_mb: 0,
                disk_used_mb: 0,
                gpu_used: 0,
                allocated_ports: snapshot.allocated_ports,
            },
            event_log: vec![],
        }
    }
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
                                instances: vec![InstanceSnapshot {
                                    app_name: "web".to_string(),
                                    namespace: "default".to_string(),
                                    instance_id: 0,
                                    image: "nginx:latest".to_string(),
                                    port: Some(8080),
                                    container_state: ContainerState::Running,
                                    consecutive_unhealthy: 0,
                                    uptime: Duration::from_secs(120),
                                }],
                                allocated_ports: vec![8080],
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
            }
            _ => panic!("expected Report"),
        }

        shutdown.cancel();
        let _ = handle.await;
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

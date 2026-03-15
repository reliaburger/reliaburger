/// Council-side report aggregator.
///
/// Runs on each council member as a spawned task. Receives `StateReport`
/// messages from assigned workers, stores the latest per-node, and
/// publishes the aggregated view via a `tokio::sync::watch` channel.
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::node::ReportingTreeSection;
use crate::meat::NodeId;

use super::transport::ReportingTransport;
use super::types::{ReportingMessage, StateReport};

/// The aggregated view of all worker reports.
///
/// Published via a `watch` channel so any number of consumers
/// (leader, API handlers, etc.) can read the latest state without
/// buffering intermediate values.
#[derive(Debug, Clone, Default)]
pub struct AggregatedState {
    /// Latest report from each worker node.
    pub reports: HashMap<NodeId, StateReport>,
    /// Nodes whose last report is older than `stale_report_timeout`.
    pub stale_nodes: Vec<NodeId>,
}

/// Aggregates state reports from assigned worker nodes.
pub struct ReportAggregator<T: ReportingTransport> {
    transport: T,
    reports: HashMap<NodeId, StateReport>,
    watch_tx: watch::Sender<AggregatedState>,
    config: ReportingTreeSection,
    shutdown: CancellationToken,
}

impl<T: ReportingTransport> ReportAggregator<T> {
    /// Create a new aggregator.
    ///
    /// Returns the aggregator and a watch receiver for consumers.
    pub fn new(
        transport: T,
        config: ReportingTreeSection,
        shutdown: CancellationToken,
    ) -> (Self, watch::Receiver<AggregatedState>) {
        let (watch_tx, watch_rx) = watch::channel(AggregatedState::default());
        let aggregator = Self {
            transport,
            reports: HashMap::new(),
            watch_tx,
            config,
            shutdown,
        };
        (aggregator, watch_rx)
    }

    /// Run the aggregator event loop until shutdown.
    pub async fn run(&mut self) {
        let stale_timeout = Duration::from_secs(self.config.stale_report_timeout_secs);
        let mut stale_check = tokio::time::interval(stale_timeout);
        // The first tick fires immediately — skip it so we don't
        // mark everything stale at startup.
        stale_check.tick().await;

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                msg = self.transport.recv() => {
                    match msg {
                        Some((_, ReportingMessage::Report(report))) => {
                            self.reports.insert(report.node_id.clone(), report);
                            let _ = self.watch_tx.send(self.build_aggregated_state());
                        }
                        Some((_, ReportingMessage::AggregatedReport { reports })) => {
                            // Merge reports from another council member
                            // (used for leader aggregation).
                            for (node_id, report) in reports {
                                self.reports.insert(node_id, report);
                            }
                            let _ = self.watch_tx.send(self.build_aggregated_state());
                        }
                        Some((_, ReportingMessage::Ack { .. })) => {
                            // Workers don't send Acks to the aggregator
                        }
                        None => break, // transport shut down
                    }
                }
                _ = stale_check.tick() => {
                    let _ = self.watch_tx.send(self.build_aggregated_state());
                }
            }
        }
    }

    /// Snapshot the current state for the watch channel.
    fn build_aggregated_state(&self) -> AggregatedState {
        let stale_timeout = Duration::from_secs(self.config.stale_report_timeout_secs);
        let now = SystemTime::now();

        let stale_nodes = self
            .reports
            .iter()
            .filter_map(|(node_id, report)| {
                if now
                    .duration_since(report.timestamp)
                    .unwrap_or(Duration::ZERO)
                    > stale_timeout
                {
                    Some(node_id.clone())
                } else {
                    None
                }
            })
            .collect();

        AggregatedState {
            reports: self.reports.clone(),
            stale_nodes,
        }
    }

    /// Directly insert a report (for the council member's own state).
    pub fn insert_local_report(&mut self, report: StateReport) {
        self.reports.insert(report.node_id.clone(), report);
        let _ = self.watch_tx.send(self.build_aggregated_state());
    }

    /// Number of reports currently held.
    pub fn report_count(&self) -> usize {
        self.reports.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::transport::InMemoryReportingNetwork;
    use crate::reporting::types::ResourceUsage;
    use std::time::SystemTime;

    fn addr(port: u16) -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn report(name: &str) -> StateReport {
        StateReport {
            node_id: NodeId::new(name),
            timestamp: SystemTime::now(),
            running_apps: vec![],
            cached_specs: vec![],
            resource_usage: ResourceUsage::default(),
            event_log: vec![],
        }
    }

    fn stale_report(name: &str) -> StateReport {
        StateReport {
            node_id: NodeId::new(name),
            // 60 seconds ago — will be stale with 30s timeout
            timestamp: SystemTime::now() - Duration::from_secs(60),
            running_apps: vec![],
            cached_specs: vec![],
            resource_usage: ResourceUsage::default(),
            event_log: vec![],
        }
    }

    fn test_config() -> ReportingTreeSection {
        ReportingTreeSection {
            report_interval_secs: 1,
            max_events_per_report: 100,
            stale_report_timeout_secs: 30,
        }
    }

    #[tokio::test]
    async fn stores_latest_report_per_node() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let council_transport = net.register(addr(2)).await;
        let shutdown = CancellationToken::new();

        let (mut aggregator, mut watch_rx) =
            ReportAggregator::new(council_transport, test_config(), shutdown.clone());

        // Send two reports from the same worker
        let msg1 = ReportingMessage::Report(report("w1"));
        let msg2 = ReportingMessage::Report(report("w1"));
        worker_transport.send(addr(2), &msg1).await.unwrap();
        worker_transport.send(addr(2), &msg2).await.unwrap();

        // Run the aggregator briefly
        let handle = tokio::spawn(async move { aggregator.run().await });

        // Wait for the watch to update
        tokio::time::timeout(Duration::from_millis(100), watch_rx.changed())
            .await
            .unwrap()
            .unwrap();

        let state = watch_rx.borrow();
        // Only one entry for "w1" — second report overwrote the first
        assert_eq!(state.reports.len(), 1);
        assert!(state.reports.contains_key(&NodeId::new("w1")));

        shutdown.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn publishes_via_watch_on_report() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let council_transport = net.register(addr(2)).await;
        let shutdown = CancellationToken::new();

        let (mut aggregator, mut watch_rx) =
            ReportAggregator::new(council_transport, test_config(), shutdown.clone());

        let handle = tokio::spawn(async move { aggregator.run().await });

        // Initially empty
        assert!(watch_rx.borrow().reports.is_empty());

        // Send a report
        worker_transport
            .send(addr(2), &ReportingMessage::Report(report("w1")))
            .await
            .unwrap();

        // Watch should update
        tokio::time::timeout(Duration::from_millis(100), watch_rx.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(watch_rx.borrow().reports.len(), 1);

        shutdown.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn marks_stale_reports() {
        let net = InMemoryReportingNetwork::new();
        let council_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();

        let (mut aggregator, _watch_rx) =
            ReportAggregator::new(council_transport, test_config(), shutdown.clone());

        // Insert a report with a timestamp 60s in the past
        aggregator.insert_local_report(stale_report("old-node"));

        // Build state and check
        let state = aggregator.build_aggregated_state();
        assert!(state.stale_nodes.contains(&NodeId::new("old-node")));
    }

    #[tokio::test]
    async fn handles_multiple_workers() {
        let net = InMemoryReportingNetwork::new();
        let w1 = net.register(addr(1)).await;
        let w2 = net.register(addr(2)).await;
        let w3 = net.register(addr(3)).await;
        let w4 = net.register(addr(4)).await;
        let w5 = net.register(addr(5)).await;
        let council_transport = net.register(addr(10)).await;
        let shutdown = CancellationToken::new();

        let (mut aggregator, mut watch_rx) =
            ReportAggregator::new(council_transport, test_config(), shutdown.clone());

        // Send reports from 5 workers
        for (i, w) in [&w1, &w2, &w3, &w4, &w5].iter().enumerate() {
            w.send(
                addr(10),
                &ReportingMessage::Report(report(&format!("w{}", i + 1))),
            )
            .await
            .unwrap();
        }

        let handle = tokio::spawn(async move { aggregator.run().await });

        // Wait until all 5 reports have been received. The watch channel
        // coalesces updates, so we can't count individual `changed()` calls.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            tokio::time::timeout_at(deadline, watch_rx.changed())
                .await
                .expect("timed out waiting for 5 reports")
                .unwrap();
            if watch_rx.borrow().reports.len() == 5 {
                break;
            }
        }

        assert_eq!(watch_rx.borrow().reports.len(), 5);

        shutdown.cancel();
        let _ = handle.await;
    }
}

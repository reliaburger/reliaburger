/// Council-side report aggregator.
///
/// Runs on each council member as a spawned task. Receives `StateReport`
/// messages from assigned workers, stores the latest per-node, and
/// publishes the aggregated view via a `tokio::sync::watch` channel.
///
/// Honesty rules (Phase 12b.2, CP5): freshness is judged on aggregator-side
/// receive time, never on the sender's wall clock — a future-dated report
/// must not stay "fresh" forever, and clock skew must not mark a live node
/// stale. Every entry is also tagged with the leadership epoch (the Raft
/// term) it arrived under; on failover the new epoch starts empty, so
/// pre-failover reports can't satisfy reconstruction coverage or feed the
/// scheduler. Entries are evicted when gossip declares their node gone or
/// when they age past `EVICTION_MULTIPLIER` stale windows.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{RwLock, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::node::ReportingTreeSection;
use crate::mayo::rollup_store::RollupStore;
use crate::meat::NodeId;
use crate::mustard::membership::MembershipSnapshot;

use super::transport::ReportingTransport;
use super::types::{DnsCapabilityReport, NodeCapabilityReport, ReportingMessage, StateReport};

/// How many stale windows an entry may age before it is evicted outright
/// (as opposed to merely listed in `stale_nodes`).
const EVICTION_MULTIPLIER: u32 = 3;

/// The aggregated view of all worker reports.
///
/// Published via a `watch` channel so any number of consumers
/// (leader, API handlers, etc.) can read the latest state without
/// buffering intermediate values. Contains only reports received under
/// the current leadership epoch.
#[derive(Debug, Clone, Default)]
pub struct AggregatedState {
    /// Latest report from each worker node.
    pub reports: HashMap<NodeId, StateReport>,
    /// Nodes whose last report was *received* longer ago than
    /// `stale_report_timeout` (receive time, not sender wall clock).
    pub stale_nodes: Vec<NodeId>,
    /// Latest additive capability evidence from new-enough nodes. Absence
    /// means no schedulable security capabilities, never an optimistic guess.
    pub capabilities: HashMap<NodeId, NodeCapabilityReport>,
}

/// A stored report plus the aggregator-side metadata that decides its fate.
struct ReportEntry {
    report: StateReport,
    /// When the aggregator received it (monotonic, aggregator-side).
    received_at: Instant,
    /// The leadership epoch (Raft term) it was received under.
    epoch: u64,
}

/// Capability evidence uses the same receive-time and leadership-epoch rules
/// as state reports, but travels in a separate backwards-compatible message.
struct CapabilityEntry {
    report: NodeCapabilityReport,
    received_at: Instant,
    epoch: u64,
}

/// DNS readiness travels in an additive H3 frame and therefore needs its own
/// lease. A DNS heartbeat must not keep stale H2 egress evidence alive (or the
/// reverse) during a partial reporting failure.
struct DnsCapabilityEntry {
    report: DnsCapabilityReport,
    received_at: Instant,
    epoch: u64,
}

/// Aggregates state reports from assigned worker nodes.
pub struct ReportAggregator<T: ReportingTransport> {
    transport: T,
    entries: HashMap<NodeId, ReportEntry>,
    capability_entries: HashMap<NodeId, CapabilityEntry>,
    dns_capability_entries: HashMap<NodeId, DnsCapabilityEntry>,
    watch_tx: watch::Sender<AggregatedState>,
    rollup_store: Option<Arc<RwLock<RollupStore>>>,
    config: ReportingTreeSection,
    shutdown: CancellationToken,
    /// Current leadership epoch (the Raft term the leader-target
    /// maintainer resolved). `None` keeps everything in epoch 0 —
    /// standalone and unit-test mode.
    epoch_rx: Option<watch::Receiver<u64>>,
    /// Gossip membership, for evicting reports of departed nodes.
    /// `None` disables eviction-by-membership.
    membership_rx: Option<watch::Receiver<Vec<MembershipSnapshot>>>,
}

impl<T: ReportingTransport> ReportAggregator<T> {
    /// Create a new aggregator.
    ///
    /// Returns the aggregator and a watch receiver for consumers.
    pub fn new(
        transport: T,
        config: ReportingTreeSection,
        shutdown: CancellationToken,
        rollup_store: Option<Arc<RwLock<RollupStore>>>,
        epoch_rx: Option<watch::Receiver<u64>>,
        membership_rx: Option<watch::Receiver<Vec<MembershipSnapshot>>>,
    ) -> (Self, watch::Receiver<AggregatedState>) {
        let (watch_tx, watch_rx) = watch::channel(AggregatedState::default());
        let aggregator = Self {
            transport,
            entries: HashMap::new(),
            capability_entries: HashMap::new(),
            dns_capability_entries: HashMap::new(),
            watch_tx,
            rollup_store,
            config,
            shutdown,
            epoch_rx,
            membership_rx,
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
                            self.store_report(report);
                            self.publish();
                        }
                        Some((_, ReportingMessage::AggregatedReport { reports })) => {
                            // Merge reports from another council member
                            // (used for leader aggregation).
                            for (_, report) in reports {
                                self.store_report(report);
                            }
                            self.publish();
                        }
                        Some((_, ReportingMessage::Ack { .. })) => {
                            // Workers don't send Acks to the aggregator
                        }
                        Some((_, ReportingMessage::MetricsRollup(rollup))) => {
                            if let Some(ref store) = self.rollup_store {
                                store.write().await.ingest(&rollup);
                            }
                        }
                        Some((_, ReportingMessage::CapabilityReport(report))) => {
                            self.store_capability(report);
                            self.publish();
                        }
                        Some((_, ReportingMessage::DnsCapabilityReport(report))) => {
                            self.store_dns_capability(report);
                            self.publish();
                        }
                        None => {
                            // The transport only closes at shutdown; anything
                            // else is a bug worth a loud log, not a silent
                            // stop (CP10).
                            if !self.shutdown.is_cancelled() {
                                eprintln!("report aggregator: transport closed unexpectedly");
                            }
                            break;
                        }
                    }
                }
                _ = stale_check.tick() => {
                    self.evict_expired(stale_timeout);
                    self.publish();
                }
                changed = Self::watch_changed(&mut self.epoch_rx) => {
                    if changed.is_err() {
                        self.epoch_rx = None;
                        continue;
                    }
                    // New leadership epoch: prior-epoch entries no longer
                    // count. Republish so consumers see coverage drop now,
                    // not at the next report.
                    self.publish();
                }
                changed = Self::watch_changed(&mut self.membership_rx) => {
                    if changed.is_err() {
                        self.membership_rx = None;
                        continue;
                    }
                    if self.evict_departed() {
                        self.publish();
                    }
                }
            }
        }
    }

    /// Await a change on an optional watch channel; pend forever when it
    /// is `None` so the `select!` arm simply never fires.
    async fn watch_changed<U: Clone>(
        rx: &mut Option<watch::Receiver<U>>,
    ) -> Result<(), watch::error::RecvError> {
        match rx {
            Some(rx) => rx.changed().await,
            None => std::future::pending().await,
        }
    }

    /// The leadership epoch entries are currently tagged with.
    fn current_epoch(&self) -> u64 {
        self.epoch_rx.as_ref().map(|rx| *rx.borrow()).unwrap_or(0)
    }

    fn store_report(&mut self, report: StateReport) {
        self.entries.insert(
            report.node_id.clone(),
            ReportEntry {
                report,
                received_at: Instant::now(),
                epoch: self.current_epoch(),
            },
        );
    }

    fn store_capability(&mut self, report: NodeCapabilityReport) {
        self.capability_entries.insert(
            report.node_id.clone(),
            CapabilityEntry {
                report,
                received_at: Instant::now(),
                epoch: self.current_epoch(),
            },
        );
    }

    fn store_dns_capability(&mut self, report: DnsCapabilityReport) {
        self.dns_capability_entries.insert(
            report.node_id.clone(),
            DnsCapabilityEntry {
                report,
                received_at: Instant::now(),
                epoch: self.current_epoch(),
            },
        );
    }

    /// Remove entries whose receive age exceeds the eviction bound.
    fn evict_expired(&mut self, stale_timeout: Duration) {
        let bound = stale_timeout * EVICTION_MULTIPLIER;
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.received_at) <= bound);
        self.capability_entries
            .retain(|_, entry| now.duration_since(entry.received_at) <= bound);
        self.dns_capability_entries
            .retain(|_, entry| now.duration_since(entry.received_at) <= bound);
    }

    /// Remove entries for nodes gossip no longer reports as active
    /// (Dead/Left members drop out of the snapshot). Returns `true` if
    /// anything was evicted. An empty snapshot is ignored: gossip hasn't
    /// published yet, and evicting everything on startup would be wrong.
    fn evict_departed(&mut self) -> bool {
        let Some(rx) = &self.membership_rx else {
            return false;
        };
        let members = rx.borrow();
        if members.is_empty() {
            return false;
        }
        let active: std::collections::HashSet<&NodeId> =
            members.iter().map(|m| &m.node_id).collect();
        let before = self.entries.len();
        self.entries.retain(|node_id, _| active.contains(node_id));
        let capability_before = self.capability_entries.len();
        self.capability_entries
            .retain(|node_id, _| active.contains(node_id));
        let dns_before = self.dns_capability_entries.len();
        self.dns_capability_entries
            .retain(|node_id, _| active.contains(node_id));
        self.entries.len() != before
            || self.capability_entries.len() != capability_before
            || self.dns_capability_entries.len() != dns_before
    }

    fn publish(&self) {
        let _ = self.watch_tx.send(self.build_aggregated_state());
    }

    /// Snapshot the current state for the watch channel: current-epoch
    /// entries only, staleness judged on receive time.
    fn build_aggregated_state(&self) -> AggregatedState {
        let stale_timeout = Duration::from_secs(self.config.stale_report_timeout_secs);
        let now = Instant::now();
        let epoch = self.current_epoch();

        let mut reports = HashMap::new();
        let mut stale_nodes = Vec::new();
        for (node_id, entry) in &self.entries {
            if entry.epoch != epoch {
                continue; // a previous leadership epoch — not current truth
            }
            reports.insert(node_id.clone(), entry.report.clone());
            if now.duration_since(entry.received_at) > stale_timeout {
                stale_nodes.push(node_id.clone());
            }
        }

        let mut capabilities: HashMap<NodeId, NodeCapabilityReport> = self
            .capability_entries
            .iter()
            .filter(|(_, entry)| {
                entry.epoch == epoch && now.duration_since(entry.received_at) <= stale_timeout
            })
            .map(|(node_id, entry)| (node_id.clone(), entry.report.clone()))
            .collect();
        for (node_id, entry) in &self.dns_capability_entries {
            if entry.epoch != epoch || now.duration_since(entry.received_at) > stale_timeout {
                continue;
            }
            capabilities
                .entry(node_id.clone())
                .or_insert_with(|| NodeCapabilityReport {
                    node_id: node_id.clone(),
                    capabilities: Default::default(),
                    egress_enforcement: Vec::new(),
                    egress_degraded: false,
                    egress_affected_workloads: Vec::new(),
                })
                .capabilities
                .dns = entry.report.capability;
        }

        AggregatedState {
            reports,
            stale_nodes,
            capabilities,
        }
    }

    /// Directly insert a report (for the council member's own state).
    pub fn insert_local_report(&mut self, report: StateReport) {
        self.store_report(report);
        self.publish();
    }

    /// Number of reports currently held (all epochs).
    pub fn report_count(&self) -> usize {
        self.entries.len()
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
            has_buildah: false,
            node_id: NodeId::new(name),
            timestamp: SystemTime::now(),
            running_apps: vec![],
            cached_specs: vec![],
            resource_usage: ResourceUsage::default(),
            event_log: vec![],
        }
    }

    fn capability(name: &str) -> NodeCapabilityReport {
        NodeCapabilityReport {
            node_id: NodeId::new(name),
            capabilities: crate::meat::cluster_state::NodeCapabilities {
                egress: crate::sesame::egress::EgressEnforcementCapability {
                    connect_ipv4: true,
                    connect_ipv6: true,
                    udp_ipv4: true,
                    udp_ipv6: true,
                    pre_start: true,
                },
                dns: Default::default(),
            },
            egress_enforcement: Vec::new(),
            egress_degraded: false,
            egress_affected_workloads: Vec::new(),
        }
    }

    fn test_config() -> ReportingTreeSection {
        ReportingTreeSection {
            report_interval_secs: 1,
            max_events_per_report: 100,
            stale_report_timeout_secs: 30,
        }
    }

    fn member(name: &str) -> MembershipSnapshot {
        MembershipSnapshot {
            node_id: NodeId::new(name),
            address: addr(9000),
            state: crate::mustard::state::NodeState::Alive,
            incarnation: 1,
            is_council: false,
            is_leader: false,
            labels: std::collections::BTreeMap::new(),
            first_seen: std::time::Instant::now(),
            resources: None,
        }
    }

    #[tokio::test]
    async fn stores_latest_report_per_node() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let council_transport = net.register(addr(2)).await;
        let shutdown = CancellationToken::new();

        let (mut aggregator, mut watch_rx) = ReportAggregator::new(
            council_transport,
            test_config(),
            shutdown.clone(),
            None,
            None,
            None,
        );

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

        let (mut aggregator, mut watch_rx) = ReportAggregator::new(
            council_transport,
            test_config(),
            shutdown.clone(),
            None,
            None,
            None,
        );

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

    #[tokio::test(start_paused = true)]
    async fn marks_stale_by_receive_time() {
        let net = InMemoryReportingNetwork::new();
        let council_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();

        let (mut aggregator, _watch_rx) = ReportAggregator::new(
            council_transport,
            test_config(),
            shutdown.clone(),
            None,
            None,
            None,
        );

        aggregator.insert_local_report(report("old-node"));

        // Not stale immediately after receipt.
        assert!(aggregator.build_aggregated_state().stale_nodes.is_empty());

        // 31 seconds of receive-side silence (timeout is 30s) makes it stale.
        tokio::time::advance(Duration::from_secs(31)).await;
        let state = aggregator.build_aggregated_state();
        assert!(state.stale_nodes.contains(&NodeId::new("old-node")));
    }

    #[tokio::test(start_paused = true)]
    async fn stale_capability_is_withdrawn_fail_closed() {
        let net = InMemoryReportingNetwork::new();
        let council_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();

        let (mut aggregator, _watch_rx) =
            ReportAggregator::new(council_transport, test_config(), shutdown, None, None, None);
        aggregator.store_capability(capability("guarded"));
        assert!(
            aggregator
                .build_aggregated_state()
                .capabilities
                .contains_key(&NodeId::new("guarded"))
        );

        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(
            aggregator.build_aggregated_state().capabilities.is_empty(),
            "a stale hook report must never keep admitting protected workloads"
        );
    }

    #[tokio::test]
    async fn dns_extension_merges_without_changing_h2_capability_frame() {
        let net = InMemoryReportingNetwork::new();
        let council_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();
        let (mut aggregator, _watch_rx) =
            ReportAggregator::new(council_transport, test_config(), shutdown, None, None, None);

        aggregator.store_capability(capability("node-1"));
        aggregator.store_dns_capability(DnsCapabilityReport {
            node_id: NodeId::new("node-1"),
            capability: crate::onion::dns::DnsCapability {
                enabled: true,
                ready: true,
                ipv4: true,
                ipv6: false,
                workload_reachable: true,
            },
        });
        // The next periodic H2-shaped frame must merge with the independent
        // DNS extension rather than replacing it with the skipped-field default.
        aggregator.store_capability(capability("node-1"));

        let state = aggregator.build_aggregated_state();
        let report = state.capabilities.get(&NodeId::new("node-1")).unwrap();
        assert!(report.capabilities.egress.can_enforce_allowlist());
        assert!(report.capabilities.dns.can_resolve_internal());
    }

    #[tokio::test(start_paused = true)]
    async fn dns_heartbeat_does_not_keep_egress_capability_fresh() {
        let net = InMemoryReportingNetwork::new();
        let council_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();
        let (mut aggregator, _watch_rx) =
            ReportAggregator::new(council_transport, test_config(), shutdown, None, None, None);

        aggregator.store_capability(capability("node-1"));
        tokio::time::advance(Duration::from_secs(20)).await;
        aggregator.store_dns_capability(DnsCapabilityReport {
            node_id: NodeId::new("node-1"),
            capability: crate::onion::dns::DnsCapability {
                enabled: true,
                ready: true,
                ipv4: true,
                ipv6: false,
                workload_reachable: true,
            },
        });
        tokio::time::advance(Duration::from_secs(11)).await;

        let report = aggregator
            .build_aggregated_state()
            .capabilities
            .remove(&NodeId::new("node-1"))
            .unwrap();
        assert_eq!(report.capabilities.egress, Default::default());
        assert!(report.capabilities.dns.can_resolve_internal());
    }

    #[tokio::test]
    async fn epoch_bump_invalidates_dns_extension_independently() {
        let net = InMemoryReportingNetwork::new();
        let council_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();
        let (epoch_tx, epoch_rx) = watch::channel(3u64);
        let (mut aggregator, _watch_rx) = ReportAggregator::new(
            council_transport,
            test_config(),
            shutdown,
            None,
            Some(epoch_rx),
            None,
        );

        aggregator.store_dns_capability(DnsCapabilityReport {
            node_id: NodeId::new("node-1"),
            capability: crate::onion::dns::DnsCapability {
                enabled: true,
                ready: true,
                ipv4: true,
                ipv6: false,
                workload_reachable: true,
            },
        });
        assert!(
            aggregator
                .build_aggregated_state()
                .capabilities
                .contains_key(&NodeId::new("node-1"))
        );

        epoch_tx.send(4).unwrap();
        assert!(aggregator.build_aggregated_state().capabilities.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn future_dated_report_does_not_outlive_its_receive_window() {
        // CP5: a sender with a wall clock a year ahead used to stay
        // "fresh" forever. Freshness must come from receive time.
        let net = InMemoryReportingNetwork::new();
        let council_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();

        let (mut aggregator, _watch_rx) = ReportAggregator::new(
            council_transport,
            test_config(),
            shutdown.clone(),
            None,
            None,
            None,
        );

        let mut future = report("skewed");
        future.timestamp = SystemTime::now() + Duration::from_secs(365 * 24 * 3600);
        aggregator.insert_local_report(future);

        tokio::time::advance(Duration::from_secs(31)).await;
        let state = aggregator.build_aggregated_state();
        assert!(
            state.stale_nodes.contains(&NodeId::new("skewed")),
            "a future-dated report must go stale on receive time"
        );

        // And past the eviction bound (3 stale windows) it disappears.
        tokio::time::advance(Duration::from_secs(60)).await;
        aggregator.evict_expired(Duration::from_secs(30));
        assert_eq!(aggregator.report_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn past_dated_report_is_fresh_when_just_received() {
        // The mirror case: a sender with a slow clock must not be
        // instantly stale.
        let net = InMemoryReportingNetwork::new();
        let council_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();

        let (mut aggregator, _watch_rx) = ReportAggregator::new(
            council_transport,
            test_config(),
            shutdown.clone(),
            None,
            None,
            None,
        );

        let mut old = report("slow-clock");
        old.timestamp = SystemTime::now() - Duration::from_secs(3600);
        aggregator.insert_local_report(old);
        assert!(aggregator.build_aggregated_state().stale_nodes.is_empty());
    }

    #[tokio::test]
    async fn epoch_bump_invalidates_previous_reports() {
        // CP5: a new leader's aggregator must not present pre-failover
        // reports as current-epoch truth.
        let net = InMemoryReportingNetwork::new();
        let council_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();
        let (epoch_tx, epoch_rx) = watch::channel(3u64);

        let (mut aggregator, _watch_rx) = ReportAggregator::new(
            council_transport,
            test_config(),
            shutdown.clone(),
            None,
            Some(epoch_rx),
            None,
        );

        aggregator.insert_local_report(report("w1"));
        assert_eq!(aggregator.build_aggregated_state().reports.len(), 1);

        // Leadership change: term 3 → 4. The old report no longer counts.
        epoch_tx.send(4).unwrap();
        assert!(aggregator.build_aggregated_state().reports.is_empty());

        // A fresh report under the new epoch counts again.
        aggregator.insert_local_report(report("w1"));
        assert_eq!(aggregator.build_aggregated_state().reports.len(), 1);
    }

    #[tokio::test]
    async fn evicts_reports_for_departed_members() {
        let net = InMemoryReportingNetwork::new();
        let council_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();
        let (membership_tx, membership_rx) = watch::channel(vec![member("w1"), member("w2")]);

        let (mut aggregator, mut watch_rx) = ReportAggregator::new(
            council_transport,
            test_config(),
            shutdown.clone(),
            None,
            None,
            Some(membership_rx),
        );
        aggregator.insert_local_report(report("w1"));
        aggregator.insert_local_report(report("w2"));

        let handle = tokio::spawn(async move { aggregator.run().await });

        // w2 dies: gossip drops it from the active snapshot.
        membership_tx.send(vec![member("w1")]).unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            tokio::time::timeout_at(deadline, watch_rx.changed())
                .await
                .expect("timed out waiting for eviction")
                .unwrap();
            if !watch_rx.borrow().reports.contains_key(&NodeId::new("w2")) {
                break;
            }
        }
        assert!(watch_rx.borrow().reports.contains_key(&NodeId::new("w1")));

        shutdown.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn empty_membership_snapshot_evicts_nothing() {
        let net = InMemoryReportingNetwork::new();
        let council_transport = net.register(addr(1)).await;
        let shutdown = CancellationToken::new();
        let (_membership_tx, membership_rx) = watch::channel(Vec::new());

        let (mut aggregator, _watch_rx) = ReportAggregator::new(
            council_transport,
            test_config(),
            shutdown.clone(),
            None,
            None,
            Some(membership_rx),
        );
        aggregator.insert_local_report(report("w1"));
        assert!(!aggregator.evict_departed());
        assert_eq!(aggregator.report_count(), 1);
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

        let (mut aggregator, mut watch_rx) = ReportAggregator::new(
            council_transport,
            test_config(),
            shutdown.clone(),
            None,
            None,
            None,
        );

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

//! Worker-side rollup push task.
//!
//! Runs on each node as a spawned task. Every 60 seconds, generates a
//! `NodeRollup` from the local `MayoStore` and sends it to the assigned
//! council aggregator via the reporting transport.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{RwLock, watch};
use tokio_util::sync::CancellationToken;

use crate::meat::NodeId;
use crate::reporting::assignment::assign_parent_address;
use crate::reporting::transport::ReportingTransport;
use crate::reporting::types::ReportingMessage;

use super::rollup::NodeRollup;
use super::rollup_generator::RollupGenerator;
use super::store::MayoStore;
use super::types::MetricKey;
use crate::reporting::transport::MAX_REPORT_SIZE;

/// Periodically generates rollups from the local MayoStore and pushes
/// them to the assigned council aggregator.
pub struct RollupWorker<T: ReportingTransport> {
    node_id: NodeId,
    generator: RollupGenerator,
    transport: T,
    mayo: Arc<RwLock<MayoStore>>,
    /// Current parent council member address.
    parent_address: Option<SocketAddr>,
    /// Receives council membership updates.
    council_rx: watch::Receiver<Vec<(NodeId, SocketAddr)>>,
    /// Rollup push interval.
    interval: Duration,
    /// Whether the next rollup should be extended (5 min backfill).
    send_extended: bool,
    /// Rollup parts refused as too large since the worker started (#2426).
    refused: u64,
    shutdown: CancellationToken,
}

impl<T: ReportingTransport> RollupWorker<T> {
    /// Create a new rollup worker.
    pub fn new(
        node_id: NodeId,
        transport: T,
        mayo: Arc<RwLock<MayoStore>>,
        council_rx: watch::Receiver<Vec<(NodeId, SocketAddr)>>,
        interval: Duration,
        shutdown: CancellationToken,
    ) -> Self {
        let parent_address = assign_parent_address(&node_id, &council_rx.borrow());
        let generator = RollupGenerator::new(node_id.clone());
        Self {
            node_id,
            generator,
            transport,
            mayo,
            parent_address,
            council_rx,
            interval,
            send_extended: false,
            refused: 0,
            shutdown,
        }
    }

    /// Run the worker event loop until shutdown.
    pub async fn run(&mut self) {
        let mut interval = tokio::time::interval(self.interval);
        // Skip the first tick (fires immediately)
        interval.tick().await;

        // A closed watch resolves instantly forever (CP10); the guard drops
        // that select arm so the worker keeps pushing to the last known
        // parent instead of hot-spinning or dying with the maintainer.
        let mut watch_open = true;

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = interval.tick() => {
                    self.push_rollup().await;
                }
                result = self.council_rx.changed(), if watch_open => {
                    match result {
                        Ok(()) => self.update_parent(),
                        Err(_) => {
                            watch_open = false;
                            if !self.shutdown.is_cancelled() {
                                eprintln!(
                                    "rollup worker: leader-target channel closed; \
                                     pushing to the last known parent"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Recompute the parent assignment from the current council membership.
    fn update_parent(&mut self) {
        let council = self.council_rx.borrow().clone();
        let new_parent = assign_parent_address(&self.node_id, &council);

        // If parent changed, mark the next rollup as extended (5 min backfill)
        if new_parent != self.parent_address {
            self.send_extended = true;
        }
        self.parent_address = new_parent;
    }

    /// Generate and send a rollup to the parent council member.
    async fn push_rollup(&mut self) {
        let parent = match self.parent_address {
            Some(addr) => addr,
            None => return,
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let extended = self.send_extended;
        // One statement, so the read guard is dropped before the queries run:
        // holding it stalled the collector's write and every read queued
        // behind it (#2449). One snapshot serves all five backfill windows.
        let store = match self.mayo.read().await.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                self.send_extended = true;
                eprintln!(
                    "mayo: rollup generation failed; requesting the last five minutes next tick: {e}"
                );
                return;
            }
        };
        // The backfill is per-minute rollups, not one 5-minute blob: a blob's
        // single timestamp collided with an already-sent minute key, so the
        // parent's dedup dropped or double-counted the whole thing (M9). The
        // normal tick's minute is the backfill's newest sub-window, so the
        // extended path replaces the ordinary rollup rather than adding to it.
        let generated = if extended {
            self.generator.generate_backfill(&store, now).await
        } else {
            self.generator
                .generate(&store, now)
                .await
                .map(|rollup| vec![rollup])
        };

        let rollups = match generated {
            Ok(r) => r,
            Err(e) => {
                // A failed generation used to vanish silently; the stateless
                // generator only rolls up the previous minute, so a swallowed
                // error is a permanent gap. Log it and retry next tick (M9).
                self.send_extended = true;
                eprintln!(
                    "mayo: rollup generation failed; requesting the last five minutes next tick: {e}"
                );
                return;
            }
        };

        let mut all_sent = true;
        'minutes: for rollup in rollups {
            for part in split_to_fit(rollup) {
                match self
                    .transport
                    .send(parent, &ReportingMessage::MetricsRollup(part))
                    .await
                {
                    Ok(()) => {}
                    // One entry alone is over the frame limit. A backfill
                    // can't send it either, so it's counted, not retried
                    // (#2426); the minute's other parts still go.
                    Err(e @ crate::reporting::ReportingError::ReportTooLarge { .. }) => {
                        self.refused += 1;
                        let refused = self.refused as f64;
                        self.mayo
                            .write()
                            .await
                            .insert_now(&MetricKey::simple(ROLLUPS_REFUSED_METRIC), refused);
                        eprintln!("mayo: a rollup part is too large to send and was dropped: {e}");
                    }
                    Err(e) => {
                        eprintln!(
                            "mayo: rollup admission failed; requesting the last five minutes next tick (older gaps remain in local metrics only): {e}"
                        );
                        all_sent = false;
                        break 'minutes;
                    }
                }
            }
        }

        // Only drop the backfill request once every minute has actually been
        // sent (M9). A partial success re-sends some minutes next tick, which
        // the parent's `(node, timestamp, part)` dedup makes idempotent.
        self.send_extended = !all_sent;
    }
}

/// The node metric counting rollup parts the transport refused as too
/// large (#2426).
pub const ROLLUPS_REFUSED_METRIC: &str = "mayo_rollups_refused_total";

/// Split `rollup` into parts that each fit one report frame (#2426),
/// numbered from 0, in entry order. The split depends only on the entries,
/// so a backfill re-sending the same minute numbers its parts the same way
/// and the parent drops the repeats. An entry too large for any frame goes
/// alone in its part, which the transport then refuses.
pub(crate) fn split_to_fit(rollup: NodeRollup) -> Vec<NodeRollup> {
    let NodeRollup {
        node_id,
        timestamp,
        entries,
        ..
    } = rollup;
    let empty = NodeRollup {
        node_id,
        timestamp,
        part: 0,
        entries: Vec::new(),
    };
    // What a part costs before its entries, with room for the entry
    // count's varint to grow and for the part number's.
    let overhead =
        postcard::experimental::serialized_size(&ReportingMessage::MetricsRollup(empty.clone()))
            .unwrap_or(0)
            + REPORT_HEADER_SLACK;
    let budget = MAX_REPORT_SIZE.saturating_sub(overhead);

    let mut parts = Vec::new();
    let mut current = Vec::new();
    let mut used = 0;
    for entry in entries {
        // An entry that can't be measured is sent and left to the
        // transport to judge.
        let size = postcard::experimental::serialized_size(&entry).unwrap_or(0);
        if !current.is_empty() && used + size > budget {
            parts.push(std::mem::take(&mut current));
            used = 0;
        }
        used += size;
        current.push(entry);
    }
    if !current.is_empty() || parts.is_empty() {
        parts.push(current);
    }
    parts
        .into_iter()
        .enumerate()
        .map(|(index, entries)| NodeRollup {
            part: u16::try_from(index).unwrap_or(u16::MAX),
            entries,
            ..empty.clone()
        })
        .collect()
}

/// Room for the frame header and the varints a part's size estimate leaves
/// out (the entry count, the part number).
const REPORT_HEADER_SLACK: usize = 64;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mayo::types::Sample;
    use crate::reporting::transport::InMemoryReportingNetwork;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[tokio::test]
    async fn sends_rollup_at_interval() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let council_transport = net.register(addr(2)).await;
        let shutdown = CancellationToken::new();

        let dir = tempfile::tempdir().unwrap();
        let store = MayoStore::new(dir.path().to_path_buf());
        {
            let mayo = Arc::new(RwLock::new(store));

            // Insert a metric so the rollup has data
            {
                let mut s = mayo.write().await;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                s.insert(&MetricKey::simple("cpu"), Sample::at(now - 30, 42.0));
                s.flush().await.unwrap();
            }

            let council = vec![(NodeId::new("c1"), addr(2))];
            let (_council_tx, council_rx) = watch::channel(council);

            let mut worker = RollupWorker::new(
                NodeId::new("w1"),
                worker_transport,
                mayo,
                council_rx,
                Duration::from_millis(100), // fast interval for testing
                shutdown.clone(),
            );

            let handle = tokio::spawn(async move { worker.run().await });

            // Council should receive a MetricsRollup within 1 second
            let result =
                tokio::time::timeout(Duration::from_secs(2), council_transport.recv()).await;
            assert!(result.is_ok(), "should receive a rollup message");

            let (from, _, msg) = result.unwrap().unwrap();
            assert_eq!(from, addr(1));
            match msg {
                ReportingMessage::MetricsRollup(r) => {
                    assert_eq!(r.node_id, NodeId::new("w1"));
                }
                _ => panic!("expected MetricsRollup"),
            }

            shutdown.cancel();
            let _ = handle.await;
        }
    }

    /// After reassignment the backfill goes out as per-minute rollups stamped
    /// exactly like ordinary ticks — the single 5-minute blob used to collide
    /// with an already-sent minute key and get dropped or double-counted (M9).
    /// Driven directly (no spawned task) so there is no timing to race.
    #[tokio::test]
    async fn backfill_sends_per_minute_rollups_and_clears_the_flag() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let council_transport = net.register(addr(2)).await;

        let dir = tempfile::tempdir().unwrap();
        let mayo = Arc::new(RwLock::new(MayoStore::new(dir.path().to_path_buf())));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        {
            // Three samples 60 s apart land in three distinct completed
            // minutes, all inside the 5-minute backfill span.
            let mut s = mayo.write().await;
            for offset in [90, 150, 210] {
                s.insert(&MetricKey::simple("cpu"), Sample::at(now - offset, 1.0));
            }
            s.flush().await.unwrap();
        }

        let council = vec![(NodeId::new("c1"), addr(2))];
        let (_council_tx, council_rx) = watch::channel(council);
        let mut worker = RollupWorker::new(
            NodeId::new("w1"),
            worker_transport,
            mayo,
            council_rx,
            Duration::from_millis(100),
            CancellationToken::new(),
        );
        worker.send_extended = true;

        worker.push_rollup().await;
        assert!(!worker.send_extended, "flag clears after a full send");

        let mut stamps = Vec::new();
        while let Some((_, _, msg)) = council_transport.try_recv() {
            match msg {
                ReportingMessage::MetricsRollup(r) => stamps.push(r.timestamp),
                _ => panic!("expected MetricsRollup"),
            }
        }
        assert_eq!(stamps.len(), 3, "one rollup per non-empty minute");
        for stamp in &stamps {
            assert_eq!(stamp % 60, 0, "backfill stamps are minute-aligned");
        }
        assert!(
            stamps.windows(2).all(|pair| pair[0] < pair[1]),
            "stamps are distinct and oldest-first: {stamps:?}"
        );
    }

    /// A failed backfill send must keep the flag set so the next tick retries;
    /// the parent's `(node, timestamp)` dedup makes any re-sent minutes
    /// idempotent.
    #[tokio::test]
    async fn backfill_flag_survives_a_failed_send() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        // addr(9) is never registered, so every send to it fails.

        let dir = tempfile::tempdir().unwrap();
        let mayo = Arc::new(RwLock::new(MayoStore::new(dir.path().to_path_buf())));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        {
            let mut s = mayo.write().await;
            s.insert(&MetricKey::simple("cpu"), Sample::at(now - 90, 1.0));
            s.flush().await.unwrap();
        }

        let council = vec![(NodeId::new("c1"), addr(9))];
        let (_council_tx, council_rx) = watch::channel(council);
        let mut worker = RollupWorker::new(
            NodeId::new("w1"),
            worker_transport,
            mayo,
            council_rx,
            Duration::from_millis(100),
            CancellationToken::new(),
        );
        for extended in [false, true] {
            worker.send_extended = extended;
            worker.push_rollup().await;
            assert!(
                worker.send_extended,
                "normal and backfill failures request recovery"
            );
        }
    }

    /// #2449: the worker held the store's read guard through its queries,
    /// and tokio's `RwLock` is fair, so the collector's next write (and every
    /// read behind it) waited for the slowest rollup. A write now goes
    /// through while a rollup's query runs.
    #[tokio::test]
    async fn a_metrics_write_is_not_blocked_while_a_rollup_is_generated() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let _council_transport = net.register(addr(2)).await;

        let dir = tempfile::tempdir().unwrap();
        let mut store = MayoStore::new(dir.path().to_path_buf());
        let gate = Arc::new(crate::mayo::store::QueryGate::default());
        store.set_query_gate(Arc::clone(&gate));
        let mayo = Arc::new(RwLock::new(store));

        let council = vec![(NodeId::new("c1"), addr(2))];
        let (_council_tx, council_rx) = watch::channel(council);
        let mut worker = RollupWorker::new(
            NodeId::new("w1"),
            worker_transport,
            Arc::clone(&mayo),
            council_rx,
            Duration::from_millis(100),
            CancellationToken::new(),
        );
        let generating = tokio::spawn(async move { worker.push_rollup().await });

        gate.entered.notified().await;
        let write = tokio::time::timeout(Duration::from_millis(200), mayo.write()).await;
        assert!(
            write.is_ok(),
            "a write waited for the rollup's query to finish"
        );
        drop(write);
        gate.release.notify_one();
        generating.await.unwrap();
    }

    /// The start of the minute before the current one: what the next
    /// ordinary push rolls up.
    fn previous_minute() -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        now - now % 60 - 60
    }

    /// `count` series of one metric, labelled as an app scrape labels them.
    fn series(count: usize) -> Vec<MetricKey> {
        (0..count)
            .map(|i| {
                MetricKey::with_labels(
                    "requests_total",
                    std::collections::BTreeMap::from([
                        ("app".to_string(), "default/web".to_string()),
                        ("namespace".to_string(), "default".to_string()),
                        ("instance".to_string(), format!("default__web-{}", i % 30)),
                        ("node".to_string(), "rb-4f2a9c1e07b3-2".to_string()),
                        ("path".to_string(), format!("/api/v1/resource/{i}")),
                    ]),
                )
            })
            .collect()
    }

    /// #2426: a node's minute of every series went out as one message, and
    /// the transport refuses a frame over 1 MiB, so a node with enough
    /// series silently stopped reporting cluster metrics for good. A large
    /// rollup now goes out in parts that each fit.
    #[tokio::test]
    async fn a_rollup_larger_than_one_report_frame_still_reaches_the_parent() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let council_transport = net.register(addr(2)).await;
        let dir = tempfile::tempdir().unwrap();
        let mut store = MayoStore::new(dir.path().to_path_buf());
        let minute = previous_minute();
        let keys = series(8_000);
        for key in &keys {
            store.insert(key, Sample::at(minute + 30, 1.0));
        }

        // How big one entry is on the wire, for the record (#2426).
        let one_thousand = crate::mayo::rollup::NodeRollup {
            node_id: NodeId::new("rb-4f2a9c1e07b3-2"),
            timestamp: minute,
            part: 0,
            entries: RollupGenerator::new(NodeId::new("rb-4f2a9c1e07b3-2"))
                .generate(&store.snapshot().await.unwrap(), minute + 90)
                .await
                .unwrap()
                .entries
                .into_iter()
                .take(1_000)
                .collect(),
        };
        let bytes =
            postcard::experimental::serialized_size(&ReportingMessage::MetricsRollup(one_thousand))
                .unwrap();
        eprintln!(
            "1,000 rollup entries: {bytes} bytes, {} per entry",
            bytes / 1_000
        );

        let council = vec![(NodeId::new("c1"), addr(2))];
        let (_council_tx, council_rx) = watch::channel(council);
        let mut worker = RollupWorker::new(
            NodeId::new("w1"),
            worker_transport,
            Arc::new(RwLock::new(store)),
            council_rx,
            Duration::from_millis(100),
            CancellationToken::new(),
        );
        worker.push_rollup().await;
        assert!(!worker.send_extended, "every part was sent");

        let mut received = std::collections::BTreeSet::new();
        let mut parts = std::collections::BTreeSet::new();
        while let Some((_, _, message)) = council_transport.try_recv() {
            let ReportingMessage::MetricsRollup(rollup) = message else {
                panic!("expected MetricsRollup");
            };
            assert_eq!(rollup.timestamp, minute);
            assert!(parts.insert(rollup.part), "part {} sent twice", rollup.part);
            for entry in rollup.entries {
                received.insert(entry.labels["path"].clone());
            }
        }
        assert!(parts.len() > 1, "8,000 series need more than one frame");
        assert_eq!(received.len(), 8_000);
    }

    /// #2426: every part fits a frame, and the split depends only on the
    /// entries, so a re-sent minute numbers its parts the same way.
    #[tokio::test]
    async fn a_split_is_deterministic_and_every_part_fits() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = MayoStore::new(dir.path().to_path_buf());
        let minute = previous_minute();
        for key in series(20_000) {
            store.insert(&key, Sample::at(minute + 30, 1.0));
        }
        let rollup = RollupGenerator::new(NodeId::new("w1"))
            .generate(&store.snapshot().await.unwrap(), minute + 90)
            .await
            .unwrap();
        let parts = split_to_fit(rollup.clone());
        assert!(parts.len() >= 2);
        for part in &parts {
            crate::reporting::transport::checked_frame_size(&ReportingMessage::MetricsRollup(
                part.clone(),
            ))
            .unwrap();
        }
        assert_eq!(parts, split_to_fit(rollup));
    }

    /// #2426: a rollup the transport refuses as too large can't be sent by
    /// a backfill either, so it doesn't start one; it is counted in a node
    /// metric instead of only a log line.
    #[tokio::test]
    async fn a_rollup_too_large_to_send_does_not_start_a_backfill() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let _council_transport = net.register(addr(2)).await;
        let dir = tempfile::tempdir().unwrap();
        let mayo = Arc::new(RwLock::new(MayoStore::new(dir.path().to_path_buf())));
        let minute = previous_minute();
        mayo.write().await.insert(
            &MetricKey::with_labels(
                "huge",
                std::collections::BTreeMap::from([("blob".to_string(), "x".repeat(2 << 20))]),
            ),
            Sample::at(minute + 30, 1.0),
        );
        let council = vec![(NodeId::new("c1"), addr(2))];
        let (_council_tx, council_rx) = watch::channel(council);
        let mut worker = RollupWorker::new(
            NodeId::new("w1"),
            worker_transport,
            Arc::clone(&mayo),
            council_rx,
            Duration::from_millis(100),
            CancellationToken::new(),
        );
        worker.push_rollup().await;
        assert!(!worker.send_extended, "a refused rollup started a backfill");
        let refused = mayo
            .read()
            .await
            .query("mayo_rollups_refused_total", 0, i64::MAX as u64)
            .await
            .unwrap();
        assert_eq!(refused.last().map(|row| row.3), Some(1.0), "{refused:?}");
    }

    #[tokio::test]
    async fn sends_extended_rollup_on_reassignment() {
        let net = InMemoryReportingNetwork::new();
        let worker_transport = net.register(addr(1)).await;
        let _c1_transport = net.register(addr(2)).await;
        let c2_transport = net.register(addr(3)).await;
        let shutdown = CancellationToken::new();

        let dir = tempfile::tempdir().unwrap();
        let mayo = Arc::new(RwLock::new(MayoStore::new(dir.path().to_path_buf())));

        let initial_council = vec![(NodeId::new("c1"), addr(2))];
        let (council_tx, council_rx) = watch::channel(initial_council);

        let mut worker = RollupWorker::new(
            NodeId::new("w1"),
            worker_transport,
            mayo,
            council_rx,
            Duration::from_millis(100),
            shutdown.clone(),
        );

        assert!(!worker.send_extended);

        let handle = tokio::spawn(async move { worker.run().await });

        // Change council to c2 only
        council_tx.send(vec![(NodeId::new("c2"), addr(3))]).unwrap();

        // c2 should receive a rollup
        let result = tokio::time::timeout(Duration::from_secs(2), c2_transport.recv()).await;
        assert!(
            result.is_ok(),
            "c2 should receive a rollup after reassignment"
        );

        shutdown.cancel();
        let _ = handle.await;
    }
}

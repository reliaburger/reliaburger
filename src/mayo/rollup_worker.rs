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

use super::rollup_generator::RollupGenerator;
use super::store::MayoStore;

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
        let store = self.mayo.read().await;
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
        drop(store);

        let rollups = match generated {
            Ok(r) => r,
            Err(e) => {
                // A failed generation used to vanish silently; the stateless
                // generator only rolls up the previous minute, so a swallowed
                // error is a permanent gap. Log it and retry next tick (M9).
                eprintln!("mayo: rollup generation failed, skipping this tick: {e}");
                return;
            }
        };

        let mut all_sent = true;
        for rollup in rollups {
            if let Err(e) = self
                .transport
                .send(parent, &ReportingMessage::MetricsRollup(rollup))
                .await
            {
                eprintln!("mayo: rollup push to parent failed, will retry next tick: {e}");
                all_sent = false;
                break;
            }
        }

        // Only drop the backfill request once every minute has actually been
        // sent (M9). A partial success re-sends some minutes next tick, which
        // the parent's `(node, timestamp)` dedup makes idempotent.
        if extended && all_sent {
            self.send_extended = false;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mayo::types::{MetricKey, Sample};
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
        worker.send_extended = true;

        worker.push_rollup().await;
        assert!(worker.send_extended, "flag survives a failed send");
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

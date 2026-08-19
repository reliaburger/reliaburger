//! Rollup generator for hierarchical metrics aggregation.
//!
//! Runs on each node. Queries the local `MayoStore` for the previous
//! time window and produces a `NodeRollup` containing per-metric
//! aggregate statistics (min, max, sum, count).

use std::collections::BTreeMap;

use crate::meat::NodeId;

use super::rollup::{NodeRollup, RollupAggregate, RollupEntry};
use super::store::MayoStore;
use super::types::MayoError;

/// Default rollup window in seconds.
pub const DEFAULT_ROLLUP_WINDOW_SECS: u64 = 60;

/// Total span (5 minutes) re-sent as per-minute rollups after reassignment.
pub const BACKFILL_SPAN_SECS: u64 = 300;

/// Generates `NodeRollup` entries from a local `MayoStore`.
pub struct RollupGenerator {
    node_id: NodeId,
}

impl RollupGenerator {
    /// Create a new generator for the given node.
    pub fn new(node_id: NodeId) -> Self {
        Self { node_id }
    }

    /// Generate the previous complete minute's rollup from the local MayoStore.
    ///
    /// The window start is aligned down to a multiple of
    /// `DEFAULT_ROLLUP_WINDOW_SECS` (OBS2). Without alignment two nodes ticking
    /// a few seconds apart stamp the same data with different `timestamp`s, so
    /// the council can't tell one node's re-sent window from a genuinely new
    /// one and can't deduplicate. Aligned windows give every node the same
    /// `(node, window)` key for the same minute.
    pub async fn generate(&self, store: &MayoStore, now: u64) -> Result<NodeRollup, MayoError> {
        // Align the window end down to a minute boundary, then step back one
        // window. `now - (now % W)` is the start of the current minute; the
        // completed window we roll up is the one before it.
        let aligned_end = now - (now % DEFAULT_ROLLUP_WINDOW_SECS);
        let start = aligned_end.saturating_sub(DEFAULT_ROLLUP_WINDOW_SECS);
        self.window_rollup(store, start, aligned_end).await
    }

    /// Generate the reassignment backfill: the last `BACKFILL_SPAN_SECS` as
    /// **per-minute** rollups, oldest first, empty minutes skipped.
    ///
    /// The backfill used to be one aggregate spanning the whole 5 minutes,
    /// stamped at `aligned_end - 300` — always a minute boundary an ordinary
    /// rollup had already used. The aggregator's `(node, timestamp)` dedup then
    /// either dropped the entire backfill (reassignment back to a previous
    /// parent) or a fresh parent's 5-minute row was summed with the old
    /// parent's overlapping 1-minute rows at query merge — double counting.
    /// Per-minute emission makes every row a genuine 60-second window, so the
    /// dedup drops exactly the minutes the parent already holds and keeps the
    /// rest. Minutes held by *both* parents still overlap at query merge;
    /// that is inherent to aggregator reassignment without handoff.
    pub async fn generate_backfill(
        &self,
        store: &MayoStore,
        now: u64,
    ) -> Result<Vec<NodeRollup>, MayoError> {
        let aligned_end = now - (now % DEFAULT_ROLLUP_WINDOW_SECS);
        let span_start = aligned_end.saturating_sub(BACKFILL_SPAN_SECS);

        let mut rollups = Vec::new();
        let mut start = span_start;
        while start < aligned_end {
            let rollup = self
                .window_rollup(store, start, start + DEFAULT_ROLLUP_WINDOW_SECS)
                .await?;
            if !rollup.entries.is_empty() {
                rollups.push(rollup);
            }
            start += DEFAULT_ROLLUP_WINDOW_SECS;
        }
        Ok(rollups)
    }

    /// Roll up one `[start, end)` window into a `NodeRollup` stamped at `start`.
    async fn window_rollup(
        &self,
        store: &MayoStore,
        start: u64,
        end: u64,
    ) -> Result<NodeRollup, MayoError> {
        let aggregates = store.query_window_aggregates(start, end).await?;

        let entries = aggregates
            .into_iter()
            .map(|(metric_name, labels_json, min, max, sum, count)| {
                let labels: BTreeMap<String, String> =
                    serde_json::from_str(&labels_json).unwrap_or_default();
                RollupEntry {
                    metric_name,
                    labels,
                    aggregate: RollupAggregate {
                        min,
                        max,
                        sum,
                        count,
                    },
                }
            })
            .collect();

        Ok(NodeRollup {
            node_id: self.node_id.clone(),
            timestamp: start,
            entries,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mayo::store::MayoStore;
    use crate::mayo::types::{MetricKey, Sample};

    fn test_store() -> (MayoStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = MayoStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    #[tokio::test]
    async fn generates_correct_aggregates() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("cpu_usage");
        // `now = 1080` aligns to window [1020, 1080); these three land inside.
        store.insert(&key, Sample::at(1020, 10.0));
        store.insert(&key, Sample::at(1040, 20.0));
        store.insert(&key, Sample::at(1060, 30.0));
        store.flush().await.unwrap();

        let generator = RollupGenerator::new(NodeId::new("node-1"));
        let rollup = generator.generate(&store, 1080).await.unwrap();

        assert_eq!(rollup.node_id, NodeId::new("node-1"));
        // The window start is aligned to a minute boundary.
        assert_eq!(rollup.timestamp, 1020);
        assert_eq!(rollup.entries.len(), 1);

        let entry = &rollup.entries[0];
        assert_eq!(entry.metric_name, "cpu_usage");
        assert_eq!(entry.aggregate.min, 10.0);
        assert_eq!(entry.aggregate.max, 30.0);
        assert_eq!(entry.aggregate.sum, 60.0);
        assert_eq!(entry.aggregate.count, 3);
    }

    /// The backfill covers the last 5 minutes as per-minute rollups, each
    /// stamped at its own minute start — the same `(node, timestamp)` keys an
    /// ordinary tick would have produced, so the aggregator's dedup works on
    /// them minute by minute. The old single-blob backfill stamped everything
    /// at one already-used minute key, which this test would have caught.
    #[tokio::test]
    async fn backfill_emits_per_minute_rollups_with_ordinary_stamps() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("mem");
        // now = 1000 aligns to 960; backfill span is [660, 960). Seed three
        // distinct minutes inside it: [660, 720), [780, 840), [900, 960).
        store.insert(&key, Sample::at(700, 100.0));
        store.insert(&key, Sample::at(800, 200.0));
        store.insert(&key, Sample::at(900, 300.0));
        store.flush().await.unwrap();

        let generator = RollupGenerator::new(NodeId::new("node-1"));
        let rollups = generator.generate_backfill(&store, 1000).await.unwrap();

        // Empty minutes ([720, 780) and [840, 900)) are skipped.
        assert_eq!(rollups.len(), 3);
        let stamps: Vec<u64> = rollups.iter().map(|r| r.timestamp).collect();
        assert_eq!(stamps, vec![660, 780, 900]);
        for (rollup, expected_sum) in rollups.iter().zip([100.0, 200.0, 300.0]) {
            assert_eq!(rollup.entries.len(), 1);
            assert_eq!(rollup.entries[0].aggregate.count, 1);
            assert_eq!(rollup.entries[0].aggregate.sum, expected_sum);
        }
    }

    /// A minute with samples in every sub-window backfills all five minutes,
    /// and the newest backfilled minute is exactly the one `generate` would
    /// produce — the backfill is a superset re-send, not a different shape.
    #[tokio::test]
    async fn backfill_newest_minute_matches_the_normal_rollup() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("cpu");
        for minute_start in (660..960).step_by(60) {
            store.insert(&key, Sample::at(minute_start + 30, 10.0));
        }
        store.flush().await.unwrap();

        let generator = RollupGenerator::new(NodeId::new("node-1"));
        let rollups = generator.generate_backfill(&store, 1000).await.unwrap();
        let normal = generator.generate(&store, 1000).await.unwrap();

        assert_eq!(rollups.len(), 5);
        assert_eq!(
            rollups.iter().map(|r| r.timestamp).collect::<Vec<_>>(),
            vec![660, 720, 780, 840, 900]
        );
        let newest = rollups.last().unwrap();
        assert_eq!(newest.timestamp, normal.timestamp);
        assert_eq!(newest.entries, normal.entries);
    }

    #[tokio::test]
    async fn normal_window_excludes_old_data() {
        let (mut store, _dir) = test_store();
        let key = MetricKey::simple("cpu");
        store.insert(&key, Sample::at(700, 100.0)); // outside window
        store.insert(&key, Sample::at(950, 50.0)); // inside window
        store.flush().await.unwrap();

        let generator = RollupGenerator::new(NodeId::new("node-1"));
        let rollup = generator.generate(&store, 1000).await.unwrap();

        assert_eq!(rollup.entries.len(), 1);
        assert_eq!(rollup.entries[0].aggregate.count, 1);
        assert_eq!(rollup.entries[0].aggregate.sum, 50.0);
    }

    /// A sample seeded into a complete aligned minute is rolled up whatever
    /// sub-minute phase `now` sits at when the worker pushes.
    ///
    /// This pins the invariant the `rollup_worker_delivers_node_rollups_to_the_leader`
    /// cluster test relies on. That test used to seed a single sample at a
    /// fixed `now - 30` offset, which fell inside the rolled-up previous minute
    /// only when `now % 60 < 30`; in the second half of a minute the previous
    /// window was empty, the leader ingested zero-entry rollups, and the test
    /// flaked. Seeding whole minutes fixes it — this test proves the window
    /// maths that makes that reliable.
    #[tokio::test]
    async fn seeded_previous_minute_is_rolled_up_at_any_phase() {
        // A sample in minute [1020, 1080) — the aligned minute before 1080.
        for phase in [0, 15, 29, 30, 45, 59] {
            let (mut store, _dir) = test_store();
            store.insert(&MetricKey::simple("cpu"), Sample::at(1050, 42.0));
            store.flush().await.unwrap();

            let generator = RollupGenerator::new(NodeId::new("node-1"));
            // `now` lands anywhere inside minute [1080, 1140); the generator
            // must always roll up the completed previous minute [1020, 1080).
            let now = 1080 + phase;
            let rollup = generator.generate(&store, now).await.unwrap();

            assert_eq!(
                rollup.entries.len(),
                1,
                "phase {phase}: the seeded previous-minute sample must be rolled up"
            );
            assert_eq!(rollup.timestamp, 1020);
            assert_eq!(rollup.entries[0].aggregate.count, 1);
        }
    }

    #[tokio::test]
    async fn empty_store_produces_empty_rollup() {
        let (store, _dir) = test_store();

        let generator = RollupGenerator::new(NodeId::new("node-1"));
        let rollup = generator.generate(&store, 1000).await.unwrap();

        assert!(rollup.entries.is_empty());
    }

    #[tokio::test]
    async fn multiple_metrics_produce_separate_entries() {
        let (mut store, _dir) = test_store();
        store.insert(&MetricKey::simple("cpu"), Sample::at(950, 50.0));
        store.insert(&MetricKey::simple("mem"), Sample::at(950, 1024.0));
        store.insert(&MetricKey::simple("disk"), Sample::at(950, 500.0));
        store.flush().await.unwrap();

        let generator = RollupGenerator::new(NodeId::new("node-1"));
        let rollup = generator.generate(&store, 1000).await.unwrap();

        assert_eq!(rollup.entries.len(), 3);
        let names: Vec<&str> = rollup
            .entries
            .iter()
            .map(|e| e.metric_name.as_str())
            .collect();
        assert!(names.contains(&"cpu"));
        assert!(names.contains(&"mem"));
        assert!(names.contains(&"disk"));
    }

    #[tokio::test]
    async fn labels_preserved_in_rollup() {
        let mut labels = BTreeMap::new();
        labels.insert("app".to_string(), "web".to_string());
        let key = MetricKey::with_labels("requests", labels.clone());

        let (mut store, _dir) = test_store();
        store.insert(&key, Sample::at(950, 100.0));
        store.flush().await.unwrap();

        let generator = RollupGenerator::new(NodeId::new("node-1"));
        let rollup = generator.generate(&store, 1000).await.unwrap();

        assert_eq!(rollup.entries.len(), 1);
        assert_eq!(rollup.entries[0].labels, labels);
    }

    /// The key unit test from the roadmap: node-level partial aggregates
    /// combine correctly at the council level.
    #[tokio::test]
    async fn hierarchical_aggregation_correctness() {
        use crate::mayo::rollup_store::RollupStore;

        // Simulate 3 nodes with known CPU values
        let mut stores = Vec::new();
        let values = [10.0, 20.0, 30.0];

        for (i, &val) in values.iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            let mut store = MayoStore::new(dir.path().to_path_buf());
            let key = MetricKey::simple("cpu_usage");
            store.insert(&key, Sample::at(950, val));
            store.flush().await.unwrap();
            stores.push((store, dir, format!("node-{}", i + 1)));
        }

        // Generate rollups from each node
        let mut rollups = Vec::new();
        for (store, _dir, node_name) in &stores {
            let generator = RollupGenerator::new(NodeId::new(node_name));
            let rollup = generator.generate(store, 1000).await.unwrap();
            rollups.push(rollup);
        }

        // Ingest all rollups into a single council RollupStore
        let council_dir = tempfile::tempdir().unwrap();
        let mut council_store = RollupStore::new(council_dir.path().to_path_buf());
        for rollup in &rollups {
            council_store.ingest(rollup);
        }
        council_store.flush().await.unwrap();

        // Verify cluster-wide aggregation
        let aggs = council_store
            .query_cluster_aggregates("cpu_usage", 0, 9999)
            .await
            .unwrap();

        assert_eq!(aggs.len(), 1);
        let agg = &aggs[0];

        // MIN across nodes = min(10, 20, 30) = 10
        assert_eq!(agg.min, 10.0);
        // MAX across nodes = max(10, 20, 30) = 30
        assert_eq!(agg.max, 30.0);
        // SUM across nodes = 10 + 20 + 30 = 60
        assert_eq!(agg.sum, 60.0);
        // COUNT across nodes = 1 + 1 + 1 = 3
        assert_eq!(agg.count, 3);
        // AVG = 60 / 3 = 20
        assert_eq!(agg.avg(), Some(20.0));
    }
}

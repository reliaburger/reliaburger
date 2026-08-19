/// Integration tests for hierarchical metrics aggregation.
///
/// Verifies end-to-end: workers generate rollups from local MayoStores,
/// rollups are ingested into council RollupStores (mirroring what
/// ReportAggregator does), and cluster-wide queries merge results correctly.
///
/// Transport routing is already tested in `tests/reporting_tree.rs`.
/// These tests focus on the aggregation maths.
use std::sync::Arc;

use tokio::sync::RwLock;

use reliaburger::mayo::query_fanout::merge_cluster_results;
use reliaburger::mayo::rollup::MetricsQueryRow;
use reliaburger::mayo::rollup_generator::RollupGenerator;
use reliaburger::mayo::rollup_store::RollupStore;
use reliaburger::mayo::store::MayoStore;
use reliaburger::mayo::types::{MetricKey, Sample};
use reliaburger::meat::NodeId;
use reliaburger::reporting::assignment::assign_parent;

/// 5-node cluster with deterministic metrics. Verify hierarchical
/// aggregation correctness via the full pipeline: generate rollups
/// from local MayoStores, ingest into council RollupStores (one per
/// council member), query and merge results.
#[tokio::test]
async fn five_node_cluster_hierarchical_aggregation() {
    // --- Setup council members ---
    let council_ids: Vec<NodeId> = (1..=3).map(|i| NodeId::new(format!("c{i}"))).collect();

    // --- Create rollup stores for each council member ---
    let mut rollup_stores = Vec::new();
    let mut rollup_store_dirs = Vec::new();
    for _i in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RwLock::new(RollupStore::new(dir.path().to_path_buf())));
        rollup_stores.push(store);
        rollup_store_dirs.push(dir);
    }

    // --- 5 workers with deterministic metrics ---
    let worker_ids: Vec<NodeId> = (1..=5).map(|i| NodeId::new(format!("w{i}"))).collect();
    let cpu_values = [10.0, 20.0, 30.0, 40.0, 50.0];
    // A fixed, minute-aligned `now` keeps the rollup window deterministic. The
    // generator rolls up `[now-60, now)`, so the sample at `now-30` lands inside
    // regardless of the wall clock (OBS2 epoch-aligned windows).
    let now = 6000u64;

    // Generate rollups from each worker and ingest into the assigned
    // council member's RollupStore (same as what ReportAggregator does).
    for (i, worker_id) in worker_ids.iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = MayoStore::new(dir.path().to_path_buf());

        // Insert deterministic CPU metric
        let key = MetricKey::simple("cpu_usage");
        store.insert(&key, Sample::at(now - 30, cpu_values[i]));
        store.flush().await.unwrap();

        // Generate rollup
        let generator = RollupGenerator::new(worker_id.clone());
        let rollup = generator.generate(&store, now).await.unwrap();

        // Determine which council member this worker is assigned to
        let parent_id = assign_parent(worker_id, &council_ids).unwrap();
        let parent_idx = council_ids.iter().position(|id| *id == parent_id).unwrap();

        // Directly ingest into the council member's rollup store
        rollup_stores[parent_idx].write().await.ingest(&rollup);
    }

    // Flush all council stores
    for store in &rollup_stores {
        store.write().await.flush().await.unwrap();
    }

    // --- Query all council rollup stores and merge ---
    let mut all_rows = Vec::new();
    for store in &rollup_stores {
        let s = store.read().await;
        let results = s
            .query_cluster_metric("cpu_usage", 0, u64::MAX)
            .await
            .unwrap();
        let rows: Vec<MetricsQueryRow> = results
            .into_iter()
            .map(|(ts, name, labels, val)| MetricsQueryRow {
                timestamp: ts,
                metric_name: name,
                labels,
                value: val,
            })
            .collect();
        all_rows.push(rows);
    }

    let merged = merge_cluster_results(all_rows);

    // Each council member aggregates sums of its assigned workers' cpu values.
    // merge_cluster_results sums partial aggregates from each council member.
    let total_sum: f64 = merged.iter().map(|r| r.value).sum();
    assert_eq!(
        total_sum, 150.0,
        "total CPU sum across cluster should be 150"
    );

    // --- Verify full aggregates from each store ---
    let mut cluster_min = f64::INFINITY;
    let mut cluster_max = f64::NEG_INFINITY;
    let mut cluster_sum = 0.0;
    let mut cluster_count: u32 = 0;

    for store in &rollup_stores {
        let s = store.read().await;
        let aggs = s
            .query_cluster_aggregates("cpu_usage", 0, u64::MAX)
            .await
            .unwrap();
        for agg in &aggs {
            if agg.min < cluster_min {
                cluster_min = agg.min;
            }
            if agg.max > cluster_max {
                cluster_max = agg.max;
            }
            cluster_sum += agg.sum;
            cluster_count += agg.count;
        }
    }

    assert_eq!(cluster_min, 10.0, "cluster-wide min CPU should be 10");
    assert_eq!(cluster_max, 50.0, "cluster-wide max CPU should be 50");
    assert_eq!(cluster_sum, 150.0, "cluster-wide sum CPU should be 150");
    assert_eq!(cluster_count, 5, "cluster-wide count should be 5");

    // Verify average: 150 / 5 = 30
    let cluster_avg = cluster_sum / cluster_count as f64;
    assert_eq!(cluster_avg, 30.0, "cluster-wide avg CPU should be 30");
}

/// Partial results returned when one aggregator is down.
///
/// Simulates a 3-council cluster where one council member's data is
/// unavailable. The query should return partial results from the
/// remaining aggregators.
#[tokio::test]
async fn partial_results_when_aggregator_down() {
    let council_ids: Vec<NodeId> = (1..=3).map(|i| NodeId::new(format!("c{i}"))).collect();

    let mut rollup_stores = Vec::new();
    let mut rollup_store_dirs = Vec::new();
    for _i in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RwLock::new(RollupStore::new(dir.path().to_path_buf())));
        rollup_stores.push(store);
        rollup_store_dirs.push(dir);
    }

    let worker_ids: Vec<NodeId> = (1..=5).map(|i| NodeId::new(format!("w{i}"))).collect();
    let cpu_values = [10.0, 20.0, 30.0, 40.0, 50.0];
    // Minute-aligned fixed `now` for a deterministic rollup window (OBS2).
    let now = 6000u64;

    // Track which workers go to which council member
    let mut per_council_sum = [0.0f64; 3];

    for (i, worker_id) in worker_ids.iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = MayoStore::new(dir.path().to_path_buf());
        let key = MetricKey::simple("cpu_usage");
        store.insert(&key, Sample::at(now - 30, cpu_values[i]));
        store.flush().await.unwrap();

        let generator = RollupGenerator::new(worker_id.clone());
        let rollup = generator.generate(&store, now).await.unwrap();

        let parent_id = assign_parent(worker_id, &council_ids).unwrap();
        let parent_idx = council_ids.iter().position(|id| *id == parent_id).unwrap();

        per_council_sum[parent_idx] += cpu_values[i];
        rollup_stores[parent_idx].write().await.ingest(&rollup);
    }

    // Flush all stores
    for store in &rollup_stores {
        store.write().await.flush().await.unwrap();
    }

    // --- Query only c1 and c3 (c2 is "down") ---
    let mut available_rows = Vec::new();
    for idx in [0, 2] {
        let s = rollup_stores[idx].read().await;
        let results = s
            .query_cluster_metric("cpu_usage", 0, u64::MAX)
            .await
            .unwrap();
        let rows: Vec<MetricsQueryRow> = results
            .into_iter()
            .map(|(ts, name, labels, val)| MetricsQueryRow {
                timestamp: ts,
                metric_name: name,
                labels,
                value: val,
            })
            .collect();
        available_rows.push(rows);
    }

    let merged = merge_cluster_results(available_rows);

    // The partial sum should be c1_sum + c3_sum (c2 data is missing)
    let partial_sum: f64 = merged.iter().map(|r| r.value).sum();
    let expected_partial = per_council_sum[0] + per_council_sum[2];
    assert_eq!(
        partial_sum, expected_partial,
        "partial sum from c1+c3 should be {expected_partial}, got {partial_sum}"
    );

    // c2's data is missing, so partial_sum < 150 (unless c2 happened
    // to get zero workers)
    if per_council_sum[1] > 0.0 {
        assert!(
            partial_sum < 150.0,
            "partial sum should be less than full cluster total"
        );
    }

    // The full sum would be 150, but we lost c2's contribution
    let missing = per_council_sum[1];
    assert_eq!(
        partial_sum + missing,
        150.0,
        "partial + missing should equal full total"
    );
}

/// Aggregator reassignment backfill neither drops minutes nor collapses them
/// onto one key (M9).
///
/// The backfill used to be a single 5-minute aggregate stamped at a minute key
/// an ordinary rollup had already used. Reassignment *back* to a previous
/// aggregator then dropped the whole backfill as a duplicate (losing the
/// minutes that aggregator never saw), and a *fresh* aggregator served the
/// 5-minute blob on one timestamp, which the merge summed with the old
/// aggregator's overlapping 1-minute row. Per-minute backfill emission makes
/// both cases behave: dedup drops exactly the already-held minutes, and every
/// minute only one store holds merges to its true value.
#[tokio::test]
async fn backfill_after_reassignment_neither_drops_nor_double_counts() {
    let worker_id = NodeId::new("w1");
    // One sample of 1.0 in each of the five completed minutes [5700, 6000).
    let dir = tempfile::tempdir().unwrap();
    let mut store = MayoStore::new(dir.path().to_path_buf());
    let key = MetricKey::simple("cpu_usage");
    for minute in (5700..6000).step_by(60) {
        store.insert(&key, Sample::at(minute + 30, 1.0));
    }
    store.flush().await.unwrap();
    let generator = RollupGenerator::new(worker_id.clone());

    // Ordinary ticks delivered minutes 5700 and 5760 to aggregator A before
    // the reassignment.
    let dir_a = tempfile::tempdir().unwrap();
    let mut store_a = RollupStore::new(dir_a.path().to_path_buf());
    for now in [5760u64, 5820] {
        let rollup = generator.generate(&store, now).await.unwrap();
        assert!(store_a.ingest(&rollup));
    }

    let backfill = generator.generate_backfill(&store, 6000).await.unwrap();
    assert_eq!(backfill.len(), 5);

    // --- Reassignment back to A: dedup keeps exactly the missing minutes ---
    let mut ingested = Vec::new();
    for rollup in &backfill {
        ingested.push(store_a.ingest(rollup));
    }
    assert_eq!(
        ingested,
        vec![false, false, true, true, true],
        "already-held minutes drop, missing minutes ingest"
    );
    store_a.flush().await.unwrap();
    let rows_a = store_a
        .query_cluster_metric("cpu_usage", 0, u64::MAX)
        .await
        .unwrap();
    let mut minutes_a: Vec<(u64, f64)> = rows_a.iter().map(|(ts, _, _, v)| (*ts, *v)).collect();
    minutes_a.sort_unstable_by_key(|(ts, _)| *ts);
    assert_eq!(
        minutes_a,
        vec![
            (5700, 1.0),
            (5760, 1.0),
            (5820, 1.0),
            (5880, 1.0),
            (5940, 1.0)
        ],
        "every minute exactly once — the blob backfill lost 5820-5940 here"
    );

    // --- Reassignment to a fresh aggregator B: no minute is missing and no
    // minute B alone holds is inflated ---
    let dir_b = tempfile::tempdir().unwrap();
    let mut store_b = RollupStore::new(dir_b.path().to_path_buf());
    for rollup in &backfill {
        assert!(store_b.ingest(rollup));
    }
    store_b.flush().await.unwrap();
    let to_rows = |rows: Vec<(u64, String, String, f64)>| {
        rows.into_iter()
            .map(|(ts, name, labels, val)| MetricsQueryRow {
                timestamp: ts,
                metric_name: name,
                labels,
                value: val,
            })
            .collect::<Vec<_>>()
    };
    // A as it stood before the reassignment-back above: minutes 5700, 5760.
    let dir_a2 = tempfile::tempdir().unwrap();
    let mut store_a2 = RollupStore::new(dir_a2.path().to_path_buf());
    for now in [5760u64, 5820] {
        let rollup = generator.generate(&store, now).await.unwrap();
        store_a2.ingest(&rollup);
    }
    store_a2.flush().await.unwrap();
    let merged = merge_cluster_results(vec![
        to_rows(
            store_a2
                .query_cluster_metric("cpu_usage", 0, u64::MAX)
                .await
                .unwrap(),
        ),
        to_rows(
            store_b
                .query_cluster_metric("cpu_usage", 0, u64::MAX)
                .await
                .unwrap(),
        ),
    ]);
    let by_minute: std::collections::BTreeMap<u64, f64> =
        merged.iter().map(|r| (r.timestamp, r.value)).collect();
    // The blob backfill served all five minutes on key 5700 (value 6.0 after
    // the merge) and left 5820-5940 with no row at all. Per-minute emission
    // keeps every minute present at its true value; only the two minutes both
    // aggregators hold overlap (inherent to reassignment without handoff).
    assert_eq!(by_minute[&5820], 1.0);
    assert_eq!(by_minute[&5880], 1.0);
    assert_eq!(by_minute[&5940], 1.0);
    assert_eq!(by_minute[&5700], 2.0, "known cross-aggregator overlap");
    assert_eq!(by_minute[&5760], 2.0, "known cross-aggregator overlap");
}

/// Multiple metrics with labels produce separate entries in the rollup.
#[tokio::test]
async fn multi_metric_aggregation_with_labels() {
    // Minute-aligned fixed `now` for a deterministic rollup window (OBS2).
    let now = 6000u64;

    // Two nodes, each with CPU and memory metrics
    for (node_name, cpu_val, mem_val) in [("n1", 25.0, 512.0), ("n2", 75.0, 1024.0)] {
        let node_dir = tempfile::tempdir().unwrap();
        let mut store = MayoStore::new(node_dir.path().to_path_buf());

        let mut cpu_labels = std::collections::BTreeMap::new();
        cpu_labels.insert("app".to_string(), "web".to_string());
        let cpu_key = MetricKey::with_labels("cpu_usage", cpu_labels);
        store.insert(&cpu_key, Sample::at(now - 30, cpu_val));

        let mem_key = MetricKey::simple("memory_mb");
        store.insert(&mem_key, Sample::at(now - 30, mem_val));

        store.flush().await.unwrap();

        let generator = RollupGenerator::new(NodeId::new(node_name));
        let rollup = generator.generate(&store, now).await.unwrap();
        assert_eq!(rollup.entries.len(), 2, "should have cpu and mem entries");
    }
}

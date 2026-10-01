//! #310: Mayo's memory over a long run.
//!
//! A node's metrics loop never stops: every minute it flushes a Parquet file,
//! every alert interval it reads the last two minutes back, and every minute
//! the rollup worker aggregates the previous minute. Retention keeps seven
//! days of files by default, so whatever one of those periodic reads costs
//! must not grow with the history on disk, or a node's memory climbs for a
//! week before it levels off.
//!
//! This binary counts every heap byte through its own global allocator, so
//! the numbers are the store's allocations, not whatever the allocator keeps
//! mapped. It simulates hours of collection with explicit timestamps (no
//! waiting), and after each block of history measures one periodic cycle:
//! the peak heap it needs, and what it leaves behind.
//!
//! #377 adds the reads that aren't periodic but still mustn't scale with
//! history: queries with no lower bound, windows far behind the newest data,
//! the council's rollup store, and the object-store backend.
//!
//! The counters are process-wide, so each test needs its own process, which
//! is how nextest runs them (`make test`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use reliaburger::mayo::rollup::{NodeRollup, RollupAggregate, RollupEntry};
use reliaburger::mayo::rollup_generator::RollupGenerator;
use reliaburger::mayo::rollup_store::RollupStore;
use reliaburger::mayo::store::{ALL_QUERY_ROW_LIMIT, MayoStore};
use reliaburger::mayo::types::{MetricKey, Sample};
use reliaburger::mayo::webhook::gather_latest_values;
use reliaburger::meat::NodeId;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to the system allocator with the caller's own
// layout and pointer; the counters are bookkeeping only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged from our caller, who upholds `alloc`'s contract.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` came from `alloc` above with this same layout.
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Collection interval, as `[metrics] collection_interval_secs` defaults.
const TICK_SECS: u64 = 10;
/// The collection loop flushes every sixth tick.
const TICKS_PER_FLUSH: u64 = 6;
/// Node metrics plus a handful of apps' CPU and memory.
const SERIES: usize = 40;

fn series_keys() -> Vec<MetricKey> {
    (0..SERIES)
        .map(|index| {
            let labels = BTreeMap::from([
                ("app".to_string(), format!("default/app-{}", index / 2)),
                ("node".to_string(), "node-1".to_string()),
            ]);
            let name = if index % 2 == 0 {
                "app_cpu_percent"
            } else {
                "app_memory_bytes"
            };
            MetricKey::with_labels(name, labels)
        })
        .collect()
}

/// Write `minutes` of collection starting at `from`, one Parquet file
/// per minute, exactly as the collection loop would.
async fn collect(store: &mut MayoStore, keys: &[MetricKey], from: u64, minutes: u64) {
    for minute in 0..minutes {
        for tick in 0..TICKS_PER_FLUSH {
            let timestamp = from + minute * 60 + tick * TICK_SECS;
            for (index, key) in keys.iter().enumerate() {
                let value = (timestamp % 97) as f64 + index as f64;
                store.insert(key, Sample { timestamp, value });
            }
        }
        store.flush().await.unwrap();
    }
}

/// One alert evaluation and one rollup, measured: (peak above the starting
/// heap, heap left behind).
async fn periodic_cycle(store: &MayoStore, rollups: &RollupGenerator, now: u64) -> (usize, isize) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let latest = gather_latest_values(store).await;
    let rollup = rollups.generate(store, now).await.unwrap();
    assert!(!latest.is_empty(), "the alert read found no fresh series");
    assert!(!rollup.entries.is_empty(), "the rollup found no samples");
    drop((latest, rollup));
    let peak = PEAK.load(Ordering::Relaxed) - before;
    let after = LIVE.load(Ordering::Relaxed);
    (peak, after as isize - before as isize)
}

#[tokio::test(flavor = "current_thread")]
async fn periodic_reads_stay_flat_as_history_grows() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MayoStore::new(directory.path().to_path_buf());
    let keys = series_keys();
    let rollups = RollupGenerator::new(NodeId::new("node-1"));

    // Blocks of history, newest first: the first ends at the current minute,
    // so the alert read's two-minute window and the rollup's previous minute
    // always have samples, and each later block reaches another hour back.
    const BLOCK_MINUTES: u64 = 60;
    const BLOCKS: u64 = 4;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let current_minute = now - now % 60;

    let mut measured = Vec::new();
    for block in 0..BLOCKS {
        let from = current_minute - (block + 1) * BLOCK_MINUTES * 60;
        collect(&mut store, &keys, from, BLOCK_MINUTES).await;
        let hours = block + 1;
        let (peak, retained) = periodic_cycle(&store, &rollups, now).await;
        println!(
            "history {hours} h ({} files): cycle peak {} KiB, retained {} B",
            hours * BLOCK_MINUTES,
            peak / 1024,
            retained
        );
        measured.push((hours, peak, retained));
    }

    let (_, first_peak, _) = measured[0];
    let (_, last_peak, _) = measured[measured.len() - 1];
    for (hours, _, retained) in &measured {
        assert!(
            *retained < 256 * 1024,
            "a periodic cycle kept {retained} B after returning, at {hours} h of history"
        );
    }
    assert!(
        last_peak * 2 < first_peak * 3,
        "one periodic cycle needed {} KiB at 1 h of history and {} KiB at {BLOCKS} h: \
         its working set grows with the history on disk",
        first_peak / 1024,
        last_peak / 1024
    );
}

/// Peak heap one read needs above the heap it started from. The read's own
/// result is dropped inside, so this is its working set plus its answer.
async fn peak_of<T>(read: impl std::future::Future<Output = T>) -> usize {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    drop(read.await);
    PEAK.load(Ordering::Relaxed) - before
}

/// Fail if any read's peak at the most history is half again its peak at the
/// least: a working set that tracks the history on disk, not the answer.
/// Every read is reported before failing, so one run shows all that grew.
fn assert_flat(reads: &[(&str, &[(u64, usize)])]) {
    let mut grew = Vec::new();
    for (label, peaks) in reads {
        let (first_hours, first) = peaks[0];
        let (last_hours, last) = peaks[peaks.len() - 1];
        let each: Vec<String> = peaks
            .iter()
            .map(|(hours, peak)| format!("{} KiB at {hours} h", peak / 1024))
            .collect();
        println!("{label}: {}", each.join(", "));
        if last * 2 >= first * 3 {
            grew.push(format!(
                "{label} needed {} KiB at {first_hours} h of history and {} KiB at \
                 {last_hours} h",
                first / 1024,
                last / 1024
            ));
        }
    }
    assert!(
        grew.is_empty(),
        "working sets grow with the history on disk:\n{}",
        grew.join("\n")
    );
}

fn start_of_current_minute() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    now - now % 60
}

/// #377: reads with no lower bound, or a window far behind the newest data,
/// used to load every file at or after their start. Their answers are the
/// same size at one hour of history and at four, so their working set must
/// be too.
#[tokio::test(flavor = "current_thread")]
async fn unbounded_and_old_window_reads_stay_flat_as_history_grows() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MayoStore::new(directory.path().to_path_buf());
    let keys = series_keys();
    const BLOCK_MINUTES: u64 = 60;
    const BLOCKS: u64 = 4;
    // History grows forwards from four hours ago, the way a node fills it.
    let origin = start_of_current_minute() - BLOCKS * BLOCK_MINUTES * 60;
    let unbounded_end = i64::MAX as u64;
    let per_series_aggregate = "SELECT MAX(timestamp) AS timestamp, metric_name, labels, \
         MAX(value) AS value FROM metrics GROUP BY metric_name, labels";

    let mut everything = Vec::new();
    let mut names = Vec::new();
    let mut old_window = Vec::new();
    let mut aggregate = Vec::new();
    for block in 0..BLOCKS {
        collect(
            &mut store,
            &keys,
            origin + block * BLOCK_MINUTES * 60,
            BLOCK_MINUTES,
        )
        .await;
        let hours = block + 1;

        let rows = store.query_all(0, unbounded_end).await.unwrap();
        assert_eq!(rows.len(), ALL_QUERY_ROW_LIMIT);
        drop(rows);
        everything.push((hours, peak_of(store.query_all(0, unbounded_end)).await));

        assert_eq!(store.metric_names().await.unwrap().len(), 2);
        names.push((hours, peak_of(store.metric_names()).await));

        // The first ten minutes of history, however much came after them.
        let window = store
            .query("app_cpu_percent", origin, origin + 599)
            .await
            .unwrap();
        assert_eq!(window.len(), 10 * TICKS_PER_FLUSH as usize * SERIES / 2);
        drop(window);
        old_window.push((
            hours,
            peak_of(store.query("app_cpu_percent", origin, origin + 599)).await,
        ));

        assert_eq!(
            store.query_sql(per_series_aggregate).await.unwrap().len(),
            SERIES
        );
        aggregate.push((hours, peak_of(store.query_sql(per_series_aggregate)).await));
    }

    assert_flat(&[
        ("/v1/metrics?name=* with no start", &everything),
        ("metric names", &names),
        ("a window at the start of history", &old_window),
        ("a per-series aggregate over all history", &aggregate),
    ]);
}

/// Every worker's rollup for one minute, as a council member receives them.
fn rollups_for_minute(keys: &[MetricKey], minute: u64) -> Vec<NodeRollup> {
    (0..3)
        .map(|node| NodeRollup {
            node_id: NodeId::new(format!("node-{node}")),
            timestamp: minute,
            entries: keys
                .iter()
                .enumerate()
                .map(|(index, key)| {
                    let value = (minute % 89) as f64 + index as f64;
                    RollupEntry {
                        metric_name: key.name.0.clone(),
                        labels: key.labels.clone(),
                        aggregate: RollupAggregate {
                            min: value,
                            max: value + 1.0,
                            sum: value * 6.0,
                            count: 6,
                        },
                    }
                })
                .collect(),
        })
        .collect()
}

/// #377: a council member's rollup store flushes a file a minute too, and its
/// session loaded every one of them for each cluster query.
#[tokio::test(flavor = "current_thread")]
async fn rollup_reads_stay_flat_as_history_grows() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = RollupStore::new(directory.path().to_path_buf());
    let keys = series_keys();
    const BLOCK_MINUTES: u64 = 60;
    const BLOCKS: u64 = 4;
    let origin = start_of_current_minute() - BLOCKS * BLOCK_MINUTES * 60;

    let mut cluster_metric = Vec::new();
    let mut aggregates = Vec::new();
    let mut owned = Vec::new();
    let mut names = Vec::new();
    for block in 0..BLOCKS {
        for minute in 0..BLOCK_MINUTES {
            let timestamp = origin + (block * BLOCK_MINUTES + minute) * 60;
            for rollup in rollups_for_minute(&keys, timestamp) {
                assert!(store.ingest(&rollup));
            }
            store.flush().await.unwrap();
        }
        let hours = block + 1;
        // The last five minutes, as the autoscaler and dashboards ask.
        let end = origin + (block + 1) * BLOCK_MINUTES * 60;
        let start = end - 300;

        let rows = store
            .query_cluster_metric("app_cpu_percent", start, end)
            .await
            .unwrap();
        assert_eq!(rows.len(), 5 * SERIES / 2);
        drop(rows);
        cluster_metric.push((
            hours,
            peak_of(store.query_cluster_metric("app_cpu_percent", start, end)).await,
        ));
        aggregates.push((
            hours,
            peak_of(store.query_cluster_aggregates("app_cpu_percent", start, end)).await,
        ));
        owned.push((
            hours,
            peak_of(store.query_owned_rows(Some("app_cpu_percent"), start, end)).await,
        ));
        assert_eq!(store.metric_names().await.unwrap().len(), 2);
        names.push((hours, peak_of(store.metric_names()).await));
    }

    assert_flat(&[
        ("a cluster metric's last five minutes", &cluster_metric),
        ("cluster aggregates for the autoscaler", &aggregates),
        ("owned rollup rows for the fan-out", &owned),
        ("rollup metric names", &names),
    ]);
}

/// #377: the object-store backend fetched every object for every read, even
/// the periodic ones the local backend already bounded (#321).
#[tokio::test(flavor = "current_thread")]
async fn object_store_reads_stay_flat_as_history_grows() {
    let directory = tempfile::tempdir().unwrap();
    let bucket = tempfile::tempdir().unwrap();
    let url = url::Url::from_file_path(bucket.path()).unwrap().to_string();
    let mut store = MayoStore::open(directory.path().to_path_buf(), Some(&url))
        .await
        .unwrap();
    let keys = series_keys();
    let rollups = RollupGenerator::new(NodeId::new("node-1"));
    const BLOCK_MINUTES: u64 = 60;
    const BLOCKS: u64 = 4;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let current_minute = now - now % 60;
    let top_names = ["app_cpu_percent", "app_memory_bytes"];

    let mut cycles = Vec::new();
    let mut top = Vec::new();
    for block in 0..BLOCKS {
        // Newest first, as in the periodic test, so every block's reads find
        // the current minute's samples.
        let from = current_minute - (block + 1) * BLOCK_MINUTES * 60;
        collect(&mut store, &keys, from, BLOCK_MINUTES).await;
        let hours = block + 1;
        let (peak, _) = periodic_cycle(&store, &rollups, now).await;
        cycles.push((hours, peak));

        let since = now - 120;
        assert!(
            !store
                .query_names_since(&top_names, since)
                .await
                .unwrap()
                .is_empty()
        );
        top.push((
            hours,
            peak_of(store.query_names_since(&top_names, since)).await,
        ));
    }

    assert_flat(&[
        ("an alert and rollup cycle over a bucket", &cycles),
        ("relish top over a bucket", &top),
    ]);
}

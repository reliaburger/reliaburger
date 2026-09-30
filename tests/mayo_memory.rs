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

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use reliaburger::mayo::rollup_generator::RollupGenerator;
use reliaburger::mayo::store::MayoStore;
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

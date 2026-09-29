# Mayo memory over a long run, 29 September 2026

Issue [#310](https://github.com/reliaburger/reliaburger/issues/310): the 0.1.0
V02 soaks saw Bun's resident memory climb for hours (node 2 of the sustained
run started at 181 MB, ended at 621 MB and peaked at 802 MB,
[record](2026-09-29-sustained-v02.md)) without tripping the leak checks.
Mayo was the suspect. This record measures it, names the cause and shows the
fix.

## How we measured

[`tests/mayo_memory.rs`](../../tests/mayo_memory.rs) is its own test binary
with a counting global allocator, so it measures the heap the store asks for,
not what the system allocator keeps mapped. It simulates collection with
explicit timestamps (no waiting): 40 labelled series every 10 seconds, one
Parquet flush a minute, exactly what the collection loop writes. After each
extra hour of history it runs one periodic cycle, the pair of reads every node
makes on a timer: the alert evaluator's latest values
(`gather_latest_values`, a two-minute window) and the rollup worker's previous
minute (`RollupGenerator::generate`). It records the cycle's peak heap and
what the cycle leaves allocated after it returns.

`cargo test --test mayo_memory -- --nocapture`, debug build, Apple M2 Max.

## Before

| History on disk | Parquet files | Cycle peak | Left after the cycle |
|---|---:|---:|---:|
| 1 h | 60 | 5,731 KiB | 78,620 B (first-use statics) |
| 2 h | 120 | 7,244 KiB | 64 B |
| 3 h | 180 | 8,811 KiB | 0 B |
| 4 h | 240 | 10,422 KiB | 0 B |

Nothing leaks: every cycle gives back what it took. But each cycle needs about
1.55 MiB more per hour of history, for 40 series. The reads ask for minutes of
data, yet `MayoStore::session` loaded every Parquet file in the directory into
an in-memory table before filtering. Retention keeps seven days by default, so
the working set of every alert evaluation and every rollup kept growing for a
week: roughly 260 MiB per cycle at 40 series, more with more series. Freed
memory goes back to the allocator, not always to the kernel, so resident
memory tracks the peaks. The soak capped metrics at `max_storage_mb = 8`
(about 9 MB on disk by the end), which bounds the effect there but still lets
it grow through the first hours; a default node has no size cap, only the
seven-day retention.

The in-memory structures the issue also suspected are bounded: the store
buffer is drained every flush, the rollup store's `seen_windows` is pruned to
its idempotency horizon and its buffer is capped, and the alert evaluator drops
inactive series it no longer sees.

## The fix

A read with a known lower bound now skips any local Parquet file whose newest
sample is older than that bound. The newest sample comes from the file's
row-group statistics in its footer, the same check retention pruning uses, so
skipping a file costs a footer read, not a data read. A file without usable
statistics is still read. The SQL keeps its own `timestamp >=` filter, so a
file that straddles the bound contributes only its in-window rows, as before
(`windowed_reads_keep_every_file_that_reaches_the_window`).

The windowed paths are the alert evaluator, the rollup worker, the autoscaler's
`query_avg`, per-app queries, `/v1/metrics` by name or `*`, and `relish top`'s
usage columns.

## After

| History on disk | Parquet files | Cycle peak | Left after the cycle |
|---|---:|---:|---:|
| 1 h | 60 | 3,933 KiB | 78,620 B (first-use statics) |
| 2 h | 120 | 4,109 KiB | 0 B |
| 3 h | 180 | 4,109 KiB | 0 B |
| 4 h | 240 | 4,109 KiB | 0 B |

Flat. The test now fails if the cycle's peak at four hours is 1.5 times its
peak at one hour or more, or if a cycle leaves more than 256 KiB behind.

## Still open

Queries without a lower bound (`metric_names`, arbitrary `query_sql`, the
object-store backend, and the council's rollup store) still load every file.
They run on demand, not on a timer, so they don't grow a node's steady-state
memory, but a wide query over a week of history is still expensive. Streaming
those queries is the 0.1.4 observability work in the
[roadmap](../roadmap.md#releases-after-010). A soak on a 0.1.1 candidate will
confirm the effect on resident memory.

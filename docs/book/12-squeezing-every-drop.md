# Squeezing Every Drop

Phases 1 through 11 built a complete orchestrator. It works, it's tested, it ships containers and watches them and recovers when they fall over. Phase 12 is different in character: nothing here adds a feature you can point at. Every change makes something already working use less disk, less bandwidth, or less CPU. It's the phase where you stop asking "does it work?" and start asking "what's it costing me?"

That's a lot of separate optimisations across a lot of subsystems — nftables maps for port forwarding, peer-to-peer image downloads, a pull-through cache, volume snapshots, log compression. We'll build them up over several passes, and this chapter grows with them. This first pass is about **log storage**: making archived logs small, and making archive queries skip work they don't need to do.

## Where the bytes actually are

Ketchup (Chapter 6) keeps logs in two places. There's the live path — the in-memory buffer and the `MemTable` that answers `relish logs` — and there's the Parquet on disk, written every flush, exported to S3 or GCS, and queried by `relish logs-search`. When we reach for compression, it's worth being precise about which one we're optimising, because the answer is not the obvious one.

Look at how the live query path is built (`src/ketchup/log_store.rs`):

```rust
let table = MemTable::try_new(Arc::new(log_schema()), vec![all_batches])?;
ctx.register_table("logs", Arc::new(table))?;
```

The live queries run against `MemTable` — the Arrow `RecordBatch`es held in memory. They never read the Parquet files back. So compressing those files does *nothing* for a `relish logs web` on a running cluster; that data is already in RAM. What the Parquet files feed is the *archive*: the bytes that get exported off the node and later queried with `relish logs-search` over a directory of `.parquet` files. That's the read path that matters here, and it's exactly what "archived logs" means.

Knowing that changes the design. We're not trying to speed up the hot path — it's already as fast as memory. We're trying to make the cold, archived copy cheap to store and cheap to scan.

## One change, two wins

Both optimisations live in a single place: the properties we hand to the Parquet writer. Until now, Ketchup created its writer with no properties at all:

```rust
let mut writer = ArrowWriter::try_new(file, Arc::new(log_schema()), None)?;
```

That `None` means Parquet defaults: Snappy compression, no bloom filters. We replace it with a deliberate `WriterProperties` (`src/ketchup/log_store.rs`):

```rust
fn log_writer_properties() -> WriterProperties {
    const BLOOM_FPP: f64 = 0.01;
    const BLOOM_NDV: u64 = 10_000;

    let mut builder = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_max_row_group_size(LOG_ROW_GROUP_SIZE);
    for column in ["app", "namespace"] {
        builder = builder
            .set_column_bloom_filter_enabled(column.into(), true)
            .set_column_bloom_filter_fpp(column.into(), BLOOM_FPP)
            .set_column_bloom_filter_ndv(column.into(), BLOOM_NDV);
    }
    builder.build()
}
```

Then `Some(log_writer_properties())` goes where the `None` was. That's the whole integration. Everything else — the export job that copies files, the `ListingTable` that reads them back — gets the benefit for free, because they all operate on whatever bytes the writer produced. Now let's unpack the two decisions baked into that function, because both come with a caveat the roadmap glossed over.

### ZSTD, and why Parquet is already "seekable"

The first decision is `Compression::ZSTD`. Log lines are gloriously repetitive — the same request paths, the same status codes, the same stack trace printed ten thousand times — and ZSTD eats repetition for breakfast. Against the flat text a `.log` file would hold, the compressed Parquet comes out more than five times smaller, often far more.

The roadmap called this item "zstd seekable frame compression", and the word that matters is *seekable*. The worry is real: if you gzip a 100MB log file into one blob, answering "give me the lines from 14:05 to 14:06" means decompressing the whole thing. Useless for random access.

But Parquet sidesteps this without any special framing on our part. A Parquet file is split into *row groups*, and each column chunk in each row group is compressed independently. To read one row group you decompress that group's chunks and nothing else. The format is random-access by construction; ZSTD slots in underneath per chunk. So we don't build a separate "seekable zstd" container — that would be reinventing what Parquet already does. We set the codec to ZSTD, set a modest row-group size so a query can skip to the groups it needs, and we're done. It's the same instinct as Chapter 6: reuse the engine, don't rebuild it.

```rust
const LOG_ROW_GROUP_SIZE: usize = 8192;
```

Small groups mean a time-range query touches only the groups overlapping that range. Large groups would compress slightly better but force the reader to crack open more data per match. Eight thousand rows is a reasonable middle.

### Bloom filters help equality, not `LIKE`

The second decision is bloom filters — and here the roadmap's instinct was half right in a way worth dwelling on, because the wrong version is a trap.

A bloom filter is a compact probabilistic structure that answers one question: "is value X *definitely not* here?" Parquet can attach one per column chunk, so a reader checking `WHERE app = 'web'` can ask each row group's filter "any 'web' in you?" and skip the groups that answer no. Cheap, and it never lies in the dangerous direction — a bloom filter has false positives (it occasionally says "maybe" when the answer is no) but never false negatives (it never says "no" when the answer is yes), so you can't miss data.

The roadmap asked for a bloom filter on the `line` column "to skip row groups in LIKE queries". That doesn't work, and it's important to see why. A bloom filter answers *equality* — "is the value exactly X". A log search is `WHERE line LIKE '%ERROR%'`, a *substring* match. There's no value X to look up; "ERROR" isn't a value in the column, it's a fragment of millions of distinct values. A bloom filter on `line` would be built from whole log lines and could never answer a substring question. It would cost space on every write and earn nothing.

So we put the filters where equality actually happens: `app` and `namespace`. Those are the columns `relish logs-search` and the cross-node queries filter on exactly (`WHERE app = 'web' AND namespace = 'prod'`), and there a bloom filter genuinely lets the reader skip archived row groups that hold no rows for that app. The `line` column gets no filter. Substring searches still lean on what Chapter 6 already described — columnar pruning and per-row-group min/max statistics — which is the honest set of tools for that job.

We also pin the false-positive rate. Parquet's default is 5%; we set 1% (`BLOOM_FPP`) and size the filter for up to ten thousand distinct values (`BLOOM_NDV`). For columns whose real cardinality is a handful of app names, that's a filter measured in bytes with an effective false-positive rate near zero — and it satisfies the target we test against.

One last piece: a writer that writes bloom filters is only half the story. The reader has to be told to use them. In `src/ketchup/remote_query.rs`, the archive query path turns pruning on explicitly:

```rust
let mut config = SessionConfig::new();
config.options_mut().execution.parquet.bloom_filter_on_read = true;
let ctx = SessionContext::new_with_config(config);
```

It's on by default in DataFusion, but setting it here means a future default change can't silently switch off the optimisation we built the filters for.

## Tests

All of this is pure Parquet and DataFusion — no Linux, no root, no network — so it runs under a plain `cargo test` on any machine. The tests live next to the code in `src/ketchup/log_store.rs` and split along the two wins.

**Compression.** `zstd_parquet_is_over_5x_smaller_than_raw_text` writes twenty thousand semi-realistic log lines, measures the resulting `.parquet` against the byte size of the equivalent flat text, and asserts the archive is more than five times smaller. The comparison is deliberately against *raw text*, not against an uncompressed Parquet file — Parquet already dictionary-encodes repeated strings, so comparing compressed-Parquet to uncompressed-Parquet would understate the real saving and miss the point. `zstd_archive_round_trips_through_remote_query` writes a thousand lines, flushes, and reads them all back through `query_remote` to prove ZSTD costs us no data. `time_range_random_access_across_row_groups` writes three row groups' worth of lines and asks for ten of them by timestamp, proving the per-row-group seek survives compression.

**Bloom filters.** `bloom_filters_written_on_app_and_namespace_only` opens the written file's Parquet metadata and asserts a bloom filter offset exists for `app` and `namespace` and is absent for `line` and `timestamp` — the honest design, checked. `equality_query_on_archive_returns_correct_app` confirms an `app =` query still returns exactly the right rows through the pruning read path. And `bloom_filter_false_positive_rate_under_one_percent` writes two thousand distinct values, reads the filter back, probes ten thousand values known to be absent, and asserts the observed false-positive rate stays under our 1% target — while every present value still checks true, because a bloom filter never reports a false negative.

That last test is a direct statistical measurement rather than a property-based one. Proptest is the right tool for exploring an input space; a false-positive *rate* is better pinned down by one large, fixed sample than by many small generated ones.

## What we learned

**Optimise the copy that's actually cold.** The reflex was to compress "the logs". Half a minute reading `log_store.rs` showed the live queries never touch the Parquet at all — they run on in-memory batches. Compression and bloom filters only ever pay off on the archived, exported copy. Always confirm which copy of the data your optimisation touches before you write it; the obvious target and the real one diverge more often than you'd think.

**Know what your index can actually answer.** A bloom filter on a substring-searched column is the kind of change that looks productive, passes review, ships, and quietly does nothing but cost bytes. Equality and substring are different questions, and only one of them has a cheap probabilistic answer. The honest move was to put the filter where equality lives and leave the substring case to the column scan — and to say so plainly rather than claim a speed-up we didn't get.

**Reuse beats reinventing, again.** "Seekable compression" sounded like a new container format with frame boundaries and an index. It turned out to be one line — set the codec to ZSTD — because Parquet's row groups already give random access. The whole storage layer keeps paying off the Chapter 6 decision to stand on Arrow, DataFusion, and Parquet rather than roll our own.

## One rule to map them all

The second pass moves from storage to networking. Here's what happens today when a packet arrives on a published port, say 30017. The kernel enters our `prerouting` chain and starts checking rules. `tcp dport 30001 dnat to 10.0.2.2:8080`? No. `tcp dport 30002 dnat to 10.0.2.3:8080`? No. It keeps going, one rule per container, until it hits the one that matches. Fifty containers, fifty rules, and the unlucky packet checks all of them. That's O(n) in the hot path of every inbound connection, and it's exactly the design kube-proxy got hammered for at scale.

We've already solved this shape of problem once. Chapter 3's eBPF service discovery put backend lookups in a kernel hash map — `(vip, port)` in, backend out, O(1) no matter how many services exist. nftables has the same trick built in, no eBPF required: a **named map**.

```
nft add map ip reliaburger portmap '{ type inet_service : ipv4_addr . inet_service ; }'
nft add rule ip reliaburger prerouting dnat ip addr . port to tcp dport map @portmap
```

The first line declares a typed map: service port in, "address . port" pair out (the `.` builds a concatenation — nftables' way of saying tuple). The second line is the only rule the chain needs, ever. It says: take the packet's TCP destination port, look it up in `@portmap`, and DNAT to whatever the map returns. Adding a container is no longer "append a rule"; it's "insert an element":

```
nft add element ip reliaburger portmap '{ 30017 : 10.0.2.5 . 8080 }'
```

One rule, one hash lookup, any number of containers. Removal gets better too, and this is the part I'm happiest about. The old code deleted a rule by running `nft -a list`, grepping the text output for the right rule, and parsing a handle number off the end of the line. Parsing the output of a CLI tool to undo your own change is a smell you learn to flinch at. With a map, deletion is `delete element ... { 30017 }` — keyed by the port we already know. No listing, no parsing, O(1).

### The Rust shape: generate argv, don't build strings

The new module (`src/grill/portmap.rs`) splits the work the same way the firewall chapter did: pure functions that *generate* commands, and a thin executor that *runs* them. The generators return `Vec<String>` — argv, not a shell string:

```rust
pub fn element_add(entry: &PortMapEntry) -> Vec<String> {
    ["add", "element", "ip", TABLE, MAP,
     &format!("{{ {} : {} . {} }}",
         entry.host_port, entry.container_ip, entry.container_port)]
    .into_iter().map(String::from).collect()
}
```

Why argv? Look at that last element: `{ 30017 : 10.0.2.5 . 8080 }`. In a shell, braces and spaces mean quoting, and quoting means the bug where your test passes and production breaks because something interpolated differently. Passed as a single argv element via `Command::args`, there is no shell and nothing to quote — `nft` receives the block exactly as we built it and joins the arguments itself. (Also note `{{` and `}}` in the `format!` string: that's how you write a literal brace, since `{}` is the placeholder syntax.)

Execution goes behind a trait, and this one introduces a Rust feature we haven't needed before:

```rust
pub trait NftExecutor: Send + Sync {
    fn run(&self, args: &[String])
    -> impl std::future::Future<Output = Result<(), String>> + Send;
}
```

If you're coming from Go: this is an interface with one method, except the method is async. Rust makes you say that explicitly — an async method really returns a future, and `impl Future<...> + Send` declares "some future type, and it's safe to move across threads" without naming the type. The payoff is the same as in Go: production code plugs in `NftCommandExecutor` (which shells out to `nft`), and the tests plug in a `RecordingExecutor` that appends every argv to a list and can be scripted to fail on the nth call. All the interesting logic becomes testable on a Mac with no nftables in sight.

And there is interesting logic, because batches can fail halfway. A container spec can publish several ports; if the second `add element` fails, we don't want the first one lingering as a half-applied mapping. `PortMapSet::apply` tracks what it added and rolls back on error:

```rust
if let Err(reason) = executor.run(&element_add(entry)).await {
    for port in &added_this_call {
        let _ = executor.run(&element_delete(*port)).await;
        self.applied.remove(port);
    }
    return Err(PortMapError::ApplyFailed { ... });
}
```

The same struct makes repeated applies *incremental* — a port that's already mapped is skipped, so re-registering a container's mappings after an agent restart doesn't error on duplicates. Both behaviours have direct unit tests (`apply_rolls_back_on_mid_batch_failure` asserts the exact argv sequence including the rollback delete; `apply_is_incremental` asserts the overlap is skipped), and they run everywhere precisely because the executor is a recording mock.

One design note: `PortMapError` is its own small error enum rather than reusing the netns module's `NetnsError`. Not for purity — for portability. The netns module is `#[cfg(target_os = "linux")]`, and tying portmap to it would have dragged the whole module (tests included) into Linux-only territory. A two-variant enum was the cheaper way to keep the logic testable on every platform. The Linux boundary wraps it at the call site.

### The switchover, and the surprise underneath it

Flipping `netns.rs` over was supposed to be mechanical: `ensure_nft_table` grows the map and the single lookup rule, `add_port_mapping` becomes an `add element`, teardown becomes `delete element`, and the forty lines of handle-parsing removal code get deleted with some satisfaction. All of that happened. But while tracing the call sites, a question wouldn't go away: who actually *calls* `add_port_mapping`?

The answer was: nobody. One integration test. The production runc path sets up the network namespace and the veth pair, but the DNAT rule that makes an allocated host port reach the container was never installed by any deploy. The whole per-rule mechanism we came to optimise was a well-tested library with no caller — the same trap we'll hit again with volumes later in this chapter, and the single most consistent lesson of the July review. An optimisation pass turned into a wiring fix.

So how do you get the port pair to where the network is created? The supervisor allocates the host port; the app spec declares the container port; runc creates the netns. Rather than threading a new method through the agent's four start paths, the pair rides along on data that already makes the journey — the OCI spec:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMapping {
    pub host_port: u16,
    pub container_port: u16,
}

// on OciSpec:
#[serde(default, skip_serializing_if = "Option::is_none")]
pub port_mapping: Option<PortMapping>,
```

`generate_oci_spec` already receives the allocated host port (it uses it for mounts), so populating the field is a `zip` of two `Option`s. RuncGrill installs the element right where it creates the namespace, stores the handle, and tears it down when the container dies or is deleted. The `#[serde(default)]` matters more than it looks: OCI specs are persisted in instance records for Phase 14's self-upgrade adoption, and records written by an older binary simply don't have the field. `default` makes them deserialise to `None` instead of failing — the difference between an upgrade that adopts running workloads and one that orphans them. (There's a test that literally deletes the field from the JSON and asserts the record still parses.)

Adoption has one more wrinkle worth savouring: when bun restarts under a self-upgrade, the containers keep running, and the kernel keeps their map elements. Nothing needs re-adding. But the *handle* — the Rust value whose shutdown deletes the element — died with the old process. So adoption rebuilds just the handle from the record, touching nftables not at all. Kernel state and process state have different lifetimes, and the adopt path is where you feel it.

Two smaller notes from the switchover. First, upgraded nodes still carry per-port rules from the old scheme, sitting ahead of the map rule and matching first. `ensure_nft_table` now lists the chain once, deletes any legacy `dnat to` rules by handle (the map's own rule prints as `dnat ip addr . port to ...` and can't match the filter — checked by a unit test with a canned listing), and that's the last handle-parsing this codebase does. Second, listing the chain gave us a guard for free: it turns out `nft add rule` happily appends duplicates, so the masquerade rule had been quietly duplicating on every port mapping since Phase 3. Probing before adding fixed that too.

And the C4 rule still stands: everything here lives in the `reliaburger` table, and nothing touches `reliaburger_fw`. The perimeter firewall reconcile deletes *its own* table wholesale; the day those two tables were one, a firewall refresh silently wiped every container's NAT. The guard test that keeps them separate stays green.

What the map deliberately doesn't fix: DNAT in `prerouting` only rewrites traffic *arriving at the node from outside*. A connection made from the node itself (or from a local container) to `container_ip:host_port` never traverses prerouting, and the cross-node story for container IPs is part of a bigger, known control-plane gap tracked in the July discrepancy register. This section made published ports genuinely reachable from outside the node, in O(1). It did not redesign the dataplane — and saying which is which out loud is half the value of a register like that.

---

*Still to come in Phase 12: the map switchover in the network namespace layer, peer-to-peer image downloads, the pull-through cache, volume snapshots and Btrfs quotas, and batch/build execution. This chapter grows with each.*

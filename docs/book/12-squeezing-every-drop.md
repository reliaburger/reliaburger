# Squeezing Every Drop

Phases 1 through 11 built a complete orchestrator. It works, it's tested, it ships containers and watches them and recovers when they fall over. Phase 12 is different in character: on paper, nothing here adds a feature you can point at. Every change makes something already working use less disk, less bandwidth, or less CPU. It's the phase where you stop asking "does it work?" and start asking "what's it costing me?"

That framing turned out to be half true, and the half that's false is the chapter's recurring plot twist. Optimisation work forces you to trace real data paths end to end — and untraced paths are where the gaps hide. Chasing O(1) port lookups uncovered that port mappings were never installed at all. Making image pulls faster uncovered that cluster-pushed images couldn't be deployed on other nodes, full stop. Wiring volume snapshots uncovered that managed volumes had never mounted. Again and again, "make it faster" became "make it exist".

The sections build up in the order the work happened: log storage first (compression and query pruning), then port mapping, the registry's replication and healing, peer-to-peer image distribution, the pull-through cache, volumes and snapshots, and finally batch and build execution.

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

### What the logs work taught us

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

The answer was: nobody. One integration test. The production runc path sets up the network namespace and the veth pair, but the DNAT rule that makes an allocated host port reach the container was never installed by any deploy. The whole per-rule mechanism we came to optimise was a well-tested library with no caller — the same trap we'll hit again with volumes later in this chapter, and the single most consistent lesson of the July review. An optimisation pass turned into a wiring fix. (A postscript: once runc grew its own command executor, the plain `add_port_mapping` wrapper lost even that test as a caller. We deleted it, and the test now calls `add_port_mapping_with_commands` directly.)

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

`generate_oci_spec` already receives the allocated host port (it uses it for mounts), so populating the field is a `zip` of two `Option`s. RuncGrill installs the element right where it creates the namespace, stores the handle, and tears it down when the container dies or is deleted. OCI specs are persisted in instance records for Phase 14's self-upgrade adoption, so the field has to round-trip. `skip_serializing_if` leaves a `None` out of the JSON entirely, and `#[serde(default)]` turns that absence back into `None` on the way in. A test round-trips both a mapping and its absence.

Adoption has one more wrinkle worth savouring: when bun restarts under a self-upgrade, the containers keep running, and the kernel keeps their map elements. Nothing needs re-adding. But the *handle* — the Rust value whose shutdown deletes the element — died with the old process. So adoption rebuilds just the handle from the record, touching nftables not at all. Kernel state and process state have different lifetimes, and the adopt path is where you feel it.

One smaller note from the switchover. `ensure_nft_table` lists the chain once before adding the map rule, and that listing gave us a guard for free: it turns out `nft add rule` happily appends duplicates, so the masquerade rule had been quietly duplicating on every port mapping since Phase 3. Probing before adding fixed that too. (We don't sweep per-port rules left by the old scheme. Nothing has shipped, so no node in the wild carries them.)

And the C4 rule still stands: everything here lives in the `reliaburger` table, and nothing touches `reliaburger_fw`. The perimeter firewall reconcile deletes *its own* table wholesale; the day those two tables were one, a firewall refresh silently wiped every container's NAT. The guard test that keeps them separate stays green.

What the map deliberately doesn't fix: DNAT in `prerouting` only rewrites traffic *arriving at the node from outside*. A connection made from the node itself (or from a local container) to `container_ip:host_port` never traverses prerouting, and the cross-node story for container IPs is part of a bigger, known control-plane gap tracked in the July discrepancy register. This section made published ports genuinely reachable from outside the node, in O(1). It did not redesign the dataplane — and saying which is which out loud is half the value of a register like that.

## Deleting data without losing it

Before this chapter's peer-to-peer downloads can exist, the registry needs three things it didn't have at the end of Phase 5: a catalogue every node agrees on, more than one copy of each image, and a garbage collector that can't destroy the last copy. All three landed with the big wiring pass, and the design that shipped differs from what we'd sketched in ways worth studying. This section describes what's actually in the tree, then hardens its weakest part.

### The catalogue lives in Raft; the blobs don't

The split is the whole design. An image is two very different kinds of data. The *manifest* — which layers, which tags, who holds them — is tiny, changes rarely, and everyone must agree on it: that's a Raft value. The *blobs* are big, immutable, and self-verifying (their name is their SHA-256): those stay on local disk, because pushing gigabytes through a consensus log would be absurd, and because a corrupted blob can't lie about itself anyway.

So a push stores blobs locally and then calls one function (`src/pickle/api.rs`):

```rust
// record_commit: apply locally, persist, propose.
state.catalog.write().await.apply_manifest_commit(&commit);
if let Some(path) = &state.persist_path {
    // catalog.json — survives restarts even without a cluster
}
if let Some(council) = &state.council {
    let _ = council.write(RaftRequest::ManifestCommit(commit)).await;
}
```

Standalone nodes get a JSON file; clustered nodes get consensus; the code path is the same either way, with `Option` as the seam. The commit records `holder_nodes = {this node}` — one copy, honestly labelled.

### Replication is a loop, not a promise

The original design said a push replicates to N peers *synchronously* before returning success. What shipped is asynchronous: the push returns once the local copy is durable, and a leader-side loop finds under-replicated manifests and fixes them. (The whitepaper still describes the synchronous version — that's discrepancy D11 in the July register, and reconciling the docs is separate work. The book describes reality.)

Is async worse? It trades a durability promise for availability: a push succeeds even when every peer is down, and the system converges later. For a cluster whose images are also sitting in a git-driven build pipeline, that's the right trade — but it makes the *heal loop* the load-bearing component, which is why it deserved better than the version that shipped. Three gaps:

1. It processed manifests in catalogue order, so the image one failure away from loss waited behind twenty that were merely one copy short of policy.
2. It healed at most... whatever it could reach that tick, with no cap — a fresh empty node joining a full cluster would trigger replication of *everything* at once.
3. It only replicated manifests the leader itself fully held. An image pushed to a worker node never gained a second copy. Ever.

And a fourth, quieter problem: the whole loop was an inline closure in `main()`, which is why none of the above had a failing test to its name. You can't test what you can't call. So the fix starts with extraction — the tick body becomes `pickle::replication::heal_tick(...)`, a function taking the catalogue, the blob store, the peer list, and returning the holder updates to propose to Raft. `main()` keeps the schedule, the leadership check, and the proposing; the logic becomes something an integration test can call with two in-process registries.

The gaps then close almost mechanically. `plan_heal` sorts candidates by ascending holder count — **rarest first**, the same instinct BitTorrent uses, because the copy count *is* the risk ranking — and truncates to ten per tick, so catching up is a controlled drip rather than a storm. And the leader learns to **pull before it pushes**:

```rust
// Pull-first: become a holder before replicating onward.
if !digests.iter().all(|d| store.has_blob(d)) {
    pull_manifest_layers(&digests, &manifest.repository, catalog,
                         peers, store, client, timeout).await?;
}
```

If the image lives only on a worker, the leader fetches it, records *itself* as a holder, and carries on. Images pushed anywhere now converge to redundancy — the integration test for the roadmap's "under-replicated image auto-heals when a new node joins" is finally writable, and written.

One operational trap surfaced while wiring this: the registry binds to `127.0.0.1` by default, which is exactly right for a laptop and silently fatal for a cluster — every peer addresses you as `http://<your-ip>:<registry-port>`, connects to nothing, and the heal loop logs failures forever. Bun now warns loudly at startup in cluster mode. The warning also says why we don't just flip the default: the registry speaks plain HTTP with no authentication yet, so a wider bind belongs behind the perimeter firewall's cluster-node allowlist.

### Two-phase GC, or: the check-then-act bug across machines

Garbage collection is where "delete unused blobs" meets "never delete the last copy", and the naive version has a beautiful failure mode. Say an image has two copies, on nodes A and B, and both nodes run GC at the same moment. Each checks the catalogue: "two holders — safe to drop mine." Both delete. Zero copies. Each node behaved correctly against the state it read; the *interleaving* destroyed the data. C programmers know this as TOCTOU — time-of-check-to-time-of-use — and adding machines just gives the race more room.

Locks fix this on one machine. Across machines, the shipped design routes the *decision* through the one place that already serialises decisions: the Raft state machine. GC becomes two-phase. A node *nominates* — `gc_candidates` builds the list of blobs it wants to drop, protecting tagged manifests, active deployments, sole copies, and anything younger than an hour (a mid-push blob has no holders yet and looks exactly like garbage) — and proposes a `GcReport`. The state machine, applying reports one at a time in log order, is the *approver*, and its rule is one line of arithmetic: a removal that would leave a layer with zero holders is refused. In the A/B race, both reports enter the log; whichever applies second finds one holder left and keeps it. Only after commit does a node physically delete what was approved.

Notice what did *not* need to change: the nodes still check first and act later. The race is still there. It's just that the "act" now passes through a total order, and the invariant is enforced at the single point where the order exists. That's the general shape of the fix for any distributed TOCTOU, and it's worth keeping in your pocket.

### Tests

The heal logic, being a plain function now, gets both kinds of coverage. Unit tests drive `plan_heal` against a fabricated catalogue: `audit_orders_rarest_first` (one-copy heals before two-copies; at-redundancy doesn't appear), `plan_heal_caps_work_per_tick`, `plan_heal_empty_when_redundancy_met`. Integration tests in `tests/suite/pickle_cluster.rs` run real registries on ephemeral ports: `heal_tick_replicates_to_new_peer` is the roadmap's auto-heal scenario end to end (push to node 1, node 2 appears, one tick, both hold everything and the proposed update says so); `heal_tick_pulls_missing_layers_first` proves the leader-pull path (image only on node 2; after one tick the leader holds it locally and the update records `{1, 2}`); `heal_tick_respects_per_tick_cap` pushes three images and asserts a cap of one heals exactly one.

A small fixture lesson from that last test: the shared push helper used constant layer bytes, so three "different" images shared every digest — and therefore one manifest. Content-addressed storage makes "distinct test data" something you must *construct*, not assume. The helper now varies layer content by repository name.

## Rarest first, like BitTorrent

With the catalogue in Raft and the heal loop keeping copies honest, we can finally make *pulls* fast: when a node needs an image, it should fetch layers from several peers at once instead of trickling them one at a time from whoever answers first. The interesting part isn't the downloading — it's deciding *which layer comes from which peer*, and that decision is a pure function.

```rust
pub fn plan_downloads(
    needed: &[Digest],
    local: &HashSet<Digest>,
    catalog: &ManifestCatalog,
    peers: &[Peer],
    self_node: u64,
) -> DownloadPlan
```

No I/O, no clock, no network. Everything the planner needs — who holds what — is already in the catalogue that Raft delivered. That's a deliberate design habit from earlier chapters: squeeze the decision-making into pure functions and leave thin I/O shells around them, because a pure function can be tested ten thousand times a second against inputs you'd never think to construct by hand. We'll cash that cheque below.

The plan applies four rules. **Dedup**: a digest listed twice (a config blob doubling as a layer) is fetched once. **Skip local**: anything already in the blob store is excluded. **Rarest first**: layers are ordered by ascending holder count. **Balance**: each layer goes to the holding peer with the fewest assignments so far, ties broken by node id so plans are deterministic.

Why rarest first, for a *download*? It sounds like a replication concern. The answer is what happens when ten nodes pull the same new image simultaneously — a rolling deploy does exactly this. Every completed fetch makes the fetching node a potential source for that layer. If everyone grabs the widely-held layers first, the layer with one copy stays at one copy while its sole holder gets hammered last, by everyone at once. If everyone grabs the scarce layers first, the scarce layers multiply fastest and the swarm feeds itself. BitTorrent figured this out twenty years ago; the copy count *is* the priority.

Layers that no reachable peer holds land in a separate `unavailable` list rather than an error — the caller decides whether to fall back to an external registry (the pull-through cache, next section) or fail honestly.

### Properties, not examples

The example-based tests cover the four rules directly (`plan_orders_rarest_first`, `plan_balances_across_sources`, `plan_dedups_digests`, `plan_skips_local_layers`). But a planner's failure modes live in topologies nobody writes by hand — seven peers, thirty layers, holder sets that overlap in awkward ways. That's proptest territory, and if you know Python's Hypothesis or Go's rapid, it's the same idea: describe the *shape* of valid inputs, let the framework generate hundreds of instances, and assert things that must hold for all of them.

```rust
fn arbitrary_topology() -> impl Strategy<Value = (Vec<Vec<u64>>, u64)> {
    (1u64..=8).prop_flat_map(|n_peers| {
        (
            proptest::collection::vec(
                proptest::collection::btree_set(1u64..=n_peers, 0..=n_peers as usize)
                    .prop_map(|s| s.into_iter().collect::<Vec<u64>>()),
                1..40,
            ),
            Just(n_peers),
        )
    })
}
```

A `Strategy` is a recipe for generating values — here "pick a peer count, then generate up to forty layers, each held by a random subset of those peers". `prop_flat_map` is how one generated value (the peer count) constrains the next (the subsets). When a property fails, proptest *shrinks*: it re-runs with progressively smaller inputs until it finds the minimal failing case, which is usually so small you can see the bug by staring at it.

Two properties hold for arbitrary topologies: every layer with at least one live holder is assigned exactly once (and holderless layers all land in `unavailable`), and no layer is ever assigned to a peer that doesn't hold it. The third — the balance bound — taught us something during writing. The draft property said "no peer gets more than ⌈layers/peers⌉ + 1". Generate freely and that's simply false: if one peer is the *sole holder* of ten layers, it must serve all ten, and no assignment strategy can help. The bound only holds when layers share the same holder set, so that's what the test generates — a uniform topology, where greedy least-loaded provably stays within ⌈n/k⌉. Property-based testing is good at this: it doesn't just check your code, it audits your *claims*, and it found the false one before a reviewer had to.

The parallel executor that runs these plans — bounded concurrency, retry against an alternate holder — is the next section, where the planner meets the wire.

### The executor: a JoinSet with a window

Running the plan is a classic bounded-concurrency loop, and it introduces `tokio::task::JoinSet` — the structured way to run a family of tasks. If you know Go's `errgroup` or Python's `asyncio.gather`, a `JoinSet` is the same social contract with one addition: tasks are *owned* by the set, so dropping it cancels everything still running. No leaked downloads.

```rust
loop {
    // Keep the window full, then wait for one completion.
    while in_flight.len() < concurrency {
        let Some(fetch) = queue.next() else { break };
        let store = Arc::clone(store);
        let client = client.clone();
        in_flight.spawn(async move { /* pull one layer */ });
    }
    let Some(joined) = in_flight.join_next().await else { break };
    // record success or push (digest, failed_peer) for the retry pass
}
```

Why a window (default four, `[images] p2p_concurrency`) instead of spawning everything? Backpressure. Fifty layers fired at two peers simultaneously is a self-inflicted denial of service; four in flight keeps the pipes full without the stampede. Note also what gets cloned into each task: an `Arc` of the blob store and a `reqwest::Client` (which is itself an `Arc` around a connection pool internally). Channels and tasks take ownership — cloning handles across that boundary is the normal cost of doing business, not a smell.

Failures don't abort the window; they accumulate, and a sequential retry pass afterwards tries each failed digest against its *other* holders. A digest that exhausts every holder fails the whole pull — which brings us to the most important line of the wiring.

### The seam, and the bug it turned out to fix

Where does the cluster path plug into the runtime? Inside `ImageStore::pull_and_unpack`, *before* the external registry client is built. The store gets an optional `ClusterImageSource` — a one-method trait implemented over the Pickle catalogue + planner + executor — and consults it first. Catalogue hit: layers arrive P2P, unpack, done. Miss: the existing external path runs untouched.

Except this "optimisation" turned out to be a correctness fix. Look at the external client's configuration:

```rust
let client_config = oci_distribution::client::ClientConfig {
    protocol: oci_distribution::client::ClientProtocol::Https,
    ..Default::default()
};
```

HTTPS only. Pickle registries speak plain HTTP inside the cluster. Which means an image pushed to the cluster registry *could not be deployed on any other node at all* — the pull had no path that could reach it. The P2P seam isn't making cluster deploys faster; it's making them exist. That's the second time this phase an optimisation task has flushed out a wiring hole (the port-mapping DNAT was the first), and it's worth pausing on why: optimisation work forces you to trace the *actual* data path end to end, and untraced paths are where the gaps hide.

One rule at the seam matters more than the rest. If the catalogue *knows* the image but its layers are unreachable, the pull **fails** — it must not fall through to the external path. `web:v1` in the cluster catalogue and `web:v1` on Docker Hub are different images that happen to share a name; silently substituting one for the other is how you deploy someone else's code. Errors are for when the truth is unavailable, not an excuse to guess.

Two mechanical notes. Name matching: parsing normalises `web:v1` to `docker.io/library/web:v1`, but the catalogue stores whatever the pusher put in the URL path (`web`), so the seam tries the bare name first, then the normalised one (`cluster_candidates`, unit-tested). Injection: the runtime is selected long before the registry or catalogue exist, so the source is installed *late* through a `OnceLock` slot shared by `ImageStore` clones — set once at startup, lock-free reads on every pull, and the standalone binary simply never sets it.

### Testing it, and a lesson about debug-mode crypto

The integration tests run two or three real registries on ephemeral ports: the roadmap's 100 MB five-layer pull lands in under five seconds with all blobs local; a plan against two holders provably uses both; a fetch whose planned peer is dead (a bound-then-dropped port) recovers via the alternate holder; a catalogue image with no reachable holder fails loudly; a catalogue miss returns `None` for the fall-through.

The 100 MB test failed on its first run — at 6.5 seconds, all of it CPU. Content-addressed storage verifies a SHA-256 on every write, and unoptimised debug-build SHA-256 crawls at roughly 30 MB/s; hashing dominated a localhost transfer several times over. The fix is a Cargo trick worth knowing: per-package profile overrides.

```toml
[profile.test.package.sha2]
opt-level = 3
```

Just the hash crates get compiled with optimisations; everything else keeps fast debug builds. The suite got faster across the board — every blob test was quietly paying the same tax.

## Caching other people's registries

A ten-node cluster deploying `redis:7` pulls the same 40 MB from Docker Hub ten times. That's rude to Docker Hub (which rate-limits you for it), slow for you, and pointless — the P2P machinery we just built can fan an image across the cluster from a single copy. The missing piece is getting that single copy *into* Pickle transparently: a pull-through cache. First external pull fetches from upstream and commits to the catalogue under `cache/<host>/<repo>`; every later pull anywhere in the cluster is a catalogue hit served peer-to-peer.

The entire difficulty of a pull-through cache is one fact: **tags move**. `redis:7` today and `redis:7` next month are different images under the same name. Cache it forever and you serve stale software; recheck it on every pull and you've rebuilt the rate-limit problem you came to solve. The middle path is a recheck window (`[images] cache_recheck_secs`, default an hour), and the state machine is small enough to be one pure function:

```rust
pub enum CacheState { Miss, Fresh, Stale(Digest) }

pub fn decide(catalog, cached_repo, tag, now, recheck) -> CacheState
```

`Miss` → fetch everything. `Fresh` (committed less than an hour ago) → serve the cache, touch nothing. `Stale` → the cheap move: a HEAD request for the manifest digest — a few hundred bytes — and compare with what we cached. Same digest: the tag hasn't moved, it's a `Hit`. New digest: `Refetch`. Note what `decide` takes: `now` is a *parameter*, not a call to the system clock. Time is an input like any other, which is why every path of this logic tests deterministically — no sleeps, no flaky windows.

The network side hides behind a three-method trait — `head_manifest_digest`, `fetch_manifest`, `fetch_blob` — with two implementations from day one. `OciUpstream` wraps the `oci-distribution` client we already ship (with an `insecure_http` constructor so integration tests can point it at an in-process registry). The test mock scripts digests and *counts calls* with an `AtomicUsize`, which is how you prove statements like "a stale check makes exactly one HEAD and zero blob fetches" — the pattern for testing internet-facing code with no internet.

Credentials follow the same resolve-at-startup shape as everything else in Sesame's orbit: `[images] external_registries` lists hosts with a username and a *secret name*; resolution maps names to plaintext through an injected lookup, and anything unresolvable degrades to anonymous access rather than failing the boot. Anonymous is what public registries want anyway.

### The fill path

The wiring slots into the seam we built for P2P: `pull_and_unpack` tries the cluster candidates first, and when those miss it asks the source's second method, `fetch_pull_through`. The failure semantics deliberately differ between the two. A cluster image that can't be materialised is a hard error — falling back would fetch a *different* image under the same name. A pull-through failure falls through to a direct external pull, because the identity upstream is the same either way; degrading is safe and gets logged.

Inside, the fill is the read-through shape every cache tutorial draws, with two guards worth naming. First, concurrent misses: a deploy of ten replicas lands ten pulls of the same new image at once, and without care they all download it from upstream. A `tokio::sync::Mutex` serialises fills, and — the part people forget — the winner's followers *re-check the cache after acquiring the lock*, because the image they were queueing to fetch is usually there by the time they hold it. One lock for all images, not per-image locks: the simplest correct thing, and contention is a deploy-time blip.

Second, the degradation rule. A stale entry whose upstream HEAD *fails* (registry down, rate-limited, DNS broken) serves the stale copy rather than failing the deploy. Availability over freshness — stated in the code, not buried in an error path.

Cached images are unsigned — they're upstream content, and we were never in a position to sign them. Under `require_signatures` they must still deploy, and it turns out the exemption needs no code at all: the scheduler's manifest lookup strips an image reference to its last path segment, which can never match a `cache/<host>/<repo>` repository. An exemption by construction is still a policy, though, so a test pins it (`check_image_schedulable_exempts_pull_through_cache`) — the difference between "it happens to work" and "it's guaranteed to keep working". Upstream trust (digest pinning, verifying upstream cosign signatures) is future work, and the code says so.

### What the integration test flushed out

The headline test stands up an in-process registry as the "upstream" with a request counter, fills the cache from node A, copies the catalogue to node B (hand-simulating Raft), and pulls from B with A as the peer — asserting the counter *doesn't move*. First pull caches, second pull is served entirely by the cluster. Two more tests cover the switches: `pull_through = false` is a clean fall-through, and a stale cache with a dead upstream serves stale.

The first run failed, and the failure was a gift: multi-segment repository names. Real OCI names contain slashes (`library/nginx`, and our own `cache/<host>/<repo>` always does), but the registry's axum routes match `/v2/{name}/...` with a *single* path segment — so peer blob transfers for cached images 404'd on routing before any handler ran. The fix leans on content addressing: blob endpoints ignore the name entirely (a blob is its digest), so peer transfer URLs simply flatten the name (`cache/docker.io/library/redis` → one segment) and nothing round-trips through the flattened form — manifests travel via Raft, never over these URLs. Full multi-segment routing in the registry API is real future work; this is the honest minimum that makes the cache correct today.

## Volumes that actually mount

This section is short and slightly embarrassing, which is exactly why it's in the book. Since Phase 1, Reliaburger has had a `VolumeManager`: it creates a managed volume's host directory, and on Linux it can wrap it in a size-enforced loop-mounted ext4 filesystem. Well-designed, unit-tested. The July review found it had **no callers**. The OCI spec generator computed the bind-mount *paths* for managed volumes, runc dutifully tried to mount them — and failed with ENOENT, because nothing had ever created the directories. Every containerised app with a managed volume had been failing to start, forever, while eight unit tests passed.

The fix is a dozen lines in the agent's startup path: before the OCI spec is generated, filter the app's volumes to the managed ones (no explicit `source` — host-path volumes are the operator's own business), and run `create_managed_volume` for each inside `spawn_blocking` (it does filesystem work and, on Linux, shells out to `fallocate`/`mkfs`/`mount`). Failure fails the deploy closed — a container without its volume shouldn't limp up and write data into an unmounted path. And the `[storage] volumes` config key, parsed-but-ignored until now, finally reaches the agent instead of a hardcoded default.

The deliberate *non*-feature deserves more words than the feature. Volumes are **never deleted on Stop**. It's tempting — create on start, delete on stop, symmetric and tidy. But look at who sends Stop: users, yes, and also the placements reconciler every time an assignment moves between nodes during a routine rebalance, and the self-upgrade machinery around a binary swap. Delete-on-stop turns "the scheduler moved your database replica" into "the scheduler deleted your database". Orphaned volume trees are the cost, an explicit `relish volume rm` (future work) is the answer, and the asymmetry is the point: creating data automatically is safe, destroying it automatically is not.

What actually catches a library-not-wired bug? Not more unit tests of the library. The new tests drive the *agent*: deploy a config with a managed volume through the real command channel and assert the host directory exists afterwards; deploy a host-path volume and assert nothing appeared under the managed root. The test that would have caught M21 on day one is the one that starts from the user's artefact — the config file — and checks the world, not the code.

## Quotas without loop devices

How do you give a directory a size limit? Filesystems don't do that — a directory is just a namespace. The loop-mount trick from Phase 1 answers by making the directory *its own filesystem*: `fallocate` a sparse 10 GiB file, `mkfs.ext4` it, `mount -o loop` it over the directory. Writes past the limit fail with ENOSPC because the filesystem is genuinely full. It works, and it's clunky: three shelled commands, a `.img` file to manage, a mount to remember to unmount, and a filesystem-in-a-file's overhead.

Btrfs makes the whole trick unnecessary, because Btrfs directories *can* be filesystems, almost. A **subvolume** looks like a directory, costs nothing to create, and is independently snapshottable; a **qgroup limit** bounds how much data it can reference. Volume creation becomes `btrfs subvolume create` plus `btrfs qgroup limit 10485760 <path>` — no image file, no mkfs, no mount table entry. Writes past the limit fail with EDQUOT instead of ENOSPC; same kernel-level enforcement, cleaner mechanics.

The code follows the pattern this chapter keeps using: pure argv generators and one pure decision function, with a thin Linux shell layer. The decision is a three-bool truth table — is the volumes directory on Btrfs, are we Linux root, is there a size limit — and Btrfs wins *even without a size limit*, because only subvolumes can be snapshotted and the next section needs that. `is_btrfs` asks the kernel directly: `statfs` returns a filesystem type id, and `0x9123_683E` is Btrfs's magic number. That's the nix crate wrapping a syscall — the first time this book has read a filesystem's identity rather than its contents.

Which backend a volume got matters later — a loop mount must be unmounted, a subvolume needs `btrfs subvolume delete`, and `rm -rf` is wrong for both — so creation records it in a sidecar file *next to* the volume (inside it would leak a stray file into the container's mount). The sidecar bought an unplanned fix: creation is now **idempotent**. Instance restarts re-drive the startup path, so `create_managed_volume` runs again on every restart — and until now a restarted sized volume would loop-mount *again*, stacking mounts on each restart. Sidecar present → already provisioned → return. The bug predates this phase; the refactor surfaced it.

Two test notes. The quota integration test provisions its *own* filesystem — `truncate` a 1 GiB file, `mkfs.btrfs`, loop-mount it in a tempdir — so it assumes nothing about the host's disks, runs under the Lima gate (`RELIABURGER_BTRFS_TESTS=1`), and asserts an 11 MiB write into a 10 MiB volume fails at write *or* sync (Btrfs buffers; enforcement can arrive at either point — check both or flake). And the backend decision function briefly had swapped test arguments — three positional `bool`s in a row is an API that invites exactly that; the truth-table tests caught it before it shipped, which is their whole job.

## Point-in-time for free

Here's the payoff for insisting on subvolumes in the last section. A Btrfs snapshot is not a copy — it's a new subvolume whose metadata points at the same extents as the original, created in constant time regardless of size. Copy-on-write does the rest: as the live volume changes, only the changed blocks diverge. A 50 GiB database snapshots instantly and costs only what subsequently changes. That's why "snapshot" was never going to mean `cp -r` here, and why the non-Btrfs answer is a loud `UnsupportedFilesystem` error rather than a fallback: an rsync masquerading as a snapshot is slow, inconsistent under writes, and teaches operators the wrong expectations. Honest error beats fake feature.

The mechanics are two argv builders away. Create is `btrfs subvolume snapshot -r <live> <dest>` — `-r` for read-only, because a backup you can accidentally write to isn't a backup. Restore is the same command in reverse *without* `-r` (the restored volume must be writable), preceded by `btrfs subvolume delete` of the live volume. Snapshots live under `{volumes_dir}/.snapshots/{ns}/{app}/{volume-slug}/{name}` with a `meta.json` beside each; names default to unix seconds from an injected clock (tests pass a fixed time, so naming is deterministic — no wall-clock in test assertions, ever).

Restore's precondition does not belong to the filesystem layer. Swapping a subvolume under a running container corrupts the workload's view of its own data, so *the agent* — the thing that knows what's running — refuses to restore while any instance of the app is non-terminal, with a 409 at the API. The guard fires before any filesystem check, which has a pleasant testing consequence: the refusal tests on macOS with the mock runtime, no Btrfs required. Layering by knowledge: the snapshot manager knows filesystems, the agent knows workloads.

One design choice worth defending: `snapshot create` with no `--volume` snapshots *every* provisioned volume of the app — discovered from the E1 sidecars on disk, not from the app's spec. The filesystem is the source of truth, which means you can snapshot (and restore) an app that's stopped, deleted from config, or mid-migration. The API is four routes under `/v1/snapshots/`, the CLI is `relish snapshot create|list|restore|delete`, and the roadmap's acceptance test runs verbatim on the Lima loopback-btrfs rig: write `v1`, snapshot, overwrite with garbage, restore, read `v1` back.

### Scheduled sweeps, and the checkpoint that is also the retry policy

Manual snapshots are for the moment before a risky migration; backups are a *schedule*. The scheduler here is deliberately boring: `[storage.snapshots] interval_secs` drives a `tokio::time::interval` loop, the same house pattern as every other background worker in Bun. The roadmap said "cron", and we considered a real cron-expression parser — and rejected it, because it drags in a date-time stack for expressiveness nobody asked for by name. An interval loop satisfies "snapshots happen on a schedule"; if someone ever needs "3 a.m. on Sundays", that's the day the dependency earns its place. Write the trade-off down and move on.

The loop's tick is a plain async function taking the volumes directory, the retention count, an optional uploader, and — as always — `now` as a parameter. Three phases. *Snapshot* every provisioned volume (discovered from sidecars). *Prune* to the newest `retain` per volume — the plan for which is a pure function, tested with fabricated metadata. *Upload* anything not yet shipped: tar + gzip the snapshot directory on a `spawn_blocking` thread (compression is CPU work; the runtime's threads are for I/O), then `put` it through `object_store` — one trait over `file://`, `s3://`, and `gs://`, which is why the integration test can prove the whole upload path against a tempdir URL and the production S3 path differs only in configuration.

The nicest design element is the smallest: each snapshot's metadata has an `uploaded: bool`, flipped after a successful put. That single flag is simultaneously the **checkpoint** (a sweep never re-ships what's shipped — asserted by running the sweep twice and counting), the **retry policy** (a failed upload leaves it false, so next tick tries again — no retry queue, no backoff state machine), and the **audit trail** (`relish snapshot list` shows an UPLOADED column). When one bit of state can serve three masters, the design is probably right.

Failures accumulate into the tick's report rather than aborting it — one app's broken volume must not stop another app's backup — and the tests exercise exactly that: a sweep over unsnapshotable volumes (every macOS dev machine) reports errors and carries on.

### Whose volume is it?

A static review of the 0.1.0 release found the hole in all this. The API checked the token against the route (`POST /v1/snapshots/a/web` needs a Deployer allowed to touch `a/web`) and then handed the JSON body straight to the snapshot manager. The manager stripped a leading `/` from `volume` and joined the rest under the app's directory. So a token scoped to `a/web` could send `{"volume": "/../../b/db/data"}` and snapshot `b/db`'s volume. Worse, a later restore of that snapshot would swap `b/db`'s live data, and the "is the app running?" guard only looked at `a/web`. The custom `name` was no better: `Path::join` with an absolute path *replaces* the base (a Python `os.path.join` habit that Rust shares), so `"name": "/anywhere"` put a root-owned subvolume wherever the caller liked.

Path joining is where traversal bugs live, and the fix is not to sanitise the string harder. It's to stop building paths from request text at all. The volume a request names is now *looked up*, not joined. `resolve_volume` normalises the requested mount path, compares it with the app's inventory (`VolumeManager::provisioned_volumes`, rebuilt from the sidecars on disk), and returns the inventory's copy of the string. User input chooses *which* of the app's volumes; it never becomes part of a path. The normaliser walks `Path::components()`, an iterator over an enum with variants for the root, `.`, `..` and plain names, and a `match` refuses anything but `RootDir` and `Normal`. One wrinkle: `components()` quietly drops a `.` in the middle of a path, so `/./data` would sneak through as `/data`. The check reads the raw text for that case too.

Names get an allowlist rather than a blocklist: 1 to 128 bytes from `[A-Za-z0-9._-]`, not starting with a dot. That's a single path component by construction. Namespace and app must be the lowercase DNS labels config validation already demands, so neither can be `..` or reach the `.snapshots` bookkeeping directory. Every failure is a new `SnapshotError::InvalidInput`, which the API maps to 400, and every check runs before the first `read_dir` or `mkdir`. The tests assert exactly that: they snapshot the whole temporary directory tree before a batch of hostile requests and compare it afterwards.

Two quieter escapes turned up along the way. The inventory walk used `Path::is_dir()`, which follows symlinks, and it descended into the volumes themselves. Volume contents belong to the container, so a container could plant `x.volume.json` inside its own volume, or a symlink to `/`, and grow its app's "inventory". The walk now asks the directory entry for its own type (`DirEntry::file_type()` doesn't follow links) and never enters a provisioned volume. The other escape was persisted metadata: restore, delete and upload all act on what `list` returns, so `list` now trusts a `meta.json` only if its fields would have filed it exactly where it sits.

The same review caught an identity bug. A multi-volume snapshot deliberately gives every volume the same timestamp, but `find` looked up by name alone and took the first match. Restore restored whichever volume the directory listing produced first. Worse, the retention sweep deleted by name, so pruning `/data`'s old `100` could delete `/wal`'s newest backup instead. `find` now takes an optional volume and refuses a name that matches more than one (`SnapshotError::Ambiguous`, a 409 that lists the candidates). `relish snapshot restore` and `delete` gained `--volume`, and the sweep always passes the volume of the snapshot it means. Rust's `Option::is_none_or` reads well in the filter: keep a snapshot if no volume was asked for, *or* if it matches the one that was.

And one line of config validation: `retain = 0` with a schedule set is now refused at startup. At the time each sweep pruned before it uploaded, so retaining nothing deleted every snapshot the moment it was taken.

### Which node's copy?

An external test of 0.1.2 on three EC2 nodes found the next hole, and it had nothing to do with paths. `volapp` ran on node-03. The tester sent `relish snapshot create volapp` to each node's API in turn, and all three said yes. Only node-03's snapshot held the live data. Node-01 and node-02 had snapshotted stale copies of the volume, left behind when the app had moved earlier (#423), and each node's `list` showed only its own. A node with no copy at all answered "not a managed volume". So a "before-upgrade" snapshot could quietly miss the data it was meant to protect.

The cause was one line in each of the four handlers: `ask_agent(...)`, straight to the local agent. The agent resolves the volume from its own disk inventory, and any node with a leftover directory passes that check. The disk can't tell a live volume from an orphan. The council can, because since #423 it records where every managed-volume app lives: its placements, or for a stopped app, the nodes it last ran on (`last_placed_nodes`), so it can go back to its data.

So the handlers now ask the council first. `volume_homes` in `cluster/orchestrate.rs` turns that record into a list of nodes, less any decommissioned one, and it rides along in the desired-app evidence every node can already read (a non-leader forwards that read to the leader). Then a small pure function picks the route:

```rust
pub(super) fn snapshot_route(self_name: &str, homes: &[String]) -> SnapshotRoute {
    match homes {
        [] => SnapshotRoute::Here,
        _ if homes.iter().any(|home| home == self_name) => SnapshotRoute::Here,
        [home] => SnapshotRoute::Forward(home.clone()),
        _ => SnapshotRoute::Ambiguous(homes.to_vec()),
    }
}
```

That `match` uses *slice patterns*, which C and Go don't have. `[]` matches an empty slice, `[home]` matches a slice of exactly one element and binds it, and `_ if ...` is a pattern with a guard: it matches anything, but only when the condition holds. Arms are tried in order, so the guard sees every non-empty list before `[home]` does. No volume home on record (a standalone node, an app the council doesn't know) means answer here, as before. A node that holds one of the copies answers for its own replica. One other home means forward. Several other homes means there's no single right copy, so the request is refused with a 409 that names them.

Forwarding works like the fault routes from Chapter 8. The request goes on with the caller's own `Authorization` header, so the volume's node repeats every role and scope check, plus an `x-reliaburger-snapshot-forwarded` header. A request carrying that header is always answered where it lands, which means two nodes that briefly disagree about where a volume lives can't bounce it between them forever.

The test is the issue's reproduction on a fake three-node cluster in `cluster_routing_tests.rs`: real routers on loopback, a scripted agent per node, `db`'s volume on node-2. It runs create, list, restore and delete through node-1 and node-3 and checks that only node-2's agent saw them, and that the listing came back with node-2's snapshots. Before the fix, node-1 listed its own.

### Owning the volume, surviving the crash

The same review had four more findings about snapshots, and they share a theme: each check was right at the moment it ran and wrong a moment later.

Take restore. The agent checked that no instance of the app was running, handed the restore to `spawn_blocking`, and went straight back to its command queue. The very next command could be `relish apply` for the same app, and nothing stopped the deploy from mounting a volume the restore was halfway through renaming. A second restore of the same app would reuse the same `.restore-staged` and `.restore-old` paths at the same time. The check was sound. It just didn't *own* anything.

So the agent now takes a reservation before it dispatches the work. `VolumeMaintenance` is a map from `(namespace, app)` to what's holding the app's volumes, and the interesting part is how a reservation ends. The obvious design is a flag the task clears when it finishes. But what if the task panics, or returns early on an error path somebody adds next year? The flag stays set, and the app can never be deployed again. We'd rather the compiler ended the reservation for us, and Rust has the tool for it. `Arc<T>` is a reference-counted pointer: cloning it bumps a count, dropping a clone lowers it, and the value is freed at zero. `Arc::downgrade` gives you a `Weak<T>`, a pointer that *doesn't* keep the value alive; `Weak::strong_count` tells you how many real owners are left. The blocking task gets a `VolumeLease` wrapping an `Arc<()>` (an `Arc` of the empty tuple, which carries no data, only the count), and the map keeps the `Weak`. The reservation is live while `strong_count()` is above zero. When the task ends, however it ends, the lease is dropped and the reservation disappears with it. If the HTTP caller gives up and hangs up, nothing changes: the lease lives in the task, not the request. And because only the agent loop touches the map, there's no lock. Go programmers will recognise the shape of a `sync.WaitGroup`, except that forgetting to call `Done` is impossible here.

With the lease in place, a restore refuses a second restore (`SnapshotError::Busy`, a 409), `relish apply` for that app is refused with "being restored", storage preparation refuses too (for a deploy that was accepted a moment *before* the restore), and a crashed instance waiting for its automatic restart simply waits a little longer. One more hole: the old "is anything running?" check treated a stopped instance with a pending restart as not running. It counts now.

The test is the review's regression, nearly word for word. A test-only `std::sync::Barrier` pauses the restore after the agent has accepted it; while it's paused, the test sends a deploy and a second restore, sees both refused, and checks that the volumes directory is still empty. Then it releases the barrier, the restore resolves, and the same deploy succeeds.

The lease had one more lesson in it, and a release build taught it to us. Each blocking task started with `let _lease = lease;`, moving the lease into a local so it would live for the whole task. (Careful with that underscore: `let _lease = ...` keeps the value until the end of the scope, while `let _ = ...` drops it on the spot. One character, very different behaviour.) The task did its work, sent the answer down the oneshot channel, and only then reached the closing brace, where Rust drops locals. That's a gap. The HTTP handler wakes on the answer, the client gets its response, and the client's next request can reach the agent loop before the blocking thread has run those last few instructions. On a busy CI runner, a test that sent a create and then a delete got `409 busy` on the delete, refused by the very operation it had just watched finish. Any client scripting create-then-delete would have hit the same thing. The fix is to be explicit about order: the task calls `drop(lease)` (the standard library function that ends a value's life right there, instead of at the closing brace) and then sends the answer. Nothing can overlap the work, because the work is already done. The regression test makes the gap deterministic: a test-only hook parks every snapshot task *after* it has answered, so a lease still alive at that point is always seen by the next request. It failed against the old order on the first run. Scope-based cleanup is lovely until the scope is a little longer than you thought.

Owning the volume doesn't help if the owner dies. Restore builds the replacement next to the live volume, renames the live one to `.restore-old`, then renames the replacement into place. Each rename is atomic; the pair isn't. A crash between them left no live volume at all, and the next restore attempt began by "cleaning up leftovers", deleting `.restore-old`, which by then was the only copy of the data. Nothing on disk said which copy was the real one.

Now a journal does: `<volume>.restore.json`, written durably *before* each step it names. There are two phases. `Staging` means the replacement is being built and the live volume is untouched; `Swapping` means the replacement is complete and the renames have begun. Recovery reads the phase and checks which of the three copies exist, and a pure function, `plan_restore_recovery`, decides what to do. Three phases (counting "no journal") times three yes-or-no copies is a small enough table to test case by case, and the test does. If the journal says `Swapping`, the live name is empty and both copies exist, the crash hit between the renames, so recovery finishes the swap. If only the original survives, it goes back. Anything the table can't explain (copies with no journal, a missing original) is `Ambiguous`: recovery keeps every copy and the journal, and says so. Bun runs recovery at startup before it adopts any workload, and every restore runs it first. While a journal is present, `create_managed_volume` refuses to mount the volume. An app that can't start is a problem you'll notice; an app that starts on an empty directory is one you'll notice later, and worse.

How do you test a crash? The Btrfs test (it runs as root on CI's privileged Linux job) gives the snapshot manager a test-only `crash_at` point. At that point `restore` returns immediately, skipping every cleanup path, exactly as if the process had been killed. The test does this at each of the six boundaries, then runs recovery with a fresh manager and checks three things: the volume holds either the original or the restored data, complete; there are no stray copies or journal; and a fresh restore still works. It's an honest simulation of a killed process. It isn't a power cut, which is why every rename is followed by an `fsync` of the parent directory (the rename lives in the directory, not the file).

The review also asked an open question: does a restored volume keep its quota? It didn't. A Btrfs qgroup limit belongs to one subvolume, and restore swaps in a new one. The volume's sidecar now records the limit it was created with, and restore sets it on the replacement before the swap.

### Two nodes, one bucket

Export had subtler problems. The archive key was `<namespace>/<app>/<volume>/<name>.tar.gz`. Run two replicas of an app on two nodes, point both at the same bucket, and have their sweeps snapshot in the same second: same key, different bytes, and whichever finished second silently replaced the other. Reusing a custom name did the same thing on one node.

The key now carries the node's name and the volume, and the archive itself is named by the SHA-256 of its bytes: `<ns>/<app>/<node>/<volume>/archives/sha256-<digest>.tar.gz`. Content addressing makes collisions boring. If the key already exists with the same size, it's the same archive and the upload is skipped; a different size is a refusal, never an overwrite. The human-facing details (snapshot name, creation time, node, volume) go in a JSON manifest beside it, keyed by creation time, name and digest. The test is the review's: two independent volume directories, same app, same volume, same timestamp, one in-memory store. Both archives must come back with their own bytes.

Then the receipt. Remember the `uploaded: bool` that served three masters? It served them for exactly one destination. Change `upload_url` from A to B and every snapshot already sent to A looked uploaded, so B never got them. Worse, the sweep pruned *before* it uploaded, whatever the flag said, so an object-store outage longer than the retention window deleted snapshots that had never left the node. One bit wasn't enough state after all.

The flag became a list of receipts, one per destination: the destination's URL (minus credentials, so rotating keys isn't a new destination), the archive and manifest keys, the digest, the size and when it completed. "Needs exporting" now means "no receipt for *this* destination". The sweep uploads first and prunes second, and `prune_plan` holds back anything the current destination hasn't confirmed. We looked at letting disk pressure override that and decided against it for now: silently deleting unexported backups is exactly the bug we were fixing. Held snapshots are counted in the sweep's report and logged instead, so a long outage shows up as a growing number rather than a surprise. This mirrors what Ketchup's log export already did with its acknowledgements (Chapter 6).

Last, memory. The uploader built the whole `.tar.gz` in a `Vec<u8>` before calling `put`. `spawn_blocking` kept the compression off the async threads, but a 50 GiB volume of already-compressed data would still have wanted 50 GiB inside Bun. The archive now streams to a spool file in `.snapshot-spool` on the volumes filesystem, through a small writer that hashes and counts the bytes as they pass. That writer is also where the limits live: it returns an error once the archive would cut into the filesystem's reserve (5% of its size, at most 10 GiB), once the stage's deadline passes, or once Bun starts shutting down. The upload then reads the spool back one 8 MiB part at a time through `object_store`'s multipart API, so one part is all it ever holds. The whole upload has a deadline (`upload_timeout_secs`), and a cancelled or timed-out upload is aborted, so S3 doesn't keep the orphaned parts. `NamedTempFile` deletes the spool file when it's dropped, success or failure.

Testing this without gigabytes of fixtures means measuring the right thing. The large-archive test uploads 3 MiB of incompressible bytes with a 256 KiB part size and asserts the uploader never held a part bigger than that. A stalled destination is `object_store`'s own `ThrottledStore` with an hour-long delay per write: the sweep must give up within its deadline, write no receipt, and ship on the next sweep. Cancellation and a full spool get the same treatment.

The quota itself hid a test that depended on the machine running it. At first the writer's limit came straight from `statvfs` on the real spool directory, and the reserve was a flat 5% of the filesystem. On a developer's 926 GiB laptop disk with 35 GiB free, 5% is 46 GiB, so the quota came out as zero and eight upload tests failed with "0 byte spool quota" while CI, on roomier disks, stayed green (issue #524). Two things were wrong. The tests read the host, so their outcome depended on whoever ran them. And the rule was too strict on large disks: refusing to spool 1 GiB when 35 GiB are free protects nothing.

The fix splits the reading from the rule. `spool_quota` is now a pure function of a `DiskSpace { available, total }`, so its edge cases are plain unit tests with no filesystem at all. The reserve is 5% or 10 GiB, whichever is smaller: a small disk keeps its 5%, and a big one keeps 10 GiB, which is plenty of headroom for logs and metadata on the volumes filesystem. The reading comes from a field on the uploader, `spool_space: fn(&Path) -> Result<DiskSpace, String>`, a plain function pointer like the ones in Chapter 1. `from_url` sets it to `host_disk_space`, which calls `statvfs`; the test helpers set it to a fixture that reports a roomy disk. A function pointer suits this better than a trait: there are only two kinds of reading, neither needs state, and `fn` is `Copy` and `Send`, so it moves into the `spawn_blocking` closure without an `Arc`. A new test runs the same export against a fixture disk with nothing free, where it fails with "0 byte spool quota", and then against the roomy one, where it succeeds. Whatever the host disk holds, the tests see only the fixture.

One smaller fix rode along. Snapshot directories were named by flattening the mount path, `/var/lib` to `var-lib`, so `/a/b` and `/a-b` both became `a-b` and an app with both volumes mixed their snapshots. The slug now escapes only `%` and `/` (percent-encoding style), which is reversible, so two paths can never share one. And `list` stopped quietly skipping what it couldn't read. A truncated `meta.json` or an unreadable directory is now an error, so "no snapshots" and "couldn't read the snapshots" no longer look alike.

These changes alter the on-disk layout, so the state generation went up: a 0.1.1 node refuses 0.1.0's state, and you start a fresh cluster. Before 1.0 that's the policy (Chapter 14), and it spared us a migration nobody would have tested properly.

## A thousand jobs, fifty envelopes

The `/v1/batch` endpoint has returned 501 since Phase 8, with the batch scheduler and tracker sitting library-complete beside it. Wiring it forced the one design question this chapter had left open: *how does the leader tell another node to run something?*

The obvious answer is "the same way deploys do". Stage 4 gave deploys a Raft-placements pipeline: the leader writes desired assignments into the replicated log, and every node runs a reconciler that polls its assignments and converges — starts what should run, **stops what shouldn't**. That machinery is exactly wrong for run-to-completion jobs, and the reasons are worth spelling out. A finished job looks like *drift* to a reconciler ("assignment says run, node says stopped — restart it!"). A rebalanced assignment means *stopping* the workload on the old node — which for a job means killing work half-done, not gracefully moving a stateless replica. Reconciliation is the right tool for "keep this running"; it is the wrong tool for "run this once". So batch dispatch is direct HTTP: the leader POSTs each node's job group to `/v1/batch/run` (service-token authenticated), and running nodes post per-job completion reports back to a callback URL. Same crate, two dispatch idioms, each matching its workload's semantics — that contrast is the section's real lesson.

The rest reuses what earlier phases built. Submissions carry *full job specs* (the old CLI sent only names, which required the cluster to already know the jobs — a genuine design bug found by wiring). Followers forward submissions to the leader with the same proxy shape as `/v1/apply`. Capacity comes from the reporting pipeline's aggregated worker reports — the very data the deploy scheduler uses — mapped into the bin-packer's `NodeCapacity`; a standalone node falls back to a single effectively-unbounded local entry, which is also what makes the integration tests run without a cluster. The `BatchTracker` lives leader-side, and a target node runs its share by synthesising a `Config` containing just those jobs and pushing it through the ordinary deploy path — retries, records, and all.

### The watcher, and an ambiguous "stopped"

Each running node watches its jobs to a terminal state by polling instance status. The first version of that watcher had a bug the failure-path test caught immediately: it treated `stopped` as success. But the runtime maps *any* process exit to `Stopped` — the exit code is tracked separately — so a job that exits non-zero shows as `stopped` while it waits out its retry backoff, and the watcher happily reported it completed. The fix exposes the exit code in `InstanceStatus` (long overdue; the TUI will want it too) and makes the predicate honest: `failed` is failure, `stopped` with exit 0 is success, `stopped` with a non-zero code is *backoff, keep waiting* — the agent will mark it `failed` once retries exhaust. States that look terminal and aren't are a classic distributed-systems trap; this one was two feet from home.

The first version of completion reporting was deliberately forgiving: reporting into an unknown batch id was a recorded no-op, because a leader restart lost the in-memory tracker and late reports from running nodes must not explode. That was a trade we wrote down at the time — batch status was best-effort across leader changes — and it didn't survive the 12b review. Forgiving turns out to be another word for forgeable, and "best-effort across restarts" is another word for lost. The section below ("When the leader forgets") replaces both halves. `relish batch` prints the batch id; `relish batch-status <id>` shows the live summary, and `--wait` polls it to a terminal state.

## Building where the tools are

The build endpoint arrived with the wiring merge in a working-but-synchronous form: fetch the context blob, run `buildah bud` and `buildah push`, answer when done. Its own comment admitted the flaw — it "matches the CLI's 300 s client timeout". Real image builds routinely take longer, and a build that outlives the HTTP request strands the client mid-response with the build still running. Client timeouts are a design forcing function: any operation that can exceed them must become *submit-and-poll*, and pretending otherwise just moves the failure to the worst possible moment.

The refactor keeps the handler body — it was correct — and lifts it into a spawned runner behind a registry of build states: `POST /v1/build` answers `202 { build_id }` immediately, `GET /v1/build/{id}` reports `running`, `completed`, or `failed` with the last useful stderr, and each `buildah` stage now sits under `[images] build_timeout_secs` so a hung build terminates instead of squatting forever. `relish build` polls (with a bound — more on that later). The first cut of this tracker was a process-local `HashMap`, on the argument that a build lives on the node that accepted it and there's nothing to aggregate. Half right: there's nothing to aggregate, but there's plenty to *lose*, and the durability section below moves the records into Raft.

The second gap was placement. A macOS node — or any node without `buildah` — used to answer 501. Now capability travels with the state reports: each worker probes `buildah --version` once at startup and carries a `has_buildah` flag in every report, and a submit on an incapable node *delegates* — it picks a capable peer from the aggregated reports, forwards the request to that peer's `/v1/build/run`, and records the build locally as `Delegated { url, remote_id }` so status reads proxy through. The client polls one node and never learns the build ran elsewhere. No builder anywhere is an honest 503 naming the actual problem.

That delegation path is also a trust boundary, and the first cut got it wrong. `/v1/build/run` receives a `context_digest` from another node and later joins it into a temp directory to unpack the context tar — as a privileged process. Nothing validated it, so a digest of `sha256:../../something` (the `replace(':', "-")` that sanitises the colon leaves the `../` untouched) would escape the build sandbox. The fix is to treat the digest as the untrusted input it is: `Digest::new` already enforces `sha256:` + exactly 64 hex characters, and `.` and `/` are not hex, so validating the digest the moment the request arrives makes the traversal unrepresentable. The general lesson for any delegated endpoint: an identifier you received over the wire and are about to turn into a filesystem path is attacker-controlled until proven otherwise — validate its shape before it touches the filesystem, not after.

There was a second, sharper trust problem in the same neighbourhood, and it's worth dwelling on because it's the kind of bug that hides in plain sight in a distributed system. The node-to-node endpoints — `/v1/batch/run`, `/v1/batch/{id}/report` and `/v1/build/run` — had no authorization at all. They sit behind the general auth middleware, which during the bootstrap window (before the first token exists) waves everything through, and even afterwards only checks that *a* valid token is present, not *which* principal it is. So a ReadOnly token — or an anonymous caller against a node bound beyond loopback — could POST a batch of jobs and make the node execute them. Worse: the completion callback attaches the cluster **service token** (the `__system` principal, Admin-equivalent everywhere) to a `callback_base_url` taken straight from the request body. Point that URL at your own server and the node hands you the keys to the cluster.

The fix has two layers. First, these are node-to-node routes, so they now require the **system principal** specifically — a new `require_system` guard that accepts only a caller presenting the service token, and unlike the role check it refuses the bootstrap-window `None` outright, because internal endpoints are never part of first-run setup. That alone closes the hole: only a node that already holds the service token can trigger a callback, and it sets the callback to a real peer. Second, defence in depth: before sending the token, the node checks `callback_base_url` against its membership table and refuses a destination that isn't a known member — so even a *compromised* node can't redirect another node's token to an arbitrary URL. The submit endpoints (`/v1/batch`, `/v1/build`) got the ordinary Deployer check they were always missing. The lesson worth carrying: "authenticated" is not "authorized," and an internal RPC that trusts a caller-supplied URL with a credential is a credential-exfiltration primitive dressed up as a convenience.

Completion closes the trust loop from Phase 10: after the push lands the image in the local registry (real holders, catalog persistence, Raft propose — all through the standard push handlers), the runner looks up the manifest digest and signs it. The first version of that signing was best-effort with an ephemeral key, which sounds harmless and wasn't — the durability section below explains why, and makes signing part of the definition of a completed build when the trust policy demands it. The full circle is worth saying out loud: **build → push → replicate (heal loop) → sign → verify at schedule → deploy → serve**, every arrow now a real code path. That was the point of the phase.

### Hardening the builder: server-owned destinations and a tar you don't trust

The digest fix above was the first half of the trust story. The second half came out of a harder look at the same handler, and it's a nice case study in what "the server owns the truth" means in practice.

Look at what the build request used to carry: a `registry_port`. The caller picked the port, and the node fetched the context from `localhost:<that-port>` and pushed the result there. Think about what that hands an attacker who can reach the endpoint. A privileged process, told to go connect to any port on the loopback interface and unpack whatever tar it finds there. That's a server-side request forgery primitive wearing a config field's clothes. The port was never the caller's to choose. Every node already knows its own registry port from `[images] registry_port`, so the fix is to delete the field and read the config. To make sure nobody sneaks it back in through a stale client, the request struct gets one serde attribute:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSubmitRequest {
    pub name: String,
    pub context_digest: String,
    pub spec: crate::config::build::BuildSpec,
}
```

`deny_unknown_fields` tells serde to reject any JSON key it doesn't recognise instead of quietly ignoring it. A body that still carries `registry_port` is now a `400`, not a silently-dropped field. The principle generalises: for anything that names a destination the server already knows — a port, a callback URL, a registry address — the request body is the wrong place to learn it. Server-owned beats caller-supplied every time a credential or a privileged action is downstream.

Then there's the tar. The old code did the most natural thing in the world:

```rust
tar::Archive::new(&context[..]).unpack(&extract_dir)?;
```

One line, and every one of its assumptions is wrong for input you got over the network. It buffers the whole body in memory first (a multi-gigabyte context is a multi-gigabyte allocation). It trusts entry paths, so an entry named `../../etc/cron.d/x` writes outside the directory. It follows the archive's idea of symlinks, so a link to `/` plus a later write walks straight out of the sandbox. It honours the mode bits, so a `04755 root` file lands as a setuid binary. And a sparse-file entry can claim to be a few kilobytes in its header while expanding to fill the disk. None of that matters for a tar *you* wrote. All of it matters for a tar a stranger sent you.

The replacement, `unpack_context`, treats the archive as hostile. It streams the download to disk against a hard byte cap (`[images] max_context_bytes`, default 256 MiB) so memory stays bounded whatever the body claims. It walks entries by hand and, for each one, rejects anything that isn't a plain file or a directory — no symlinks, no hard links, no devices, no FIFOs. It maps each path through a function that keeps only "normal" components and throws out anything absolute or containing `..`. It counts the bytes it actually *writes* (not the sizes the headers advertise, which is what defeats the sparse-file trick) and stops at the cap. It caps the entry count. And on Unix it masks the mode down to the `rwx` bits, so the setuid bit can't survive the trip. The Dockerfile gets the same suspicion: `confine_dockerfile` canonicalises the resolved path and asserts it still starts with the context directory, which catches both a literal `../../outside` and the sneakier "swap a subdirectory for a symlink" version.

One more thing had to change, and it's the sort of bug you only see when you think about failure. Each `buildah` stage runs under a timeout:

```rust
tokio::time::timeout(timeout, child.output()).await
```

When that timer fires, the future is dropped. Dropping a future does not kill the process it was waiting on. And even if it did, `buildah` spawns children of its own, which would be orphaned and reparented to init, still chewing CPU. So a build that hangs past its timeout used to leave a little colony of runaway processes behind. Two changes fix it. The child is spawned in its own **process group** — on Unix, `command.process_group(0)` makes the child the leader of a fresh group, and its descendants inherit that group id. On timeout we send `SIGKILL` to the *negative* pid, which POSIX defines as "signal every process in the group", so `buildah` and its children die together. As a backstop, `kill_on_drop(true)` guarantees the direct child dies even if we never reach the explicit kill. Testing this needs no real `buildah`: a shell shim that backgrounds a `sleep`, records the grandchild's pid, and waits is enough — after the timeout fires, the test polls until that grandchild is gone, proving the whole group went down and not just the parent.

The self-cleaning directory is a small thing that pays off on every error path. Rust has no `finally` block and no manual destructor call; instead the `Drop` trait runs code the instant a value leaves scope, on *every* exit — a normal return, an early `return`, a `?` that bails, or a panic unwinding the stack. Wrap the temp directory in a type whose `Drop` removes it, and a build that succeeds, fails, times out, or panics all leave nothing behind:

```rust
impl Drop for ScopedDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
```

This is RAII — Resource Acquisition Is Initialisation, the same idiom C++ uses — and in Rust it's the *default* way to manage a resource, not a pattern you reach for. The directory also gets a random suffix now (`<build_id>-<random>`), because the old path was derived from the context digest, so two concurrent builds of the same context fought over one directory. Distinct directories, cleaned on every path: two bugs closed by one type.

### One table for "who may call this?"

The batch/build authorization fixes above were done handler by handler, each route growing its own `require_system` or `authorize(Deployer)` line. That works, but it leaves the answer to a basic security question — *which principal may call this route?* — scattered across four thousand lines. That kind of rule drifts: add a route, forget the check, and nobody notices until a review, if then.

So the role requirements now live in one table, `ROUTE_MATRIX`, mapping every mounted route to the principal it needs — `Public`, `AnyToken`, `Deployer`, `Admin` or `System`. It doesn't add enforcement (the per-handler checks still do that, and token *scopes* are a later theme); it makes the current rules auditable in one place. The guard that keeps it honest is a test: it scans the router's source for every `.route("…")` and asserts each path appears in the matrix. Add a route without a matrix entry and the test goes red. The point isn't the table — it's that a security-relevant list can no longer fall silently out of date with the code it describes.

## When the leader forgets

Here's a thought experiment. You submit a thousand-job batch, the leader answers "batch 3, accepted", and thirty seconds later the leader restarts. What does `relish batch-status 3` say now?

With the first version of the tracker, one of two things. Either "batch 3 not found" — the tracker was a `HashMap` in the leader's process memory, and the process is gone. Or, worse, a summary of somebody *else's* batch: the id counter also lived in that process, so the restarted leader starts handing out 1, 2, 3 again, and the next submission wears your batch's name. The jobs themselves are fine — they run on other nodes and keep running — but every record of what was asked for, and every callback that arrives late, lands in a tracker that has never heard of them. The build registry had the same disease. In-memory bookkeeping in a distributed system isn't bookkeeping. It's a rumour.

The fix is the same move deploys made back in Stage 4: put the state where the cluster already keeps state it can't afford to lose. The Raft state machine grows two fields — `batch_state` and `build_state`, each a monotonic id counter plus the tracked records — and four log entry types: `BatchRegister`, `BatchJobUpdate`, `BuildRegister`, `BuildUpdate`. Registering a batch is now a Raft write whose *response* carries the allocated id, exactly the shape certificate serials already use: the counter increments inside `apply`, serialised by the log, so two concurrent submissions can't race to the same number and a restarted leader continues the sequence from the replicated value instead of starting over. New `RaftRequest` variants are appended, never inserted (the wire is self-describing JSON, but variant discipline costs nothing). The new `DesiredState` fields are required: a snapshot of a different state generation is refused before it's decoded (Chapter 14), so there's no older shape to fill in.

Two details of the state machine are worth pausing on, because they're both about the same rule: **`apply` must be deterministic**. Every replica runs the same entries and must arrive at the same state, or the cluster quietly forks. So the batch record's timestamp travels *in the request* — if `apply` called `SystemTime::now()`, three replicas would record three different times. And garbage collection of old terminal records happens *inside* `apply`, at registration time, keyed on the incoming record's clock: every replica prunes the same records at the same log index. We considered the obvious alternative, a periodic leader-side GC task, and turned it down — a timer is one more moving part, it needs its own leader-election awareness, and pruning exactly when the state grows is both deterministic and sufficient (a retention window plus a cap of fifty terminal records, the same cap deploy history uses).

Durable records make honest reports possible, and honest means *validated*. The report endpoint used to accept anything: any status string, any job name, any batch id, no questions asked — and with the trackers durable, "unknown batch" no longer has the excuse of "maybe the leader restarted". So reports now run through a transition table. The heart of it is one `match` on a tuple:

```rust
let legal = matches!(
    (job.status, status),
    (JobStatus::Pending, _)
        | (JobStatus::Running, JobStatus::Completed | JobStatus::Failed)
);
```

If you haven't seen it, `matches!` is a macro that asks "does this value fit this pattern?" and answers with a `bool` — and the value here is a *tuple*, so one pattern constrains both the current state and the proposed one at once. `|` inside a pattern means "either of these". Everything the table doesn't allow is refused: a forged status string is a 400, an unknown batch or job a 404, a conflicting transition (`completed` → `failed`) a 409. A *duplicate* terminal report, though, is a 200 that changes nothing — reports get retried now, and a retry must be safe to receive twice. Idempotency isn't a courtesy; it's what makes the retries below possible.

Because the other half of the JOB3 finding was delivery. A completion callback was fire-and-forget: one POST, result ignored. Dispatch was too — if the target node was down, its jobs stayed "pending" until the one-hour watch timeout shrugged them into `failed`. Both ends now retry with bounded backoff, and dispatch that exhausts its retries fails the affected jobs immediately, with the truth, instead of letting them age out. But retries only shrink the window; they can't close it. The node could die *after* running the job and *before* any callback lands. For that there's a pull backstop: the leader runs a per-batch watcher that polls the assigned nodes' status APIs — the same instance-status read `relish status` uses — and applies whatever terminal outcomes it finds through the same validated, idempotent report path. Push is the fast path; pull is the guarantee. And because the watcher's identity is just "a task in the leader's process", a restarted leader resumes it lazily: a status read on a non-terminal batch with no live watcher spawns one from the durable record. The integration test for this is pleasingly literal — register a batch in Raft, run its job on another node with the callbacks aimed at a dead port, build a fresh API process over the same council, and watch the batch complete anyway.

Two smaller batch repairs ride along. Unschedulable jobs used to vanish — the response listed them once and the tracker never heard of them; they're now first-class records with a terminal `Unschedulable` status that shows up in every summary. And the job namespace, which used to travel in two independent places (the submission field and the job spec — the agent deployed into one while the watcher matched on the other, stranding the batch for an hour if they disagreed), is resolved once at submit: a conflict is a 400, and the resolved value is written into both places so dispatch, deploy and watching can't diverge. While we were in the area, the allocator's profile groups moved from a `HashMap` to a `BTreeMap`, which closes an old low-priority finding: the same submission now produces the same assignment plan every time, instead of depending on hash iteration order.

Builds got the same treatment with one extra wrinkle: a build can be submitted to any node, and only the leader can write to Raft. Batch dodges this because submissions are already leader-forwarded; a build has to run where `buildah` is. So build tracking gets a small internal endpoint, `/v1/build/track` (system principal only), that a builder whose council handle is a follower uses to route its register/update through the leader. Reads need no forwarding at all — every council member holds a replica of the state. And "resumes or terminates honestly" for a build means terminates: a `Running` record whose runner is this node, with no live runner task in this process, can only mean the node restarted mid-build, so the first status read rewrites it to `failed: builder restarted mid-build` — durably — rather than serving `running` until the heat death of the universe.

Delegation had a quieter gap, flagged in the review as "probably fine, please check". It wasn't fine. The context blob the CLI uploads is a *bare* blob: no manifest points at it, so the catalogue has no holder record for it, so the heal loop never replicates it. The delegated builder fetches the context from its own local registry — correct in principle, since request-supplied registry endpoints are exactly what the JOB2 fix banned — but its registry has never seen the blob, so the build dies on a 404. The fix keeps the endpoint discipline and moves the bytes instead: before POSTing `/v1/build/run`, the delegating node copies the blob from its own registry to the builder's (both addresses derived from membership plus its own config, a HEAD check first so the copy is idempotent). Delegation also stopped being single-shot: the submit path now walks *all* capable peers, transferring and dispatching to each until one accepts, and only then reports the honest 502 with the last error.

Running the buildah-gated suite for this theme flushed out one more bug, and not where we were looking. Real `buildah push` against Pickle failed with `determining upload URL: http: no Location header in response` — on main too, so the gated end-to-end test had been quietly red for a while. The debug log pointed at the chunked-upload PATCH: the OCI distribution spec says every chunk response carries a `Location` header (the URL for the next chunk or the final PUT), and containers/image 5.29 — the library inside buildah 1.33 — started reading it strictly instead of falling back to the original URL. Our PATCH 202 never set one. One header, one line of axum, and the whole build → push → catalogue pipeline works against a modern buildah again. Gated tests only earn their keep if somebody runs them; this is what they're for.

Which leaves signing, and the finding that sounds pedantic until you follow it through. "Required signing is best effort" meant: the build runner asked the agent to sign, the agent generated a *fresh ephemeral keypair*, signed with it, and attached the result. Nothing trusts that key. It's not in any trust policy, it chains to nothing — the signature was cryptographically valid and semantically worthless, and a `require_signatures` cluster would refuse to deploy the image it decorated. Better still, `AttachSignature` for a digest the catalogue didn't know was a silent no-op, so a build could report `completed` having attached its useless signature to nothing at all. Both halves are now honest. The state machine refuses an attach for an unknown digest (the response is `Refused`, and the caller treats it as the failure it is). And the build runner signs with something the cluster actually trusts: it generates a workload CSR, has the council's Workload CA issue a certificate for it — the same CSR path every deployed instance uses — builds a keyless signature over the digest, and *verifies it against the cluster root CA* before attaching, the exact check enforcement will run at schedule time. When `[images.trust_policy] require_signatures` is set, that whole sequence is part of the terminal-state definition: no trusted signature, no `Completed`, and the failure reason says why. On a trust-free cluster the failure is still just a warning, because an unsigned image there is still a useful image.

The CLI got the last crumb of the review: its wait loops. `relish build` polled every two seconds, forever; a build stuck `running` (say, on a node that restarted before this theme) held the terminal hostage until Ctrl-C — and Ctrl-C was the *only* exit. Both `relish build` and the new `relish batch-status --wait` now take a `--timeout` (defaults sized to the server-side limits plus margin), race the poll against `tokio::signal::ctrl_c()` with `select!`, and exit non-zero carrying the last known state, so a timed-out wait tells you where things stood instead of nothing.

## A million jobs without a million records

Everything in the batch path so far treats a job as a *thing*: a spec in the request, a record in Raft, a report per transition, an owner helper and two log files on the node. That's fine for a thousand jobs. Now picture a million. The request alone would be a few hundred megabytes, every finished job would be its own Raft write, and a node's job checkpoint (capped at 16 MiB and rewritten on every state change) would stop admitting work somewhere around the fortieth thousand. The whitepaper's answer to "can the leader schedule 100M jobs a day?" says the Raft log records only batch-level decisions. The code, until this section, didn't.

Kubernetes has the same problem in a different shape. An Indexed Job creates one Pod per index, and each Pod is several etcd writes over its life (create, bind, status updates, finalizer removal, deletion). The upstream scalability envelope stops at 150,000 Pods per cluster, so a million tasks run in waves. The usual escape hatch is a work queue: a few long-lived workers draining Redis. That scales beautifully, and the orchestrator no longer knows your tasks exist. Retries, logs and results become your code.

We wanted the other thing: every task its own process, retried and tracked by the orchestrator, at a control-plane cost proportional to chunks rather than individual tasks. This section first builds the pieces as libraries, tested hard and measured in one process, and then wires them into Raft, the API and `relish`. Wiring changes the Raft log and snapshot formats, so it bumps the compatibility generations, and 0.2.0 needs a fresh cluster; before 1.0.0 we don't migrate (the plan in `docs/plans/2026-09-28-plan-million-jobs.md` has the details).

### Template plus count, ranges instead of records

A *task array* is one job template plus a count. Every index from 0 to `count - 1` becomes one task, and `{index}` in the arguments becomes the task's number. A million-task submission serialises to a few hundred bytes; there's a test for that.

The indices are grouped into fixed-size *chunks* (1,024 by default, so a million tasks is 977 chunks). The chunk, not the task, is what the leader hands to a node and what it records. Which leaves one question: how do you remember which of a million things are done without a million entries? You store ranges. `IndexRangeSet` keeps `u32` indices as sorted, disjoint, non-adjacent inclusive ranges. A million tasks finished in order is one pair, `[[0, 999999]]`. Sparse failures cost one pair each.

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<[u32; 2]>", into = "Vec<[u32; 2]>")]
pub struct IndexRangeSet {
    ranges: Vec<(u32, u32)>,
}
```

Those two serde attributes are worth a look. `into` tells serde to convert the value into another type (`Vec<[u32; 2]>`, a list of two-element arrays) and serialise that instead; `try_from` goes the other way on the way in, through a `TryFrom` implementation that can fail. That's where the invariant check lives: reversed pairs, overlaps and touching ranges are refused at the door, so no code path ever sees a malformed set, however it arrived. The private field does the rest. Nobody outside the module can build a `ranges` vector by hand.

Lookups are binary searches with `partition_point`, which takes a predicate that's true for a prefix of the slice and false for the rest, and returns where it flips. Finding the range that might hold `index` is "the first range whose end isn't below it":

```rust
let position = self.ranges.partition_point(|&(_, last)| last < index);
```

Inserting a range finds the run of ranges it overlaps or touches with two of those searches and replaces the run with one merged range using `Vec::splice`. The arithmetic around the ends is done in `u64`, because `last + 1` overflows at `u32::MAX` and the full `u32` domain has 2^32 members, one more than a `u32` can count. The test suite includes a property test (proptest generates random sequences of inserts, removes and takes) that checks the set against a plain `BTreeSet<u32>` model after every step, and re-checks the canonical-form invariant each time.

### The leader's bookkeeping, and a fence

`TaskArrayState` is the leader's view of one array: chunk ids move between a queued set, a held set per node and a done set, and a finished chunk contributes only its counts and its failed indices. It never reads a clock and never holds a record per task, because it's shaped to live inside the Raft state machine, where `apply` has to be deterministic.

The interesting rule is who's allowed to retire a chunk. Suppose node n3 goes quiet, the leader gives up on it and hands its chunks to n1. Then n3 comes back and reports one of those chunks as done. If the leader accepted that, the chunk's tasks would count twice. So every grant carries an *attempt* number, re-granting bumps it, and a completion is accepted only from the node holding the chunk *at the current attempt*. Everything else is a typed error. The distributed-systems name for this is a fencing token, and it's the same idea as the term number on a Raft message: a stale actor can't act, however confident it is.

Granting is a separate pure function, `plan_grants`, which tops each node up to about two rounds of its slots, emptiest node first, lowest chunk ids first. Fast nodes drain their chunks sooner and get more, so there's no up-front split to get wrong. The design doc's original sketch partitioned the count across nodes by capacity once, up front, which guarantees a straggler tail whenever one node turns out slower than predicted.

The test that matters most here is another property test: a random mix of plans, completions, node losses and cancels, after each of which every chunk must be in exactly one of queued, held or done, and every task counted exactly once. Then there's a plain unit test that runs a whole million-task array with 1% of indices failing for good on three nodes. The leader's state at the end is well under the 256 KiB budget, and the number of rounds depends on the number of chunks. At a fixed chunk size this still grows with task count; the gain is amortisation, not constant cost.

### The node: slots, not chunks

On the node, a `TaskPool` runs tasks through a `tokio::sync::Semaphore` whose permits are the node's slots. Every chunk the node holds draws from the same permits, so the tail of one chunk overlaps the head of the next instead of leaving slots idle. Each task is a tokio task in a `JoinSet` holding an *owned* permit, the same RAII trick Chapter 4 used for connections: dropping the permit frees the slot, so there's no release call to forget on an error path.

Running an attempt is behind a trait with two real implementations, which is our rule for when a trait is allowed to exist. Production `OwnedRunner` uses the node's durable runtime ownership and cached images. It holds the resource reservation through cancellation and uncertain retirement; a kill acknowledgement is not proof of exit. `ProcessRunner` remains a direct-process test and benchmark backend with bounded capture. `FakeRunner` computes the outcome from the invocation, and it's what makes a million-task test run in half a minute. It takes the outcome as a closure:

```rust
pub fn new(
    delay: Duration,
    outcome: impl Fn(&TaskInvocation) -> AttemptOutcome + Send + Sync + 'static,
) -> Self
```

`impl Fn(&TaskInvocation) -> AttemptOutcome` means "any function or closure with this signature", and the extra bounds say it can be shared across threads (`Send + Sync`) and doesn't borrow anything short-lived (`'static`). The struct stores it as a `Box<dyn Fn(...)>`, a heap-allocated closure called through a pointer, which is Rust's closest equivalent to a Go `func` value. The fake also records the highest number of attempts it saw running at once with `AtomicU32::fetch_max`, which is how the test proves the semaphore really bounds concurrency.

Waiting for a permit races against cancellation with `tokio::select!`, and here we add `biased;` as its first line. Normally `select!` picks randomly among ready branches, to be fair. With `biased;` it checks them in the order written, so once the array is cancelled, a free permit never wins the race and starts one more task.

### Durable enough, cheaply

A node running thousands of tasks a second can't fsync thousands of times a second. The ledger writes each finished task as a 22-byte record (index, attempts, outcome, exit code, run time and 64-bit grant generation) into an append-only file, in blocks with a CRC32 each, and a background writer *group-commits*: it fsyncs once per 100 ms or 4,096 records, whichever comes first, and only then tells each waiting chunk its records are durable. Only then does the chunk get reported to the leader.

On restart, `replay` reads the file back. Decoding uses `as_chunks`:

```rust
let (raw_records, _) = body.as_chunks::<RECORD_BYTES>();
```

The `::<RECORD_BYTES>` is a *turbofish*, the syntax for passing a generic parameter explicitly. Here the parameter is a number, not a type (a *const generic*), and the result is a slice of fixed-size arrays, `&[[u8; 22]]`, plus whatever bytes were left over. The decoder then takes `&[u8; RECORD_BYTES]`, so indexing into a record can never go out of bounds, and the compiler knows it.

A block cut short by a crash at the very end is truncated before new records are appended: nobody was told those records were durable. Damage anywhere earlier is an error, not a skip, because skipping would silently re-run or lose finished tasks. Anything in a held chunk without a terminal record runs again. That's at-least-once execution, and it's a deliberate trade for tasks this short.

### Putting it together, in one process

The acceptance test in the portable suite wires three simulated nodes (each a real pool with a fake runner and a real ledger on disk) to the real leader state and grant policy. One task in a hundred fails its first attempt and succeeds on retry. A third of the way through, node n3 is lost: its chunks go back to the queue at the next attempt, and its late reports are refused by the fence. At the end every index has exactly one accepted outcome, exactly one retry is counted per failing index, and the three ledgers between them hold a terminal record for all of them. The suite runs 100,000 tasks, which takes about three seconds in a debug build. With the full million it took 30.6 seconds on the laptop, about 32,700 tasks a second, and the leader changed its state in 422 ticks. Real processes will be far slower than a fake; the Criterion suite (`make bench-task-arrays`) measures the machine's fork/exec floor through the real runner, so we'll know by how much before promising anyone a number.

### Wiring it in: one Raft entry per array per tick

Those 422 ticks become Raft entries once the pieces are wired, so the shape of the entry matters. We gave task arrays exactly one new `RaftRequest` variant:

```rust
/// Register, sync, cancel or requeue a task array.
TaskArray(Box<crate::meat::task_array_store::TaskArrayWrite>),
```

`TaskArrayWrite` is its own enum with registration, sync, cancellation and requeue variants, plus atomic mixed-manifest registration and cancellation, and the rules for applying each one live beside the data in `meat::task_array_store`, where they're plain functions with plain unit tests. The state machine's part is six lines. Why the `Box`? A Rust enum is as large as its largest variant, because every value has to fit in the same slot. A `Sync` carries two vectors and a `Register` carries a whole `JobSpec`, and without the box every `RaftRequest` in the log (including the humble `Noop`) would pay for that space. `Box<T>` puts the payload on the heap and leaves a pointer behind. It's the same reason the other big variants in that enum are boxed.

Arrays take their ids from the same counter as ordinary batches, so `relish batch-status 12` names one thing. The apply function borrows the counter as a closure:

```rust
pub fn apply(
    &mut self,
    write: &TaskArrayWrite,
    allocate_id: impl FnMut() -> u64,
) -> Result<TaskArrayApplied, TaskArrayStoreError>
```

`FnMut` permits several calls: a mixed manifest takes one parent ID and one ID per resource profile. Validation happens before any allocation, so an invalid profile rejects the whole submission without consuming IDs. The call site still says `|| batch_state.allocate_id()` without `TaskArrays` knowing about the counter.

The leader runs one loop, on every node, which does nothing unless the node leads. Once a second it reads the replicated arrays and sends each node its share: the chunks it holds, each with its grant attempt. The node's answer is its free slots and the chunks it has finished. For each running array the leader then writes a single `Sync` entry holding both the finished chunks and the next grants. To plan grants that account for the chunks being retired in the same entry, the leader clones the state, applies the results to the clone and plans against that. A node that finished a chunk gets its replacement in the same entry, and `apply` re-checks everything anyway. A holder that hasn't answered for 30 seconds gets a `Requeue`, and the fence from earlier makes its late reports harmless.

The node persists the highest recovery/term/index control version and the highest grant generation per chunk. It reconstructs work from the next valid snapshot: every sync tells it what it holds, and it starts what it isn't running, cancels what it no longer holds, and keeps reporting a finished chunk until the leader stops listing it. That makes a restarted leader and a restarted node the same case as a normal tick. There was one trap. Finished results sat in a map keyed by chunk id, with the attempt stored beside the result. If the leader takes a chunk back and later re-grants it to the same node at the next attempt, the old run (cancelled, but still finishing) could land after the new one and overwrite it, and the chunk would never be reported again. Keying the map by `(chunk, attempt)` makes that impossible, rather than unlikely.

Two smaller Rust points came out of the node. The first is that `TaskRunner` can't be used as a trait object. Its method returns `impl Future`, a type each implementation picks for itself, and `dyn TaskRunner` would need one type known up front. So the node holds an enum, `NodeRunner::Owned`, `NodeRunner::Process` or `NodeRunner::Fake`, whose own `run` matches and forwards. With two implementations that's three lines, and the API state can hold one concrete node type. The second is that Clippy rejected our first version of "open the array if we haven't yet", a `contains_key` followed by `insert`, because it looks the key up twice. The `Entry` API does it once:

```rust
let run = match arrays.entry(assignment.batch_id) {
    Entry::Occupied(entry) => entry.into_mut(),
    Entry::Vacant(entry) => match self.open(assignment).await {
        Ok(run) => entry.insert(run),
        Err(error) => { /* answer with zero slots and the reason */ continue; }
    },
};
```

`entry` returns a handle to the slot, full or empty, and holding it keeps the map borrowed, so nothing else can change the map between the check and the insert.

On the node, a restart replays the ledger and each held chunk runs only the tasks with no terminal record (`TaskPool::resume_chunk`). The binary has to be on the node's `[process_workloads]` allowlist, like any host process, and a node that can't run an array answers with zero slots and the reason, which `relish batch-status` prints. The integration test pushes 100,000 fake tasks through a real single-node council: the whole array cost 51 Raft entries.

What we didn't do is as telling. No per-task Raft entries, obviously. No bitmap for the done set: a million-bit bitmap is 122 KiB whatever it holds, while ranges are eight bytes in the common case. No progress over the reporting tree: it's bincode, so a new field there would drag a binary format along, and a pull over HTTP puts the leader in charge of the cadence. Image tasks now use rootful Linux OCI isolation and resource limits. Host tasks still refuse mount isolation and explicit resource limits, and require the owned process runtime and an allowlist. And no speculative duplicates of slow chunks. At-least-once would allow them, but we'd rather measure a tail before we optimise one.

### Packing profiles and querying outcomes

A shared `ExecutionBudget` accounts for app commitments and actual running attempts in CPU and memory. App creation, rolling replacements and adoption use the same ledger as batch work. Recovery restores ownership even if the node's capacity has shrunk; it stops new admission rather than erasing a surviving app. Waiting batch attempts enter a FIFO resource queue. This prevents continual overtaking by smaller requests, but can leave free capacity idle behind a large request. There is no tenant DRF or app pre-emption; new deployments still need rollout headroom.

A manifest groups up to sixteen homogeneous resource profiles. Stable parent/profile/index identities avoid serialising a million specs. Workers reserve each attempt, not its queued chunk, release requests during retry backoff, and reuse a bounded pool of owned runtime identities per namespace. Each image attempt has a fresh runtime generation, a read-only root and temporary scratch space. Cached images reduce transfer and unpacking cost; runc still launches a container per attempt, so startup cost remains part of the throughput budget.

Terminal outcomes stream into the ledger while a chunk runs. The acknowledgement waits for the checksummed block, fsync and derived redb index transaction. A failed write stops local work and reports a refusal. The leader's accepted owner/generation ranges select results, including failures beyond the capped summary list. The index keeps the newest grant for each task even when an older execution finishes later. Output is keyed by grant too. New worker and array directories are synced in
their parent before a grant can execute, because syncing a file alone does not
make its directory entry durable. Streaming recovery retains one checksummed
block and only records from held chunks; it does not build a sparse set of every
historical index. The explicit `replay` API still collects records for tests and
small callers.

A disaster-recovery epoch can rewind the council snapshot while worker grant fences remain newer. Comparing just term and index would either hang that work or mix histories. The node instead persists a recovery refusal before cancelling old attempts, preserves their directories and requires fresh worker data after re-enrolment. Ordinary leader elections keep the same epoch and still resume durable outcomes. An empty leader snapshot still syncs once with each worker; it cannot assume no work exists locally. The recovery test finishes generation five, rolls control back to generation one in a new epoch, restarts the worker and verifies that neither execution nor deletion can cross that refusal. External effects still need a business key that survives a cluster rebuild.

The derived index uses a 1 MiB cache per profile, rather than redb's 1 GiB default. A node can retain many profiles, so leaving the database default would make job-history queries compete with application memory. The million-record ledger case also exercises index rebuild and lookup with that small cache.

The default view is a summary, not a task list. Watch, JSON status and the dashboard show counts and rates. Histograms merge before p50/p95/p99 are read; those are bucket upper bounds for final attempts, not end-to-end latency. Detail pages inspect at most 4,096 indexes, return at most 1,000 rows and contact at most eight workers, with a cursor even for an empty failure page. Retention starts at terminal acceptance and groups a parent with its profiles. Worker loss can lose detail without changing replicated accepted counts.

The manual's burger manifest runs 1,000 small and 64 larger hashing jobs beside the web service; the executable homepage tour checks both the app and all accepted outcomes. This demonstrates the path, not the whitepaper's daily target. Qualifying 100 million unique successes needs a real sustained run with resource profiles, concurrent apps, retries, failures and storage measurements. The [implementation plan](../plans/2026-10-04-plan-delegated-jobs.md) records that gate.


Control reports carry exact failed counts and a capped preview of 256 index
ranges per chunk. The full result index remains authoritative beyond that
preview. Streaming capture moves to the writer when a task finishes, rather
than keeping another copy until the chunk ends. Otherwise sparse failures in
a large chunk would make the executor retain hundreds of megabytes of output
that it had already persisted.

Owned runtime reuse also has to preserve the source's namespace. A runtime
process alone does not carry the agent's firewall binding. Delegated executors
now live under `/reliaburger/<namespace>/<executor>/<slot>`. The node publishes
the namespace ancestor before starting a descendant, and the eBPF connect hook
uses it when there is no exact app binding. Exact app bindings keep precedence.
The executor caches at most 256 ancestors and evicts only one with no surviving
attempts. It journals cache ownership before publication, and startup retires
old runtime owners before clearing those recorded, same-boot bindings. A live
source check refuses start or stops the original owner when enforcement is lost.
The real-runc namespace regression connects successfully inside its namespace,
refuses a cross-namespace service, removes the live binding during execution,
and checks that the process tree has retired before recovery clears the journal.

Rebasing this path onto the newer security work exposed a second admission
boundary: arrays must bind tags and apply upstream trust rules before committing
any definition, just as ordinary jobs do. Both array and mixed-profile submissions
now use the API's shared image binder, verify required cosign signatures over the
bound digest, and accept every still-trusted cluster root during CA rotation.
Followers preserve caller authentication when forwarding admission and cancellation.
The shared batch counter also checks space for the manifest parent and every
profile before allocating anything. A refusal cannot leave half a manifest or
panic a Raft replica when the counter is exhausted. Regression tests cover both
submission forms, unavailable registries, refused upstream images, missing cosign
verification, and exhausted IDs.

The image-backed acceptance tests have their own `owned_task_arrays` binary.
The regular Linux gate keeps the warmed image mirror alive while they run;
the OCI interruption driver isolates networking and cannot serve that role.
Main's finite evidence registry now names both runtime cases and the cluster
worker-loss case explicitly. A repository regression check catches missing
bindings and stale reviewed OCI source fingerprints before aggregation.

### Network policy for jobs

Jobs are the 0.2.0 headline workload, and for most of the release they couldn't say where they were allowed to connect. An app could carry `[egress] allow = [...]` and `[firewall] allow_from = [...]`; a `JobSpec` had neither, so a million crawler tasks ran with the network wide open. Decision D26 gave jobs both, with the same types and the same validation as apps (`validate_network_policy` now checks either kind: every egress entry parses without a DNS lookup, every `allow_from` source is `app` or `namespace/app`).

Programming them was the interesting part, because a job runs in three different places.

A job the agent runs directly goes through the same create → program → start path as an app. `apply_network_pre_start` used to take its allowlist from the app spec, and a job has none, so the job's start now prepares the allowlist itself and the loop takes it from that resolution when there's no app spec. A retry does the same from the recorded job spec.

A fresh task attempt holds a `NamespaceLease` from the delegated runtime. The lease grew two methods: `enforce_egress` programs the task cgroup after `runc create` and before `start`, while runc holds the process, and `publish_address` records that the task's container address belongs to the job's namespace on every port, with its `allow_from`. Both are undone in `retired()`, which only runs once the runtime has proven the attempt gone. A name that doesn't resolve within five seconds refuses the attempt. Apps start deny-all and let the re-resolver fill in the allowlist later; a short task has no later, so refusing is the honest answer.

A reusable executor is the subtle one. Its commands all land in the executor's `task` cgroup, so the allowlist goes there once, when the executor starts. That only works if every command the executor runs wants the same allowlist, and the executor key already hashes the whole template minus the fields that may vary per command. `egress` and `firewall` aren't on that list, so two templates with different policies get different keys and never share a container. `tasks_with_different_egress_never_share_an_executor` pins it.

The agent's kernel reconciliation has to know about all of this, or its sweep would scrub a task's allowlist as an orphan and its reconcile would forget a task's address. So the agent owns a small `DelegatedNetwork` that the runtimes write into: the task cgroups holding an allowlist, and the task addresses with their owners. A runtime claims a cgroup *before* writing it, and the sweep reads the claims *after* reading the kernel, so anything the kernel showed is already claimed. An `AtomicBool` change flag makes the agent's next tick reconcile when a task's address appears, which is how a job's `allow_from` grant reaches the kernel.

A node that can't enforce either refuses the whole array at admission instead of failing every attempt, and `runtime = "process"` refuses both: a host task shares the node's network and runs as root, so an allowlist or a grant there would promise a boundary the task can step out of. `job_egress_allowlist_is_enforced` runs both the agent path and a task lease in the Lima VM.

### Definitions, runs and durable trigger identities

A template and a count describe work, but not why it runs. A manually submitted
cleanup, the same cleanup at 03:00 UTC, and a deployment migration need stable
run identities. Replaying the admission transaction after a lost response must
return the original run, rather than launch the command again.

#638 adds `JobDefinition` and `JobCatalog` to the existing
`TaskArrays` state machine. A definition fixes the template, task count and
trigger policies. Each admitted `RunRecord` captures a revision, the complete
definition's digest and its trigger. The execution snapshot remains in the
ordinary task-array record. Updating the reusable definition cannot rewrite
that snapshot or change an existing run's unknown-outcome policy. Count defaults
to one when the task policy is omitted.

`JobWrite` is an enum: its `Put` variant records a definition and optionally
starts a manual or deployment-hook run; `Fire` claims a scheduled occurrence.
Matching the enum forces each transaction to handle its own inputs. The store
prepares metadata on a candidate clone, validates execution capacity, and only
then allocates an ID and publishes both pieces. A refused transaction cannot
advance the definition revision without creating its promised run. A duplicate
manual or hook identity returns the existing run; changed work under the same
identity is refused. This deduplication lasts while the run is retained.

Cron identity uses the definition revision and UTC minute. The same transaction
advances the occurrence cursor and creates the run. With overlap forbidden,
it advances the cursor even when it deliberately skips a firing. The cursor
survives result pruning and definition updates, so neither a new leader nor a
backwards clock step can revive that occurrence. The initial missed-run policy
is explicitly `skip`; there is no catch-up queue. An `allow` overlap policy
still obeys the shared active-run bound.

The catalogue caps reusable definitions and retained run provenance, validates
its shape when deserialising, and bounds complete definition bytes, including
schedule text. Before checking the shared counter, the store computes whether
this exact write needs an ID. Replaying an accepted run or skipping an occurrence
still works when the counter has no IDs left.

The public paths now use those transactions. TOML apply persists a deployment
intent, admits prerequisite hooks, and waits for accepted successful results.
A leader commits app publication and ordinary run admission together. The
standalone controller serialises app publication and cancellation through an
owned mutex guard, held by an independent worker even if the client drops its
event stream. Tokio's `OwnedMutexGuard` keeps an `Arc` reference to the mutex,
so a spawned task can hold it without borrowing a departed stack frame.

Standalone writes clone the checkpoint, preflight identities and progress
capacity, then publish private JSON with file and directory fsync. Only durable
publication replaces the in-memory value. An I/O error fences later writes and
dispatch. Compact initial ranges can expand into sparse progress; admission
reserves that representation before accepting work. Reopen validates the chunk
partition, counts and exact hook identities. Omitted metadata must never open
an app gate. Expired results are pruned before capacity preflight; otherwise the
last receipt could block the submission that would prune it.

Worker admission preserves encrypted templates. Execution decrypts with live
namespace keys in a blocking task, then injects indexed environment without
discarding the decrypted values. Decrypted configuration stays execution-local;
retained output follows the normal scoped capture contract.
A conservative launch marker is durable before start. On reopen an unfinished
marker becomes unknown. Known non-zero exits can retry; uncertain execution
requires a user decision tied to the exact grant fingerprint. Losing namespace
enforcement after launch is unknown too: retiring the process tree cannot prove
that external effects never occurred. The real-container test establishes a
running owner before removing its binding, then checks retirement and that
honest outcome. Bulk runs keep at-least-once replay on the same engine.

Singleton attempts retain successful head/tail output and forward stdout/stderr
under the logical name, namespace and stable `run-ID`. Physical executor slots
can't serve as log selectors because later jobs reuse them. A per-run reader
guard holds that generation while its follower drains. Cleanup closes new
readers, gives existing readers a bounded grace period, then cancels stalled
followers before reusing the slot. Rust's `Drop` releases the guard even when a
client disconnects. Reused process slots replace old capture files before
launch, giving checkpoint readers a new file identity; old bytes cannot become
another job's output. A tail snapshots complete-line offsets and file identity;
following resumes there instead of replaying those lines twice. Bounded CLI and
dashboard summaries show runs, schedules, accepted counts and rates. Detail is
an indexed worker query. Followers preserve the caller's credential for reads
and writes, so forwarding cannot expand a scoped user's authority.

The regressions exercise replay, immutable snapshots, cron rollback and overlap,
hooks across leadership changes, storage failure, malformed reopen, worker
restart, output identity and public endpoints. Protocol/state are 48/65 and
require matching binaries and a fresh cluster. Executor reuse, throughput
qualification and resident model workers remain separate issues; these semantics
don't establish 100m accepted successes/day.

## Lessons from the phase

**Optimisation is an audit with a deliverable.** The single most consistent finding of this phase wasn't a speed-up. It was library-not-wired: `add_port_mapping` with no production callers, `VolumeManager` with no production callers, an HTTPS-only pull client that made cluster images undeployable, a CLI that sent job names to a cluster that had never heard of the jobs. Well-tested libraries pass review; only tracing the live path from the user's artefact to the kernel finds the missing arrow. If you take one habit from this chapter: when you're asked to optimise something, first prove it runs.

**Route distributed decisions through the place that already has an order.** The GC couldn't safely delete "one of two copies" until the *decision* went through Raft — not the deletion, the decision. The same shape appears in the heal loop's holder updates and the catalogue commits. Check-then-act across machines is always a race; a total order is the only fix, and you usually already have one.

**Match the dispatch idiom to the workload's lifecycle.** Deploys reconcile (converge on desired state, forever); jobs dispatch (run once, report, done). Stage 4's placements machinery is exactly right for the first and destructively wrong for the second — a completed job looks like drift, a moved assignment kills running work. One crate, two idioms, both correct.

**States that look terminal and aren't.** `stopped` meant "success", except when it meant "failing job between retries". `cache/redis:7` meant the same image as upstream, except when the tag had moved. A snapshot named like a directory was one, except it needed `subvolume delete`. Every one of these was caught by a test that checked the *unhappy* path — and the failing-job watcher bug went from written to caught to fixed inside an hour because the failure test existed before the fix.

**Say what you didn't do.** Cached upstream images are unsigned and exempt from trust policy — written at the exemption. The registry has no auth, so the loopback warning explains the firewall posture. Rootless proxies don't survive adoption; local-origin traffic bypasses the DNAT map; the whitepaper still describes synchronous replication. Each gap is recorded where a maintainer will trip over it. Honest edges are cheaper than surprises.

The roadmap's milestone for this phase reads: port mapping uses O(1) nftables maps, images download from multiple peers in parallel, logs compress 5× with random-access reads. All true. The truer summary: the paths those words describe now actually exist, end to end, with tests standing on each one.

## Short jobs without fresh containers

What does a job that runs for five milliseconds cost? Until now, a whole
container. Bun pulled or found the image, created namespaces, wrote an OCI
bundle, started runc, waited for the payload and tore everything down again.
Caching image layers takes care of the pull. It doesn't touch the rest. For a
nightly backup that runs for an hour, nobody notices. For an array of a million
tiny commands, the setup *is* the job.

How big is the gap? On one four-vCPU VM, in the same sixty seconds, fresh
containers finished a couple of hundred BusyBox `true` jobs. Native host
executors finished about a quarter of a million. That's the one throughput
comparison this section quotes; the rest live in the
[timed-scenario record](../qualification/2026-10-09-timed-job-scenarios/README.md),
next to what they do and don't prove.

The fix isn't one trick. It's two cheaper runtimes, an explicit way to choose
between them, and a lot of care about when a reused process is really safe to
reuse. The [plan](../plans/2026-10-07-plan-reusable-executors-and-throughput.md)
sets out the qualification we still owe before claiming 100 million successes
a day.

### Choosing the runtime per job

A job now names its runtime:

```toml
[job.prepare-record]
image = "registry.example.com/dataset-tools@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
runtime = "shared-runc"
command = ["/usr/local/bin/prepare-record", "{index}"]
cpu = "100m-500m"
memory = "64Mi-256Mi"
```

There are three choices. `runc` (the default) gives every attempt a fresh
container. `shared-runc` runs each command as a new process inside a container
that's kept warm for compatible commands. `process` runs an allowlisted host
binary or script with no image at all. In Rust they're the three variants of a
`JobRuntime` enum, and `#[serde(rename_all = "kebab-case")]` turns `SharedRunc`
into the `shared-runc` you write in TOML.

Our first version guessed the runtime from the fields: an image meant a
container, `exec` or `script` meant a host process. That's convenient right up
until a job has both, or a node quietly interprets an image name as a host path.
Now the runtime is chosen first, and it decides which fields are legal:

```rust
pub fn is_host(&self) -> bool {
    self.runtime == JobRuntime::Process || self.exec.is_some() || self.script.is_some()
}

pub fn validate_runtime(&self) -> Result<(), &'static str> {
    match self.runtime {
        JobRuntime::Process if self.image.is_some() => {
            Err("runtime=process refuses image; use exec or script")
        }
        JobRuntime::Process if self.exec.is_some() == self.script.is_some() => {
            Err("runtime=process requires exactly one of exec or script")
        }
        JobRuntime::Runc | JobRuntime::SharedRunc if self.image.is_none() => Err(
            "runtime=runc/shared-runc requires an image; host exec/script requires runtime=process",
        ),
        // ... a container runtime also refuses exec and script
        _ => Ok(()),
    }
}
```

Two bits of syntax are new here. The `if` after a pattern is a *match guard*:
the arm only matches when the pattern fits *and* the condition holds, so one
variant can have several arms, tried top to bottom. `A | B` matches either
variant. The error type, `&'static str`, is a borrowed string that lives for
the whole program, which a string literal always does. It's fine for a fixed
message; callers wrap it in their own error type.

`exec.is_some() == script.is_some()` is a compact "exactly one of": it's true
when both are set or neither is. And `is_host` is deliberately broader than the
validated rule. An unvalidated spec that names a host command anywhere still
counts as host, so routing and the `host-exec` permission check can never treat
a host command as a container, whichever order the checks run in.

Worker admission and the owned runner check the same contract, so a control
message can't bypass the API. The job schema and stored definitions changed,
so the protocol and state generations in `src/compatibility.rs` advanced
together. Before 1.0, an old cluster starts fresh rather than reading an old
definition differently.

**One node, two backends.** A node that runs both containers and host commands
uses `bun --runtime mixed`, which builds a `MixedGrill<C, H>`. The type is
generic over its container backend `C` and host backend `H`. Rust
*monomorphises* generics: it compiles a separate copy of the type for each
concrete pair, so production gets a `MixedGrill<RuncGrill, ProcessGrill>` with
no dynamic dispatch, and the portable tests get one built from two mocks.
`--runtime runc` stays container-only and refuses host jobs. The adapter never
falls back to the other backend after a failed launch.

The adapter keeps a small route journal recording which backend owns each
instance, written before creation. Status and recovery read that journal rather
than guessing from a PID. Switching an instance to the other backend needs
positive proof that the original owner retired. A missing route isn't
permission to recreate: both backends' inventories must prove the identity
absent first.

A file lock serialises route changes, and the adapter moves that lock into a
spawned task. If the caller's future is dropped mid-create, the lock stays held
until the runtime operation really finishes. That's right for create and stop.
It was wrong for `exec`. `relish exec app -- sleep 3600` held the lock for an
hour: the agent's 300-second timeout dropped the caller's future, the spawned
task kept going, and `stop` waited behind it. `exec` doesn't change a route, so
it now reads the route without the lock and awaits the backend in the caller's
own future. Dropping that future drops the backend call.

Inventory snapshots had the same disease. We had routed them through the
exclusive lifecycle claim used by create, start and retirement, so a read could
queue behind a mutation. A public run of a thousand fresh containers never
finished, because every snapshot timed out behind live container work. Later,
after a slot switched from containers to host jobs, inventory waited on the
claim now held by the running host replacement. Reads now take no claim. They
read the atomically published route files, or ask the original backend for its
retirement receipt, and change nothing. Anything that changes execution
authority keeps the stronger fence. A snapshot is observation.

Lock-free reading has its own trap. Route files are replaced the safe way:
write a temporary file, then `rename` it over the old one, so a reader sees the
old route or the new one, never half of each. The reader opens the file, then
checks it's a private file with exactly one link, the guard against someone
planting a hard link to a file they control. But a reader that opens the old
file a moment before the rename is left holding a file that has just lost its
only name, and its link count reads zero. Our check called that tampering, and
the orchestrator logged "state unavailable" for an executor about once a run.
A planted hard link has *two or more* links; zero means "replaced while you
were looking". `read_bounded` now opens the path again in that case, a bounded
number of times, and the test reproduces the exact interleaving: open, rename,
validate.

A mixed node also has to describe itself honestly. It reported its runtime as
`runc+process`, which the capability classifier didn't recognise, so the
secrets catalogue skipped all its container tests. Now the classifier sees the
container backend on a mixed node, and reports the host backend only when the
executable allowlist is configured. Host commands run with Bun's own authority.
A container capability on the same node must never be read as isolation for a
host workload.

### Keeping a container warm

`shared-runc` keeps the expensive part of a container (its namespaces, network,
mounts and runtime owner) and throws away the cheap part (the process). Which
commands may share one container? Ones that would have got an identical
container anyway. That's the *compatibility key*: a SHA-256 hash of the job
template with the per-command fields removed.

`ExecutorKey::new` clones the template, clears `command`, `exec`, `script`,
`schedule` and `run_before`, fills in the default namespace and resource
ranges, serialises the result to JSON and hashes it. What's left is the pinned
image digest, the namespace, the environment and the CPU and memory ranges. Two
different commands with the same image and limits share a key. A different
memory limit or a rotated credential gets a different container. The key also
refuses an image that isn't pinned by digest: `myimage:latest` can move under a
warm container, and then "compatible" would be a lie.

A pool holds at most 32 of these containers per node, each running one command
at a time. A chunk of a thousand indexes is still a queue, not a thousand
processes. Here's the checkout:

```rust
async fn slot(
    &self,
    key: ExecutorKey,
    reservation: crate::meat::Resources,
    holder: Option<u64>,
    cancel: &CancellationToken,
) -> Option<(usize, Option<Context>, Option<ResourceLease>)> {
    loop {
        let changed = self.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        // ... return None if already cancelled
        let mut slots = self.slots.lock().await;
        let warm = (!self.budget.has_waiters())
            .then(|| {
                slots.iter().position(|slot| {
                    !slot.busy
                        && slot
                            .context
                            .as_ref()
                            .is_some_and(|context| context.key == key)
                })
            })
            .flatten();
        let selected = warm.or_else(|| slots.iter().position(|slot| !slot.busy));
        if let Some(index) = selected {
            // ... charge the budget for an empty slot before any image I/O
            slots[index].busy = true;
            if lease.is_some() || slots[index].context.is_some() {
                slots[index].holder = holder;
            }
            slots[index].key = Some(key);
            return Some((index, slots[index].context.take(), lease));
        }
        drop(slots);
        tokio::select! { biased; () = cancel.cancelled() => return None, () = changed => {} }
    }
}
```

The function prefers a free slot whose container already has our key. Failing
that, it takes any free slot, which may hold an incompatible container to
retire. If nothing's free, it waits for a change and tries again.

`holder` records which run has the slot, but only once there are resources
behind it: a reservation charged right here, or a container that already holds
one. A node reports its busy slots plus the ones that still fit as its
capacity. So a caller that got a slot but is still queued for resources must
not count. If it did, the node would advertise a slot it can't run anything on,
and the leader would send work there instead of to a free peer. Such a caller
becomes the holder only when `admit` charges its reservation. A caller that
retires another profile's container drops back out until its own reservation
is charged. Our first version recorded every caller at checkout, and #654's
author found the phantom slot in review.

A few Rust details carry weight. `bool::then` turns `true` into `Some(value)`
and `false` into `None`, and `.flatten()` collapses the resulting
`Option<Option<usize>>`. So the whole `warm` expression reads: "only if nobody
is queued for resources, find a compatible idle slot". That condition matters.
Without it, a steady stream of tiny jobs could keep reusing warm containers
forever while a large job waited for capacity that never came free.

`Option::take` moves the container context out of the slot and leaves `None`
behind. The caller now owns it. If the caller's future is dropped halfway
through a command, the slot is still `busy` with no context, and stays
quarantined rather than being handed to someone else.

The waiting is the subtle part. `Notify::notified()` creates a future, and
`enable()` registers it *before* we look at the slots. Without that, a slot
could be released between our check and our wait, and we'd sleep through the
notification. `tokio::pin!` fixes the future in place on the stack, which
`enable` needs. `tokio::select!` waits for whichever finishes first, and
`biased;` makes it check cancellation first rather than picking at random.

**The helper.** Inside each warm container, PID 1 is a small static C program.
Rust keeps admission, credentials, outcomes and retirement; the helper only
forks commands and reports on them. Static linking means the image doesn't need
a particular libc or a worker framework of its own. Bun ships the helper inside
its own binary, and that's the first time we've compiled C into the Rust build.

A *build script* is a `build.rs` file at the crate root. Cargo compiles and
runs it before compiling the crate, and anything it prints as `cargo:...` is an
instruction back to Cargo. Ours compiles `helper.c` twice:

```rust
fn compile_executor() {
    println!("cargo:rerun-if-changed=src/bun/reusable_executor/helper.c");
    // ... choose the compiler from CC_<target>, CC or plain `cc`
    for (name, host) in [
        ("rb-executor-helper", false),
        ("rb-host-executor-helper", true),
    ] {
        let output =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"))
                .join(name);
        let mut command = Command::new(&compiler);
        command.args(["-O2", "-static", "-std=c11", "-Wall", "-Wextra", "-Werror"]);
        if host {
            command.arg("-DRB_EXECUTOR_HOST");
        }
        let result = command
            .arg("src/bun/reusable_executor/helper.c")
            .arg("-o")
            .arg(output)
            .status()
            .expect("failed to execute static Linux C compiler");
        assert!(
            result.success(),
            "static executor helper compilation failed"
        );
    }
}
```

`OUT_DIR` is a scratch directory Cargo gives each build script, so generated
files never land in the source tree. `for (name, host) in [...]` destructures
each tuple in the array as it loops, like Python's `for name, host in ...`.
`main` only calls this when `CARGO_CFG_TARGET_OS` is `linux`, the target being
built for rather than the machine doing the building. And yes, that's
`expect` and `assert!`. Panicking is how a build script says "this build
failed", so the no-panics rule for production code doesn't apply here.

The pool then embeds both executables:

```rust
const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rb-executor-helper"));
const HOST_HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rb-host-executor-helper"));
```

All three macros run at compile time. `env!` reads an environment variable
while compiling (Cargo sets `OUT_DIR`), `concat!` glues string literals, and
`include_bytes!` copies the file into the binary as a fixed-size byte array,
which the `&[u8]` constant borrows for the life of the program. At run
time Bun writes those bytes into the container's private bootstrap directory.
There's no install step and no chance of a helper from a different build.

That bootstrap directory came from a bug. Our first version bind-mounted the
helper as a file into the shared unpacked image, which made runc create a file
mountpoint there. Two containers starting at once raced to create it, and the
first jobs failed with `file exists`. The helper now runs from a private
directory bind, and the image needs no mountpoint at all.

**What a command gets.** The helper starts each command in a sibling task
cgroup, as a separate uid, without the helper's capabilities or descriptors,
with private mount and IPC namespaces, fresh scratch filesystems and an
explicit environment. The image root stays read-only. The PID and network
namespaces are shared between commands in the same container. That's the
isolation trade-off you opt into with `shared-runc`, and why it's not the
default.

Bun talks to the helper over a private Unix socket. It authenticates the peer
against the container's real init process, rather than trusting a PID written
in a message. Every command carries a sequence number. Each message back
(started, output, exited, cleaned up) repeats it, and Bun refuses any message
whose sequence doesn't match the command it's waiting for, so a late message
from one command can never be read as news about the next.

**Positive retirement.** When is a slot safe to reuse? Not when the command
exits. It may have left a background child running, and that child would share
the next command's cgroup and limits. Bun asks for cleanup; the helper kills and
reaps every remaining descendant and replies with a cleanup receipt for the same
sequence; Bun then checks the task cgroup's `cgroup.events` says
`populated 0`. Only then is the slot free. We call this *positive* retirement:
we act on proof that something is gone, never on the absence of news. A
timeout, a cancellation or a missing receipt retires the whole container
instead. Recovery after a Bun restart carries the same obligation, including
the sibling task cgroup, before any work is replayed.

Retirement can't wait forever either. A process stuck in an uninterruptible
kernel wait, say on a hung NFS mount, ignores `SIGKILL` until the kernel lets
go. `RETIREMENT_DEADLINE` gives up after ten seconds: the caller gets its
timeout back, while the slot and its resource reservation stay quarantined. The
eviction loop keeps retrying and frees both once the cgroup finally empties.

Our first version of that deadline was a loop that checked the clock between
attempts. #654's author pointed out in review that this bounds nothing if one
attempt never comes back. Each attempt awaits the runtime's `state` and `kill`,
and runc's lifecycle lock can make either wait indefinitely. So the deadline
now wraps the whole operation, every attempt and the final file removal
included:

```rust
let retired = tokio::time::timeout(budget, async {
    while !self.retirement_step(context).await {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    self.remove_files(context).await
})
.await
.unwrap_or(false);
```

In Rust, a timeout cancels a future by dropping it, at whichever `.await` it
happens to be paused. Nothing inside gets a chance to clean up. Go has no
equivalent: a goroutine only stops if it checks its `context`. Python's
`asyncio.wait_for` is closer, but it at least raises `CancelledError` inside
the task. So everything under the timeout has to be *cancel-safe*: dropping it
halfway must leave nothing inconsistent. Three pieces weren't.

The first was the runtime calls themselves. If an abandoned `kill` were simply
started again on the next attempt, a runtime that hangs would collect one stuck
call per retry. The `state`-then-`kill` probe now runs as its own spawned task,
and the context keeps its `JoinHandle`. Awaiting a `&mut JoinHandle` doesn't
consume it, so a cancelled attempt leaves the handle in place. The next attempt
waits on that same probe instead of starting another.

Filesystem cleanup needs the same ownership. Tokio's `fs` functions hand work
to a blocking thread; dropping the future that awaits a removal doesn't stop
that thread. Our first fix kept the caller's wait bounded, but a retry could
start another removal while the old one was still outstanding. Executor paths
come from the slot number. Once reused, the same path names the next executor,
so a delayed removal could delete its files. The context now also retains one
cleanup `JoinHandle`, awaited by mutable reference. A timeout leaves that handle,
the slot and its reservation together. Only completion permits reuse. The
regression test holds cleanup at a gate across repeated timeouts, retires another
executor alongside it, then opens the gate and checks that capacity comes back.
It doesn't need a hung disk to exercise that ownership rule.

The third was releasing the executor's namespace binding. That takes a lock
and then decrements a counter, and cutting it off in between would leak the
binding. It now runs in its own task, once retirement is proven.

The eviction loop had the same flaw one level up: it retired executors one
after another, so a single stuck one held up all the rest. Each executor now
gets one second per eviction tick, and if it hasn't retired by then, it goes
back into quarantine and the loop moves on. The tests use a fake runtime whose
`kill` never returns. With the old loop, both tests hang until their guard
fires.

Reading the mixed-runtime route journal exposed a different race. The writer
publishes a complete record by renaming it over the previous one. A reader that
already opened the old inode then sees a link count of zero. That is a normal
replacement, while two links still mean an unsafe hard-linked record. Checking
for zero links and *then* inspecting metadata again for privacy leaves a gap:
the rename can happen between the two inspections. We now decide whether to
retry and whether to accept the record from one metadata snapshot. An
`Exclusive` reader, which requires a single link, reopens at most eight times
and checks ownership and permissions on every accepted record. All readers
still refuse symlinks. A deterministic test renames the journal at the validation
boundary; the unsafe-replacement tests keep the security checks honest.

A local upgrade qualification exposed another distinction: `Regular` and
`OwnerOnly` readers do not require a live link. Reopening on every rename could
exhaust their budget even though the already-open record was complete and met
its privacy policy. Those readers now validate and read that snapshot directly.
The complete old record is a valid concurrent observation, just as it would be
if the rename happened after validation. Tests replace the path at every
inspection and require one bounded snapshot read; a separate test refuses an
unsafe original snapshot even when its replacement has valid permissions.

Environment filtering also exposed a test dependency. The OCI crash fixture's
runc wrapper read `OCI_CRASH_ROOT` from Bun's environment, which belongs to the
admission injector, not runtime commands. The rootless gate correctly lost it.
The wrapper now finds its fixture beside its own executable. Bun keeps the
private variable for the injector, and descendants keep their filtered
environment. Fixing the test's dependency preserves the boundary we're testing.
Another cancellation fixture put a fake `ip` on `PATH` inside the owner
wrapper, after recording the command's environment. That override no longer
reaches the command. The test now starts an isolated caller with the fake tool
already on its `PATH`, so the recorded environment includes it without changing
the test runner's environment.
The reviewed OCI test registry pins the source file's hash as well as each
test's identity. Changing the fixture also requires refreshing that pin after
review and runtime validation; otherwise CI correctly refuses the old approval.

**What the real Linux tests found.** Most of the bugs in this path were
invisible to mocks:

- A command allocated 128 MiB under a 32 MiB `memory.max` and still succeeded,
  because the VM had swap. `memory.max` limits resident memory, not memory plus
  swap. Memory-limited image jobs now also set `memory.swap.max = 0`, and the
  test demands a real OOM kill in `memory.events`.
- Eight 300 ms commands with one-second timeouts shared one slot. Three
  succeeded, because the timer started while they were still queueing for the
  slot. The timeout now starts once a command is admitted.
- A warm container's 10 millicores and 8 MiB of helper overhead stay reserved
  while it's idle. Ignoring that let a node advertise three slots where two
  fitted, while ignoring idle compatible helpers made a busy node advertise
  none. Offers now count both. Idle containers retire after a second, and
  sooner if someone is queued for resources.
- A queued command's encrypted environment was decrypted *before* it waited
  for a slot. When the namespace's key was retired during the wait, the
  plaintext still ran. Encrypted values are now resolved against live keys
  again after the wait, and must still match the container's compatibility
  key. The pool receives this as a closure, a small function that borrows the
  runner and template and returns a future. Its `Fn` bound means it can be
  called repeatedly without consuming what it borrows. No plaintext ever
  reaches the replicated template or the worker's ledger.
- A Bun crash beside a running application left the replacement refusing to
  start: `kernel source entries have no original ownership`. Delegated jobs
  publish their namespace into the same kernel map as applications, but
  startup only consulted the application journal. Startup now also validates
  the delegated journal (names, boot identity, cgroup inodes). Recognising an
  entry still isn't permission to clear it: that waits until the old runtime
  owners have positively retired.

### When cleanup killed the next command

The real Linux regression ran two commands through one warm container. The
first succeeded. The second died with `SIGKILL` before it did any work.

Our first cleanup wrote `1` to the task cgroup's `cgroup.kill` file, which
kills everything in the group in one go, then waited for the group to empty and
reused it. An empty cgroup looked safe. The kernel remembered something we
couldn't see. We saw this on Ubuntu's 6.8 kernel with runc 1.4; that's what we
tested, not the full list of affected versions.

To follow it, you need two pieces of Linux. The first is how the helper starts
a command. The classic way is `fork()`, then write the child's PID into the
target cgroup's `cgroup.procs`. For a moment, the child runs outside its
limits. `clone3` with `CLONE_INTO_CGROUP` closes that gap: you pass an open
descriptor for the destination cgroup directory, and the child is born there.
It needs no capability, only write access to that cgroup's `cgroup.procs`.
That's cgroup v2 *delegation*: Bun creates the task cgroup and `chown`s its
`cgroup.procs` to the helper's uid, so the helper can place children there and
nowhere else.

The second piece is how `cgroup.kill` catches a child that's being forked
while the kill sweeps through. Each cgroup has an internal counter, `kill_seq`,
which goes up on every kill. A fork compares the counter before and after; if
it moved, the new child is killed too. On affected kernels,
`CLONE_INTO_CGROUP` read the *parent's* counter (the helper's cgroup) before
the fork and the *destination's* counter after it. Our kill bumped only the
destination's. Every later child into that cgroup saw a mismatch and was
killed at birth, even with no kill anywhere near it. Emptying the group didn't
reset the counter.

There's an [upstream fix](https://kernel.googlesource.com/pub/scm/linux/kernel/git/tip/tip/+/8e359920216689b3b79e0fe8961a77fe312a511f)
that reads the destination's counter both times. We can't assume every node
has it, or a distribution backport, and a version string can't tell us. So
ordinary cleanup no longer touches `cgroup.kill`:

```c
static pid_t launch(int directory) {
    struct clone_args arguments = {.flags = CLONE_INTO_CGROUP, .exit_signal = SIGCHLD,
                                   .cgroup = (uint64_t)directory};
    return (pid_t)syscall(SYS_clone3, &arguments, sizeof(arguments));
}

static int cleanup(int fd, uint64_t sequence) {
    unsigned char receipt[9];
    if (all(fd, receipt, sizeof(receipt), 0) || receipt[0] != 'C' ||
        decode64(receipt + 1) != sequence) return -1;
#ifdef RB_EXECUTOR_HOST
    if (retire_owned_children()) return -1;
#else
    if (kill(-1, SIGKILL) && errno != ESRCH) return -1;
#endif
    int status;
    while (waitpid(-1, &status, 0) > 0 || errno == EINTR) errno = 0;
    if (errno != ECHILD) return -1;
    return event(fd, 4, sequence);
}
```

(We've trimmed a test-fixture branch that uses plain `fork()` off Linux.)
`launch` calls `clone3` through `syscall`, because older C libraries have no
wrapper for it. `cleanup` first checks that Bun's request carries the sequence
of the command that just ran. In a container, `kill(-1, SIGKILL)` signals every
process in the helper's PID namespace except PID 1 itself, which covers a
background child that changed its process group or its uid. The `waitpid` loop
reaps them all until the kernel says `ECHILD`, no children left. Only then does
the helper send event 4, the cleanup receipt, and Bun checks the cgroup is
empty.

`cgroup.kill` still has a job: retiring a whole executor after a timeout,
cancellation or uncertain cleanup. Then the killed task cgroup must never run
another command. Our first version intended that but didn't enforce it. It
ignored a failed `rmdir`, and the next executor's `create_dir_all` quietly
adopted the surviving directory, poisoned counter and all. Now retirement
doesn't count until the task cgroup is really gone, and setup creates it with
`create_dir`, which fails rather than adopting an existing one. A root-gated
test plants a killed cgroup at the next executor's path and checks the command
runs in a fresh directory. A mocked launch would never have found any of this.

### Native host executors

Shared containers fixed the container setup. Host jobs still paid for their
own: each attempt launched a new durable owner, published its ownership record
and polled for the outcome. On rootful Linux, host `exec` and `script` jobs now
borrow the same bounded pool, with a host build of the same helper
(`-DRB_EXECUTOR_HOST`). The compatibility key drops the image and keeps the
namespace, environment and resource profile, so different allowlisted binaries
can share a slot.

The resource story is the same. Each command is born into its limited task
cgroup with `CLONE_INTO_CGROUP`, so CPU, memory, swap and PID limits apply
before user code runs. The helper lives in its own charged cgroup and doesn't
eat the command's allowance. A durable owner holds the helper for the life of
the slot, and the existing group-commit task ledger records each command's
outcome, so there's no new owner record per command. Other platforms keep the
original one-owner-per-attempt backend and refuse explicit CPU or memory
limits, since they can't enforce them.

What's different is that this is *trusted* host execution with resource
controls, not a sandbox. Commands run as Bun's user, with the host filesystem.
That made us look hard at what the helper itself is allowed to do, and the
answer was "too much" in three places:

- **Capabilities.** The container helper keeps `CAP_SYS_ADMIN` and
  `CAP_SETPCAP` to give each command private mounts. Inside a user namespace
  those are harmless. Our first host helper kept them too, and on the host
  they're real: `CAP_SYS_ADMIN` alone lets you mount filesystems. The host
  helper now keeps only `CAP_SETUID`, `CAP_SETGID` (to switch to Bun's user)
  and `CAP_KILL` (to clean up). The gated test reads `CapEff` and `CapPrm` from
  `/proc/<pid>/status` and expects exactly those three bits.
- **Environment.** The helper `exec`s with exactly the environment it's sent.
  Our first version sent only the job's own variables, so `/usr/bin/env`
  printed nothing on Linux, while the same job on macOS inherited everything
  Bun had, cloud credentials included. Every host backend now calls one
  function, `host_environment`, which keeps a short allowlist of Bun's
  variables (`PATH`, `HOME`, the locale and a few more) and lays the job's
  `env` over it. The in-process backend calls `Command::env_clear()` first,
  because Rust's `std::process::Command`, like `subprocess` in Python,
  otherwise inherits the parent's whole environment. Clearing has a cost if
  you miss a caller, and we did: the owner's exec gate now cleared its
  environment too, but `relish exec` built its owner record with an empty map,
  so an exec'd `/usr/bin/env` printed nothing. #654's author caught it in
  review. Exec now copies the workload's own recorded environment, the
  in-memory backend's exec applies the same function, and tests run a command
  found only on the workload's custom `PATH`.
  Recovery exposed a second mistake: validating that saved environment by
  rebuilding it from the recovering Bun's defaults. A restart with a different
  `PATH` rejected a valid owner before it could retire. Inherited values are a
  snapshot of the preparing Bun. Recovery now checks every explicit workload
  override against that snapshot and rejects unrequested variables outside the
  allowlist; it does not compare inherited values with the new caller. Tests
  preserve a historical `PATH` and missing `HOME`, while rejecting changed or
  missing workload values and an injected private variable.
- **The socket.** The helper's socket used to live in `/tmp` under a
  predictable name. Authentication stopped impersonation, but not another user
  creating that name first, which made Bun refuse the slot forever: a cheap
  denial of service. Sockets now live in `/run/reliaburger/host-executors`
  (mode 0711), where only root can create names. The test plants a file at
  the old `/tmp` name, owned by `nobody`, and checks the next command starts.

Cleanup is harder without a PID namespace. `kill(-1, SIGKILL)` on the host
would signal every process Bun's user owns. So the host helper makes itself a
*subreaper* with `prctl(PR_SET_CHILD_SUBREAPER)`: orphaned descendants are
re-parented to it rather than to init, even after a `setsid`. Its
`retire_owned_children` reads its own children from
`/proc/self/task/<pid>/children`, opens a *pidfd* (a file descriptor that
refers to one specific process) for each and signals through it, then reaps
and repeats until none are left. A pidfd can't be fooled by PID reuse, and
because the helper is single-threaded and hasn't reaped the child yet, the PID
it read is still that child. Then the same cleanup receipt and empty-cgroup
check apply. Losing Bun's connection makes the helper retire its children
before it exits.

One more host bug was about output. The helper relays stdout and stderr with
`poll`, at most sixteen 4 KiB reads per stream per pass, so a flooding command
can't starve the exit check. After the command exited, our first version made
just one more pass. A pipe holds 64 KiB by default, so that looked like plenty,
but a command can grow its pipe to a megabyte with `fcntl(F_SETPIPE_SZ)`. One
that wrote 500,000 bytes and exited lost most of them, and still reported
success. Now the helper reads each stream to end-of-file after exit. If a
background descendant still holds the pipe open, it stops once that's been
quiet for 10 ms or after 16 MiB, and appends a visible
`[reliaburger: output written after exit truncated]` line. The test makes the
race deterministic: it sends `SIGSTOP` to the helper while the command fills
its pipe and exits.

That fix had a bug of its own, and only a benchmark found it. Each loop pass
reads the pipes and then checks whether the command has exited. For a quiet
command both pipes are already at end-of-file in the pass that sees the exit,
but the "both streams closed" check ran only after the next `poll`. With the
command gone, that `poll` waits its full 10 ms. Ten milliseconds sounds
harmless, but a `busybox true` takes well under one, so every command now cost
about 11 ms. Host jobs fell from the roughly 4,000 a second we'd measured
before to 1,233 a second, the same in every round. The check now runs before
polling. A gated test times 200 quiet commands on one warm executor: 2.35
seconds with the bug, 0.17 seconds without, against a two-second limit.

None of this keeps a *model* loaded. Each command is still a new process that
loads whatever it loads. Keeping a model resident between requests is a
separate piece of work, #641.

### Feeding fast workers

With commands this cheap, the bottleneck moved. Workers finished their durable
completions fast, but public jobs crawled. The cause was the leader's grant
loop. Recall that a node holds a couple of granted chunks at a time and asks
for more as it finishes. Each one-second control tick, the leader accepted the
node's receipts, and only on the *next* tick did it deliver the grants that
replaced them. A fast worker emptied its chunks and spent most of each
two-second cycle waiting.

The fix is *grant lookahead*: give fast nodes enough queued work to cover the
gap. How much is enough? That depends on how long commands take, so the leader
learns it from the final-attempt duration histogram the nodes already report.
Bucket `i` counts commands that took up to `2^i` ms. Using the upper bound of
each bucket gives a conservative estimate of throughput: slots × two seconds ÷
average duration. Learned depth is capped at sixteen chunks.

That's the idea. The edges took four more fixes:

- **A slow start mustn't stick.** The last bucket has no upper bound, and our
  first version fell back to the small window if it held a single sample. One
  cold image pull switched lookahead off for the rest of the array, and since
  the counts only grew, it was never forgotten. The planner now uses a second,
  *decaying* histogram, and falls back only when more than one in sixteen
  recent samples overflowed.
- **The tail must be shared, by capacity.** `plan_grants` tops up the
  emptiest node first. Near the end of an array, the first fast node could
  take sixteen of the last twenty chunks while another sat idle. Our first
  cap split what was left evenly, and #654's author showed in review that this
  is wrong too. With 27 slots on one node and 8 on the other, the last twenty
  chunks went 10/10, so the bigger node finished early and the smaller one
  owned half the tail. The cap is now each node's share of every outstanding
  chunk, queued or already held, in proportion to its slots: 15/5 in that
  example. Counting held chunks means a node still working through a big
  grant doesn't get more on top. Rounding each share up would hand out more
  chunks than exist, and rounding down would strand some. So the whole parts
  go out first, and the leftover chunks go to the largest fractions (the
  *largest remainder* method that some countries use to share out
  parliamentary seats). The baseline window still applies, so even a node
  with a thousandth of the capacity gets two chunks.
- **Lookahead needs automatic replay.** If a node dies, every chunk it held
  has an unknown outcome. Arrays that replay automatically don't mind. Arrays
  that need an operator to acknowledge and replay would turn sixteen chunks
  into manual work. Those keep the baseline window, and the leader passes the
  run's policy to `plan_grants` as a plain `bool`.
- **The durations must be honest.** The executor used to time the whole
  `runner.run` call, which for a pool includes waiting for a slot and, on a
  cold start, an image pull. A 5 ms command queued behind 256 callers reported
  50 to 200 ms. `Attempt` now carries `ran: Option<Duration>`, filled from the
  helper's start receipt to its exit receipt. `None` says "this runner can't
  tell", which is not the same thing as zero and doesn't look like a very fast
  command. Fresh containers keep the old clock: starting the container is part
  of their cost.

The decay is the part with a distributed-systems twist:

```rust
while self.recent_duration_counts.iter().sum::<u64>() > RECENT_DURATION_SAMPLES {
    for recent in &mut self.recent_duration_counts {
        *recent /= 2;
    }
}
```

Once the recent histogram holds more than 4,096 samples, every bucket is
halved. That's exponential decay, the same thing an exponentially weighted
average does with a factor like 0.9. Why not use a float? Because this state
lives in Raft. Every replica applies the same receipts and must end up with
byte-identical state, and integer division gives the same answer on every CPU
and compiler. Floating-point rounding mostly would, but "mostly" isn't a word
you want near a replicated state machine. `&mut self.recent_duration_counts`
iterates over mutable references to the array's elements, and `*recent`
dereferences each to update it in place. The planner's arithmetic then runs in
`u128`, so multiplying sixteen `u64` counts by durations can't overflow. This
is pure planning from committed state. The decaying histogram is the one new
durable field, so it rode on this release's existing state-format bump.

Lookahead only works if nodes advertise honest capacity, since queued grants
still wait for the node's concurrency and CPU/memory admission. Our first
version counted every caller inside `runner.run` as running, including callers
still waiting for a pool slot. A node whose budget fitted two executors
advertised 32 slots.

The first fix swung too far the other way. It counted commands between the
helper's start and exit receipts. A `busybox true` lives for about a
millisecond, so a node running 27 executors flat out would sample only the few
commands caught mid-flight, and advertise far less than it was doing. (We first
blamed this for a benchmark plateau; the drain bug above was the real cause, but
the under-count is real too.) The right count sits between the two. Each
pool slot records which run's caller holds it, from the moment resources are
charged to it until release, setup and cleanup included. A caller waiting for
a slot or for admission doesn't count, and a millisecond command does. One regression fills a two-executor budget with 64
two-second commands and checks the node advertises two. Another stops the
helper with `SIGSTOP` so a submitted command holds its slot without starting,
and checks the slot counts as busy while no command counts as started.

The started-command count still has a job: it feeds `relish batch watch`,
which shows verified commands beside other attempts. A backend without start
receipts, including fresh containers, reports it as unknown rather than
guessing from a launcher PID.

The price of lookahead is ownership. More granted work may need reconciliation
or replay after a worker is lost. That window is bounded, and stale attempts
keep their existing fences.

### What the measurements do and don't show

Measuring this turned out to be as instructive as building it, mostly because
of the measurements that lied. The numbers live in two records: the
[earlier matched and one-hour runs](../qualification/2026-10-09-host-job-executors/README.md)
and the [current equal-minute scenarios and concurrency sweep](../qualification/2026-10-09-timed-job-scenarios/README.md).
Here's what we learned reading them.

**Compare like with like.** The first landing-page experiment made host jobs
look ten times slower than shared containers. It also reserved a whole CPU for
each host job and a tenth of one for each shared command, so on a four-CPU node
the two paths ran different numbers of commands at once. Every path now uses
the same CPU request, limit, memory and concurrency.

**Equal counts aren't equal work.** A million raw processes and a thousand
fresh containers take wildly different times, so the totals can't be lined up.
The demonstration now gives every path the same sixty seconds and credits only
outcomes the leader accepted before the cutoff. A minute's count times 1,440 is
a daily *projection*, and we label it as one.

**Receipts have a granularity.** The first equal-minute run reported zero
fresh-container successes despite real progress. Fresh containers couldn't
finish a thousand-job receipt chunk inside a minute, and an unfinished chunk
earns nothing. Fresh runs now use one-job chunks; fast paths keep a thousand.
That changes reporting, not resources. An empty accepted window now fails the
harness instead of quietly printing zero.

**Cold and warm are different questions.** Each concurrency point starts with
cold executors and a warm image, then measures a cold minute and a warm minute
in the same submission, subtracting the counters at the boundary so no job is
counted twice.

**The raw baseline isn't a floor.** Raw `fork` and `exec` in the VM have no
limits, durability or outcomes. Adding the same cgroup limits to the raw path
made it *slower* than our native executor, because the raw runner moves each
child into its cgroup from a large Rust parent, while the helper clones
straight into it from a tiny one. So you can't subtract one rate from another
and call the difference "scheduler overhead". Isolation and durability have
real work to do.

**A no-op measures overhead.** Every run used BusyBox `true`. That isolates
per-job cost, which is exactly what this work attacked, but it says nothing
about a command that does real work. Size a real workload with real commands
on your own hardware.

**More concurrency isn't always faster.** Twenty-seven slots came from
admission arithmetic: what fits in the VM's job budget. The sweep from 1 to 64
showed host jobs plateau from 8 and fall off past 32, while shared containers
liked 27 once warm. We keep 27 as a common comparison point because it makes
the comparison fair, not because it's the best setting for each runtime. The
host plateau also says the next limit is work supply through grants and
receipts, not process creation.

**A minute isn't an hour.** Each path then ran for an hour at admitted speed,
one after another, beside the same live application. No path had a terminal
failure and the application answered every probe. The hours also showed how
much a cold minute understates a warm pool: shared containers averaged more
than three times their first-minute rate over the hour. And the VM's CPU was
only a little over half busy for the public paths, against more than 90% for
raw processes, so the next limit is feeding work and accepting receipts, not
starting processes.

**An hour is not a day.** Storage is the unfinished part. Free disk in the VM
fell to about 200 MiB by the end of the host hour, and every bounded storage
scan was incomplete, so none of this proves storage stays bounded. A fixed pool
of 32 live slots doesn't bound history either: slot identities include the
namespace, so rotating through new namespaces leaves retired ownership and
routing journals behind. The fresh-container hour also recorded four retries
that later succeeded, and we couldn't say why: a bulk success record keeps the
final outcome and the attempt count, not the earlier failure's reason. At this
volume you want bounded summaries of failure causes, not millions of log lines
for successful jobs. A real daily run, faults and collection of that history
remain in #668.

**Benchmark your own fixes.** The review that hardened this code was checked
by rerunning the benchmarks on the fixed build, and the reruns found three
regressions in the fixes themselves. One was the 10 ms drain wait above. The
second was a single line. Bounding retirement had made the "is the task group
empty?" check async, and the read moved to `tokio::fs::read_to_string`. Tokio's
file functions hand each call to a pool of blocking threads, because ordinary
file I/O can stall a thread. That's the right default for a disk. But
`cgroup.events` lives in cgroupfs, which, like `/proc`, is answered from kernel
memory and never waits. The read happens once per command, and the thread hop
cost about 3% of host-job throughput. It's a plain synchronous read again,
with a comment saying why.

The third was memory. Bun's resident memory after five minutes of host jobs
moved by up to 170 MiB between builds, with changes that had nothing to do
with memory. Reverting them one at a time never brought it back to #654's
figure. The cause was glibc's allocator. To avoid lock contention, glibc gives
busy threads their own *arenas*, up to eight per core, and keeps freed memory
in each arena for reuse rather than returning it to the kernel. Tokio's worker
and blocking threads all count. So resident memory tracked how many threads
had ever been busy, not how much data Bun held. Go and Python manage their own
heaps, so you meet this mostly in C, and in Rust, which uses the system
allocator by default. Setting `MALLOC_ARENA_MAX=2` brought the same run down to
about 190 MiB. Bun now does it for itself, before the runtime starts any
threads:

```rust
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn limit_malloc_arenas() {
    if std::env::var_os("MALLOC_ARENA_MAX").is_some() {
        return;
    }
    // SAFETY: mallopt only tunes the allocator, and runs here before Bun
    // starts any other thread. A value glibc rejects is reported by the return
    // value, which we can ignore: the default arenas simply stay in place.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}
```

`#[cfg(...)]` compiles the function only on Linux with glibc. musl and macOS
have different allocators and no such knob. Calling a C function is `unsafe`
in Rust because the compiler can't check what C does. The `// SAFETY:` comment
records why this call is fine, and an operator's own `MALLOC_ARENA_MAX` still
wins. Bun now ends the same run at 165 MiB, against 587 MiB for #654, and
accepts at least as many jobs: its hot paths wait on I/O, not on the allocator.

**Keep the failures.** One rerun of the direct matrix forgot its private
hostname wrapper, collided with the live node and produced a thousand startup
failures. The record keeps it beside the successful rerun. Runs use the normal
three attempts and report accepted retries separately, because a retry that
succeeds hides its cause rather than fixing it.

**Put the experiment in the tool.** Reproducing a benchmark shouldn't mean
remembering a dozen paths from someone's temporary directory, so the four
scenarios are now part of Relish. `relish bench --scenario
jobs-shared-containers` runs with sensible defaults; there are scenarios for
fresh containers, host processes and the raw baseline too. The runner counts
through an `AcceptedCounts` value that refuses a counter going backwards or a
changed total, so polling twice can't turn one success into two. The active
submission is an `Option`: `None` means we may submit, `Some` means we must
keep watching the work we own. Ctrl-C triggers a `CancellationToken` that stops
submission and observation without skipping cleanup. The raw baseline keeps its
child waiters in a Tokio `JoinSet`, a set of spawned tasks you can await as
they finish, and drains exits after the deadline without crediting them.

Adding the benchmark's flags taught a Rust lesson of its own. Clap's derive
macro generates the parser from the command enum, and with every benchmark
option inline the generated code overflowed the test threads' default stack,
in tests for unrelated commands too. A Rust enum is as large as its largest
variant, and that size lands on the stack wherever the value is built. The
options moved into their own `#[derive(clap::Args)]` struct, held as
`Bench(Box<BenchOptions>)`. `Box<T>` puts the value on the heap and stores only
a pointer, so every variant shrank back to a few words. When you add a
specialised command, keep running the ordinary command tests: generated code
can change their stack use too.

Inside each pool, admission, cold setup, command execution and cleanup each
record into a fixed sixteen-bucket histogram of `AtomicU64` counters, so
concurrent slots record without taking a lock. A small guard records the
elapsed time in its `Drop` implementation, Rust's destructor, which runs on
every exit path, error returns included. Its lifetime spans exactly one phase,
so waiting for a slot can't leak into execution time.

### Soak jobs while the apps keep running

A fast command completing a million times tells us little about what happens
when Bun dies halfway through owning it. The release soak already kills agents,
powers off nodes and loses quorum while data-bearing apps run. It now drives
all three job runtimes through the same faults, within the existing 90-minute
fast and eight-hour final tiers.

The [extension plan](../plans/2026-10-10-plan-release-job-soak.md) describes a
bounded controller with two resource profiles per runtime. It persists each
submission intent before sending it, then reuses that exact request ID after
a lost reply or controller restart. Accepted counters must conserve indexes
and never regress. A small authenticated verifier on each VM independently
records the audited cohort's logical effects; repeated attempts are counted,
not claimed as exactly-once execution. Cron, publication-triggered singletons
and deploy gates use the same common job path. Fresh containers do not expose live
command activity through the summary API, so their fault-overlap proof joins an
independent boot/start-time receipt to the current private generation and a
populated cgroup. Counting queued callers as active would fabricate coverage.

The controller deliberately tests non-zero exits, deadlines with descendants,
memory limits and cancellation. Long jobs opt out of automatic replay. If a
fault leaves an unknown outcome, the test operator acknowledges only a known
replay-safe fixture, after observing its exact grant fingerprint twice.
That explicit decision is part of the evidence, not a production recovery
policy.

Reusable executors outlive their commands, so the app-only leak checker needed
to change. It now recognises an exact executor only when its private journal,
current boot, generation, pool slot and cgroup identity agree. A prefix is no
proof. Complete disk inventories and actual cgroup limits accompany the owner
proof; they do not substitute for the scheduler's commitment ledger. Missing
job evidence, stale heartbeats, orphaned owners or incomplete drain fail the
release verdict. Drain begins inside the last two minutes, with no new settle
allowance.

This machinery still needs the normal staged fast and final qualification on
the integrated candidate. It records reliability under app load and faults;
the hourly saturation measurements and #668's retained-storage/24-hour work
answer different questions. No extrapolated daily rate determines a soak pass.

### Lessons from short jobs

**Reuse needs proof, not hope.** Every reuse bug in this section had the same
shape: a slot looked empty, so we reused it. An exit code isn't proof the
command's children are gone. An empty cgroup isn't proof the kernel has
forgotten it was killed. A missing directory record isn't proof nothing was
published. Positive retirement, acting only on a receipt or an observed empty
state, is slower to write and much faster to debug.

**Mocks can't see the kernel.** The `kill_seq` bug, the swap that let a
memory-limited command survive, the racing image mountpoint and the oversized
pipe all needed a real Linux node. Mocks were still the right tool for the
state machines around them. They just can't fail the way a kernel does.

**Reads shouldn't wait behind writes.** Twice, an inventory snapshot queued
behind a mutation's lock and starved a control loop. A lock that deliberately
outlives an abandoned caller is the right fence for changing authority, and the
wrong one for looking.

**Distinguish "pending" from "broken".** A recovery test failed because it
read a logical run as active while its retry was still waiting for a runtime
binding, then treated the `pending` instance with no PID as a running process
missing one. The waiter now waits through non-running states but still fails
at once if a known running process has no PID. We kept the cases separate, so
fixing the test couldn't hide the real missing-PID bug it was written to catch.

**Measure the thing you claim.** The most misleading numbers here weren't
wrong. They measured something else: unequal reservations, unequal counts,
receipt chunks bigger than the window, a no-op instead of real work. Writing
down what a number *doesn't* show turned out to be most of the work.

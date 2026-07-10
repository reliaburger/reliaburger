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

The heal logic, being a plain function now, gets both kinds of coverage. Unit tests drive `plan_heal` against a fabricated catalogue: `audit_orders_rarest_first` (one-copy heals before two-copies; at-redundancy doesn't appear), `plan_heal_caps_work_per_tick`, `plan_heal_empty_when_redundancy_met`. Integration tests in `tests/pickle_cluster.rs` run real registries on ephemeral ports: `heal_tick_replicates_to_new_peer` is the roadmap's auto-heal scenario end to end (push to node 1, node 2 appears, one tick, both hold everything and the proposed update says so); `heal_tick_pulls_missing_layers_first` proves the leader-pull path (image only on node 2; after one tick the leader holds it locally and the update records `{1, 2}`); `heal_tick_respects_per_tick_cap` pushes three images and asserts a cap of one heals exactly one.

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

## A thousand jobs, fifty envelopes

The `/v1/batch` endpoint has returned 501 since Phase 8, with the batch scheduler and tracker sitting library-complete beside it. Wiring it forced the one design question this chapter had left open: *how does the leader tell another node to run something?*

The obvious answer is "the same way deploys do". Stage 4 gave deploys a Raft-placements pipeline: the leader writes desired assignments into the replicated log, and every node runs a reconciler that polls its assignments and converges — starts what should run, **stops what shouldn't**. That machinery is exactly wrong for run-to-completion jobs, and the reasons are worth spelling out. A finished job looks like *drift* to a reconciler ("assignment says run, node says stopped — restart it!"). A rebalanced assignment means *stopping* the workload on the old node — which for a job means killing work half-done, not gracefully moving a stateless replica. Reconciliation is the right tool for "keep this running"; it is the wrong tool for "run this once". So batch dispatch is direct HTTP: the leader POSTs each node's job group to `/v1/batch/run` (service-token authenticated), and running nodes post per-job completion reports back to a callback URL. Same crate, two dispatch idioms, each matching its workload's semantics — that contrast is the section's real lesson.

The rest reuses what earlier phases built. Submissions carry *full job specs* (the old CLI sent only names, which required the cluster to already know the jobs — a genuine design bug found by wiring). Followers forward submissions to the leader with the same proxy shape as `/v1/apply`. Capacity comes from the reporting pipeline's aggregated worker reports — the very data the deploy scheduler uses — mapped into the bin-packer's `NodeCapacity`; a standalone node falls back to a single effectively-unbounded local entry, which is also what makes the integration tests run without a cluster. The `BatchTracker` lives leader-side, and a target node runs its share by synthesising a `Config` containing just those jobs and pushing it through the ordinary deploy path — retries, records, and all.

### The watcher, and an ambiguous "stopped"

Each running node watches its jobs to a terminal state by polling instance status. The first version of that watcher had a bug the failure-path test caught immediately: it treated `stopped` as success. But the runtime maps *any* process exit to `Stopped` — the exit code is tracked separately — so a job that exits non-zero shows as `stopped` while it waits out its retry backoff, and the watcher happily reported it completed. The fix exposes the exit code in `InstanceStatus` (long overdue; the TUI will want it too) and makes the predicate honest: `failed` is failure, `stopped` with exit 0 is success, `stopped` with a non-zero code is *backoff, keep waiting* — the agent will mark it `failed` once retries exhaust. States that look terminal and aren't are a classic distributed-systems trap; this one was two feet from home.

Completion reports are deliberately forgiving: reporting into an unknown batch id is a recorded no-op, not an error, because a leader restart loses the in-memory tracker and late reports from running nodes must not explode. The trade — batch status is best-effort across leader changes — is accepted and written down. `relish batch` now prints the batch id; `relish batch-status <id>` shows the tracker's live summary.

## Building where the tools are

The build endpoint arrived with the wiring merge in a working-but-synchronous form: fetch the context blob, run `buildah bud` and `buildah push`, answer when done. Its own comment admitted the flaw — it "matches the CLI's 300 s client timeout". Real image builds routinely take longer, and a build that outlives the HTTP request strands the client mid-response with the build still running. Client timeouts are a design forcing function: any operation that can exceed them must become *submit-and-poll*, and pretending otherwise just moves the failure to the worst possible moment.

The refactor keeps the handler body — it was correct — and lifts it into a spawned runner behind a registry of build states: `POST /v1/build` answers `202 { build_id }` immediately, `GET /v1/build/{id}` reports `running`, `completed`, or `failed` with the last useful stderr, and each `buildah` stage now sits under `[images] build_timeout_secs` so a hung build terminates instead of squatting forever. `relish build` polls. Note what this tracker is *not*: unlike batches, there's no leader-side registry and no forwarding — a build lives on the node that accepted it, because there's nothing to aggregate.

The second gap was placement. A macOS node — or any node without `buildah` — used to answer 501. Now capability travels with the state reports: each worker probes `buildah --version` once at startup and carries a `has_buildah` flag in every report, and a submit on an incapable node *delegates* — it picks a capable peer from the aggregated reports, forwards the request to that peer's `/v1/build/run`, and records the build locally as `Delegated { url, remote_id }` so status reads proxy through. The client polls one node and never learns the build ran elsewhere. No builder anywhere is an honest 503 naming the actual problem.

Completion closes the trust loop from Phase 10: after the push lands the image in the local registry (real holders, catalog persistence, Raft propose — all through the standard push handlers), the runner looks up the manifest digest and asks the agent to sign it. Best-effort, deliberately: a standalone node has no council to sign with, and the failure mode is a printed warning telling you exactly what won't work (`require_signatures` deploys). The full circle is worth saying out loud: **build → push → replicate (heal loop) → sign → verify at schedule → deploy → serve**, every arrow now a real code path. That was the point of the phase.

## Lessons from the phase

**Optimisation is an audit with a deliverable.** The single most consistent finding of this phase wasn't a speed-up. It was library-not-wired: `add_port_mapping` with no production callers, `VolumeManager` with no production callers, an HTTPS-only pull client that made cluster images undeployable, a CLI that sent job names to a cluster that had never heard of the jobs. Well-tested libraries pass review; only tracing the live path from the user's artefact to the kernel finds the missing arrow. If you take one habit from this chapter: when you're asked to optimise something, first prove it runs.

**Route distributed decisions through the place that already has an order.** The GC couldn't safely delete "one of two copies" until the *decision* went through Raft — not the deletion, the decision. The same shape appears in the heal loop's holder updates and the catalogue commits. Check-then-act across machines is always a race; a total order is the only fix, and you usually already have one.

**Match the dispatch idiom to the workload's lifecycle.** Deploys reconcile (converge on desired state, forever); jobs dispatch (run once, report, done). Stage 4's placements machinery is exactly right for the first and destructively wrong for the second — a completed job looks like drift, a moved assignment kills running work. One crate, two idioms, both correct.

**States that look terminal and aren't.** `stopped` meant "success", except when it meant "failing job between retries". `cache/redis:7` meant the same image as upstream, except when the tag had moved. A snapshot named like a directory was one, except it needed `subvolume delete`. Every one of these was caught by a test that checked the *unhappy* path — and the failing-job watcher bug went from written to caught to fixed inside an hour because the failure test existed before the fix.

**Say what you didn't do.** Cached upstream images are unsigned and exempt from trust policy — written at the exemption. The registry has no auth, so the loopback warning explains the firewall posture. Rootless proxies don't survive adoption; local-origin traffic bypasses the DNAT map; the whitepaper still describes synchronous replication. Each gap is recorded where a maintainer will trip over it. Honest edges are cheaper than surprises.

The roadmap's milestone for this phase reads: port mapping uses O(1) nftables maps, images download from multiple peers in parallel, logs compress 5× with random-access reads. All true. The truer summary: the paths those words describe now actually exist, end to end, with tests standing on each one.

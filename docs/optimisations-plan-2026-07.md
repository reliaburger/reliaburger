# Phase 12 Implementation Plan — Optimisations ("Squeezing Every Drop")

**Date:** July 2026. **Executor note:** this plan is prescriptive; follow it step by step and do not skip the tests-first ordering. All paths are relative to the repo root. Verified against the codebase as of 2026-07-06 on branch `stage-3b-transport-hmac-signatures` (`src/pickle/` ~4,100 lines across 9 files, `src/grill/netns.rs` 923 lines, `src/grill/volume.rs` ~270 lines, `src/meat/batch.rs` + `batch_tracker.rs` library-complete, council state machine already applies Pickle Raft commands).

Two Phase 12 items are **already done — do not redo them**:

- Parquet bloom filters on `app`/`namespace` (1% FPP) — `src/ketchup/log_store.rs:47-64`, tests at `log_store.rs:663` onwards.
- Zstd compression for archived logs via Parquet's per-row-group ZSTD codec — same file, `zstd_parquet_is_over_5x_smaller_than_raw_text` and `zstd_archive_round_trips_through_remote_query`.

The book chapter (`docs/book/12-squeezing-every-drop.md`) already contains the logs section. Every step below names the book section it must add — write it in the same commit, not at the end.

## 0. Ground rules (non-negotiable)

1. **Tests first.** Every step lists its tests; write them, watch them fail, then implement.
2. **`make ci` before every commit** (fmt --check, clippy -D warnings, test). No exceptions.
3. **Ask the user before every commit** — show the commit message and summary first. Never amend; always new commits.
4. **Book alongside code.** Each step updates `docs/book/12-squeezing-every-drop.md`. The audience knows C/Python/Go, not Rust: explain each new Rust concept on first appearance (this phase introduces `JoinSet`, `statfs` via nix, proptest strategies, feature-gated dependencies).
5. **Update `docs/progress.md`** (Phase 12 section, lines 321-333) as items complete; update `docs/README.md` and the top-level `README.md` (test counts, new CLI commands, new endpoints) at the end of the phase.
6. **British English** in prose and doc comments; serde derives stay American.
7. **No `unwrap()`/`expect()`/`panic!()` in production code**; thiserror enums per subsystem; anyhow only in binaries.
8. **tokio-only sync primitives; `CancellationToken` shutdown** for every new long-lived task.
9. **tokio `start_paused` pitfall:** never combine `tokio::spawn` with a paused clock in tests — drive loops manually with `try_recv`/explicit ticks (this bit us before; see the scheduler tests in step E3).

## 1. What Phase 12 delivers

Six work slices, ordered:

| Slice | Item | Roadmap line |
|-------|------|--------------|
| A | nftables named maps for O(1) port mapping | roadmap.md:586 |
| B | Pickle catalog via Raft + replication on push + GC/audit loops | prerequisite for C/D; subsumes review L10/M2 |
| C | P2P multi-source image downloads (rarest-first, parallel, dedup) | roadmap.md:587 |
| D | Pull-through cache for external registries | roadmap.md:588 |
| E | Managed-volume wiring (M21), Btrfs subvolume quotas, volume snapshots + scheduled upload | roadmap.md:589-590 |
| F | Wire `SubmitBatch` and `SubmitBuild` into the agent | progress.md:323-324 |

**Milestone (roadmap.md:597):** port mapping uses O(1) nftables maps, images download from multiple peers in parallel, logs compress 5x with random-access reads (already done). All Phase 12 tests pass.

## 2. Scope decisions (user-confirmed — do not relitigate)

1. **Batch and build wiring are in scope** for this plan (steps F1/F2), even though they're "wiring" — the user confirmed both.
2. **Linux-only features (nftables maps, Btrfs) are tested via the Lima setup.** Unit tests for command/rule generation run everywhere; live tests are env-gated and run inside the test VM via `relish dev test` (`src/relish/dev.rs:760-819`), which already exports `RELIABURGER_RUNC_TESTS`, `RELIABURGER_NETNS_TESTS`, `RELIABURGER_EBPF_TESTS`. This phase adds `RELIABURGER_BTRFS_TESTS` and `RELIABURGER_BUILDAH_TESTS` to that list, plus `btrfs-progs` and `buildah` to the VM provisioning.
3. **Volume snapshots are Btrfs-only.** On any other filesystem, snapshot operations return a clear `UnsupportedFilesystem` error. No copy fallback. Live testing on Lima against a loopback Btrfs mount the tests create themselves.
4. **Snapshot upload targets the `object_store` abstraction** (already a dependency, `Cargo.toml:61`). Tests run against the local filesystem backend; real S3/GCS is a manual acceptance step. Enable the `aws` and `gcp` features so `s3://`/`gs://` URLs genuinely work (this also unblocks fixing review M20 for log export later, though that stays in the wiring track).

Decisions made in this plan (documented with rejected alternatives — all book material):

5. **P2P chunk = layer.** No sub-layer HTTP range splitting this phase. The roadmap's "chunk selection" tests operate at layer granularity. Rejected: range-splitting layers > 100 MB across peers — real complexity, marginal win at our layer sizes; note it as future work in the book.
6. **Batch/build dispatch is direct HTTP** from the submitting node to target nodes' Bun APIs with the service token, mirroring the existing cross-node fan-out (`src/bun/api.rs:516-584` resolves `NodeId` → URL via the membership table and calls `fan_out_query` with `state.service_token`). Rejected: committing per-node allocations to Raft (design doc §5.2) — needs state-machine changes and node-side watchers we don't have, and the O(nodes) Raft write buys nothing at current scale; rejected: the reporting tree — it is read-only by design and partly unwired (L6).
7. **Snapshot scheduling is interval-based** (`interval_secs`), not cron expressions. Rejected: the `cron` crate — it drags in chrono for a feature nobody asked for by name; the roadmap's "scheduled snapshot job runs on cron" is satisfied by a scheduled loop. Note the trade-off in the book.
8. **Cached external images are exempt from `require_signatures`.** Today the scheduler passes external (non-Pickle) images through unverified (`src/meat/scheduler.rs:181-200` returns `Ok` when the manifest isn't in the catalog). Once the pull-through cache stores them in Pickle they would suddenly fail a `require_signatures = true` policy. Exempt manifests whose repository starts with `cache/` — they are upstream content and were never signable by us. Upstream trust policy (digest pinning, cosign verification of upstream sigs) is future work; say so in the book.
9. **Uniform `registry_port` across the cluster.** Gossip carries only `address` (`NodeStatus` in `src/bun/agent.rs`); peers' Pickle URLs are derived as `http://<gossip-ip>:<registry_port>` using the local node's configured port. Document this constraint in `docs/README.md`.

## 3. Verified ground truth (trust this; re-verify only if compilation disagrees)

### 3.1 Port mapping today

- `src/grill/netns.rs:352-430` `add_port_mapping()`: root mode runs `nft add rule ip reliaburger prerouting tcp dport {hp} dnat to {ip}:{cp}` — **one rule per container port**, O(n) chain traversal. Rootless mode spawns a tokio TCP proxy (untouched by this plan).
- Rule removal (`netns.rs:520-563`) lists all rules with `nft -a list`, parses handles from text. Fragile and O(n); the map design removes it entirely.
- `ensure_nft_table()` (`netns.rs:459-518`) creates table `ip reliaburger`, chains `prerouting` (NAT hook, priority -100) and `postrouting` (masquerade). Idempotent.
- `nft` is invoked via `tokio::process::Command::new("nft")` (`netns.rs:524`); pure rule-text helper `nft_dnat_rule()` at `netns.rs:659` with unit tests at 806/812.
- **C4 guard (critical):** the perimeter firewall uses a *separate* table `reliaburger_fw` (`src/firewall/rules.rs:73-146`) and its reconcile deletes/recreates only that table. The test `ruleset_uses_isolated_table_name` (`rules.rs:241-253`) asserts it never touches `ip reliaburger`. Nothing in slice A may weaken this.

### 3.2 Pickle today

- **Catalog is Raft-capable but the registry doesn't use Raft.** The council state machine applies `ManifestCommit`/`UpdateLayerLocations`/`GcReport`/`DeleteTag`/`AttachSignature` (`src/council/state_machine.rs:86-95,153`; `RaftRequest` variants at `src/council/types.rs:106-113`) and exposes `council.manifest_catalog()` (`src/council/node.rs:202`, test at 387). But the registry API (`src/pickle/api.rs`) keeps a node-local `Arc<RwLock<ManifestCatalog>>` created as `ManifestCatalog::default()` at boot (`src/bin/bun.rs:472`), applies commits locally with hardcoded `holder_nodes: {0}` (`api.rs:339,376,462`), and loses everything on restart. Review finding L10 is therefore *half*-stale: the state machine half exists; the registry half is what slice B wires.
- Replication (`src/pickle/replication.rs`: `select_peers` :46, `check_peer_has_layers` :77, `replicate_manifest` :175, `ReplicationConfig` default redundancy 2 / timeout 30s), peer pull (`src/pickle/pull.rs`: `pull_layer_from_peer` :19, `pull_manifest_layers` :70 — **sequential**, `find_peer_for_layer`, `image_available_locally`), and GC (`src/pickle/gc.rs`: `gc_sweep` :47 with sole-copy protection :99-102) are all implemented and unit-tested with **zero production callers**.
- **M2 TOCTOU (must fix in B3):** two nodes each holding one of two copies both observe `holders.len() == 2`, both delete → total loss. Also mid-push blobs (empty holder set) look "orphaned".
- Signing/verification is complete (`src/pickle/signing.rs`); verification is manifest-digest-level, enforced at schedule time (`src/meat/scheduler.rs:181-200`, called from `src/bun/agent.rs:1950-1980`). P2P moves blobs, not manifests — it cannot bypass verification. Keep it that way.
- Dead config that comes alive in this phase (`src/config/node.rs`, `[images]`): `redundancy`, `gc_retain_tags`, `gc_retain_days`, `external_registries`, `push_sync`. `registry_bind`/`registry_port` already work.
- `oci-distribution 0.11` is already a dependency (`Cargo.toml:38`) — use it for the upstream client; do not add a new OCI crate.

### 3.3 Volumes today

- `src/grill/volume.rs`: `VolumeManager::create_managed_volume()` (:41-79) creates `{volumes_dir}/{ns}/{app}/{mount_path}` and, on Linux as root with a `size` limit, `setup_loop_mount()` (:85-141): `fallocate` sparse file → `mkfs.ext4` → `mount -o loop`. **No production callers (review M21)** — mount entries are generated in `src/grill/oci.rs:285-306` but the host directory is never created, so runc bind-mounts fail with ENOENT.
- `VolumeSpec { path, source: Option<PathBuf>, size: Option<String> }` (`src/config/types.rs:294-302`); `[storage] volumes` config parsed but the agent hardcodes the default (dead config).
- Size parsing (`"10Gi"` → bytes) and `check_usage()` exist with unit tests (:190-271). No integration tests.
- Example configs: `examples/phase-1/container-volumes.toml`, `proc-volumes.toml`.

### 3.4 Object store and scheduling today

- `object_store = { version = "0.12", features = ["fs"] }` — fs only. Ketchup export (`src/ketchup/export.rs:59-111`) copies Parquet files with an `ExportCheckpoint` (JSON `HashSet` of exported names) but treats `s3://` as a literal local path (M20) and is itself unwired (H10) — both stay in the wiring track; slice E only borrows the checkpoint pattern.
- No cron infrastructure. The house pattern is `tokio::time::interval` in a spawned loop with a `CancellationToken` (e.g. `src/mayo/rollup_worker.rs`).

### 3.5 Batch and build today

- `/v1/batch` and `/v1/build` return 501 (`src/bun/api.rs:1761-1791`, routes at :184-185).
- `schedule_batch(jobs: &[BatchJob], nodes: &mut [NodeCapacity]) -> BatchAllocation` (`src/meat/batch.rs:51-116`) — greedy bin-packing, 11 unit tests including a 100k-jobs-under-1s bench. `BatchJob { name, resources }`, `BatchAllocation { assignments: Vec<(String, NodeId)>, unschedulable: Vec<String> }`.
- `BatchTracker` (`src/meat/batch_tracker.rs`, 246 lines): `register(&[(String, NodeId)]) -> BatchId`, `mark_completed/mark_failed(batch_id, job_name)`, `summary(batch_id) -> Option<BatchSummary>`, `gc(max_age)`. 6 unit tests. Library-only.
- Build library (`src/pickle/build.rs`, 585 lines): `tar_context`, `digest_of`, `validate_build`, `execute_build(spec, context_digest, pickle_port) -> BuildahJob { build_cmd, push_cmd, destination, local_tag, context_blob_digest }`, `BuildResult { destination, layers, size_bytes }`, context blob URLs under `/v2/_buildcontext/blobs/`. 24 unit tests. **X1 bug:** `relish build` uploads the context to port 9117 (Bun API — no `/v2` routes) instead of the Pickle registry port (`src/relish/commands.rs:880`).
- `relish batch <file>` sends **job names only** (`src/relish/commands.rs:924-938`, `client.submit_batch(&job_names)`); the agent would have no specs to run. F1 changes the request to carry the specs.
- Jobs run locally today via the supervisor (`WorkloadInstance.is_job`, `src/bun/supervisor.rs:55`); run-to-completion, no restart.
- **No leader→node dispatch mechanism exists anywhere** (review L1). The nearest pattern is the HTTP fan-out with membership-resolved URLs and the service token (`src/bun/api.rs:516-584`). F1 builds on that.

### 3.6 The Lima test rig

- `relish dev test [filter]` (`src/relish/dev.rs:749-819`): ensures the build VM, runs `sudo -E cargo test -j 2 --features ebpf` inside it with `RELIABURGER_RUNC_TESTS=1 RELIABURGER_NETNS_TESTS=1 RELIABURGER_EBPF_TESTS=1`, `--test-threads=1`. Env-gated tests print a skip message when the variable is absent (pattern: `tests/ebpf.rs:29,82`).
- `relish dev create --nodes 3` builds a real multi-node cluster in Lima VMs — the acceptance environment (§10).

## 4. Assumptions about the parallel wiring track

A separate effort is closing the review's wiring backlog (progress.md:298-319). This plan **does not depend on** any of it except where noted:

- L1 (scheduler remote dispatch for *deploys*) is not needed — F1 builds its own dispatch for batch/build, which L1 may later reuse.
- L6 (reporting-tree rollups with real resource usage) is not needed — F1 gathers capacity via a new `/v1/capacity` endpoint instead.
- C5 (auth middleware) is assumed present on protected routes, as it already is for `/v1/*` (`route_layer` + `auth_middleware`); new endpoints in this plan go behind the same layer.
- If the wiring track lands L10 fragments first, steps B1-B4 shrink to review-and-adopt; check `docs/progress.md` before starting each B step.

## 5. Work breakdown

Seventeen commit-sized steps. Order within a slice is mandatory; slices A and E are independent of B/C/D; F needs B (build pushes through the replicating registry) and benefits from C.

---

### A1 — nftables map generation and the planner (unit-testable everywhere)

**Tests first** (new file `src/grill/portmap.rs`, `#[cfg(test)]` at the bottom):

- `portmap_definition_syntax` — the map definition string is exactly `add map ip reliaburger portmap { type inet_service : ipv4_addr . inet_service ; }`.
- `map_rule_syntax` — the lookup rule is `add rule ip reliaburger prerouting dnat ip addr . port to tcp dport map @portmap`.
- `element_add_syntax` / `element_delete_syntax` — `add element ip reliaburger portmap { 30001 : 10.0.2.2 . 8080 }` and `delete element ip reliaburger portmap { 30001 }`.
- `apply_rolls_back_on_mid_batch_failure` — applying 3 mappings where the 2nd fails deletes the 1st and returns the error (recorded via the mock executor).
- `apply_is_incremental` — applying a second batch does not re-add existing elements (planner diffs against known state).

**Implementation** — new module `src/grill/portmap.rs` (add `pub mod portmap;` to `src/grill/mod.rs`):

```rust
/// Executes nft commands. Two implementations: the real one shells out,
/// the test one records argv and returns scripted results.
pub trait NftExecutor: Send + Sync {
    fn run(&self, args: &[String]) -> impl Future<Output = Result<(), NetnsError>> + Send;
}

pub struct PortMapEntry {
    pub host_port: u16,
    pub container_ip: Ipv4Addr,
    pub container_port: u16,
}

pub fn portmap_definition() -> Vec<String>;
pub fn map_rule() -> Vec<String>;
pub fn element_add(e: &PortMapEntry) -> Vec<String>;
pub fn element_delete(host_port: u16) -> Vec<String>;

/// Adds all entries; on failure deletes the ones already added and
/// returns the first error.
pub async fn apply_port_mappings<E: NftExecutor>(exec: &E, entries: &[PortMapEntry]) -> Result<(), NetnsError>;
```

The trait is justified (two real implementations: production + recording mock). Return argv vectors, not shell strings — `nft` joins argv itself, which sidesteps brace-quoting bugs. TCP only, matching current behaviour; note UDP maps as future work in the book.

**Book:** new section "One rule to map them all" — what an nftables named map is, why chain traversal is O(n) and a map lookup is O(1), the analogy to Onion's eBPF `backend_map`, and the argv-vs-shell-string quoting lesson. First appearance of a trait with an `impl Future` return — explain it against Go interfaces.

---

### A2 — switch `netns.rs` to map elements; Lima stress test

**Tests first:**

- Update the pure-function tests in `netns.rs` (:806-818) — `nft_dnat_rule()` is deleted; its callers move to `portmap::element_add`.
- Integration (Lima, `RELIABURGER_NETNS_TESTS=1`, root): update the existing ignored `port_mapping_nftables` test to assert traffic still DNATs through the map; add `portmap_map_handles_1000_ports` — insert 1000 elements, `nft list map ip reliaburger portmap` contains them all, delete all, map is empty. Keep the whole test under ~30s.
- Keep `firewall/rules.rs::ruleset_uses_isolated_table_name` green — do not touch `reliaburger_fw`.

**Implementation:**

1. `ensure_nft_table()` additionally creates the map and the single lookup rule, idempotently (probe with `nft list map ip reliaburger portmap`; create on non-zero exit — mirror the existing chain-probe style).
2. **Legacy cleanup:** after ensuring the map rule, list `prerouting` once and delete any leftover per-port `dnat to` rules that aren't the map rule (upgrade path from the old scheme). This is the last time we ever parse `nft -a list` output; delete `remove` handle-parsing (:520-563) afterwards.
3. `add_port_mapping()` root path calls `portmap::element_add`; `PortMapHandle` now stores just `host_port`; teardown calls `element_delete` — O(1), no listing.
4. Production `NftExecutor` impl wraps the existing `tokio::process::Command::new("nft")` call site (:524).

Rootless TCP-proxy mode is untouched.

**Book:** extend A1's section with the migration story (running clusters have per-port rules; the one-time sweep) and the C4 war story — why container NAT and the perimeter firewall live in separate tables and what happened when they didn't.

---

### B1 — Pickle catalog through Raft

**Tests first:**

- `src/pickle/api.rs` unit/tower tests: pushing a manifest with a council handle present proposes `RaftRequest::ManifestCommit` and does **not** touch the local catalog; without a council it applies locally (standalone fallback — existing tests keep passing).
- Council-level test (pattern: `src/council/node.rs:387` `manifest_catalog_returns_the_replicated_catalog`): a `ManifestCommit` written on node A is visible via `manifest_catalog()` on node B.
- Integration: restart survival — push, restart the council node (redb-backed store), catalog still lists the manifest.

**Implementation:**

- `PickleState` (`src/pickle/api.rs:24`) gains:

```rust
pub struct PickleState {
    pub store: BlobStore,
    pub catalog: Arc<RwLock<ManifestCatalog>>,   // standalone fallback + read cache
    pub council: Option<CouncilWriteHandle>,     // the same handle type ApiState.council carries
    pub node_id: u64,                            // real Raft node id, not 0
}
```

- `manifest_put` (three call sites building `ManifestCommit` at :336, :373, :459): set `initial_holders = BTreeSet::from([state.node_id])`; when `council` is `Some`, `council.write(RaftRequest::ManifestCommit(commit)).await` and rely on the state machine to apply; when `None`, apply locally exactly as today.
- Reads (`manifest_get` :503, `tags_list` :547): prefer `council.manifest_catalog().await` when clustered, else the local catalog.
- `src/bin/bun.rs:583`: pass the council handle and node id into `PickleState`. The agent's signature-verification path (`agent.rs:1950-1980`) likewise prefers the council catalog when available.
- Non-leader writes: `council.write` forwards to the leader (existing openraft behaviour) — if the current handle doesn't forward, return 503 with a `Retry-After` and note it; do not build leader-forwarding here.

**Book:** new section "Making the registry cluster-aware" — why manifest metadata belongs in Raft (small, needs consensus) while blobs stay on disk (big, content-addressed, self-verifying). Explain `Option<Handle>` as the standalone/clustered seam.

---

### B2 — replication on push

**Tests first:**

- Unit: peer-URL derivation from a membership snapshot (`NodeStatus.address` + local `registry_port`), self excluded.
- Integration (`tests/pickle_cluster.rs`, new): two in-process registries (two `BlobStore`s in tempdirs + two routers on ephemeral ports). Push a 3-layer image to A with B as a peer and `redundancy = 2`: B ends up with all layers (`has_blob`), and the committed holders set is `{A, B}`. Second test: peer down → push still succeeds with holders `{A}` and a logged warning.

**Implementation:**

- Bring `[images] redundancy` and `push_sync` to life (`src/config/node.rs` — they parse already).
- `PickleState` gains `peers: Arc<RwLock<Vec<Peer>>>`; `src/bin/bun.rs` populates it from the membership table on change (same watch the API uses) mapping to `http://{ip}:{registry_port}` per decision §2.9.
- In `manifest_put`, after blobs are verified locally and before the Raft commit: `select_peers` → `replicate_manifest` (both exist, `replication.rs:46,:175`) → `initial_holders = {self} ∪ succeeded_peers`. Push succeeds if the local copy is intact even when all peers fail — the audit loop (B4) heals.
- `push_sync = false`: commit with `{self}` immediately, spawn a background task that replicates and proposes `UpdateLayerLocations { added }` on success.
- Factor the "replicate then commit" sequence into `commit_and_replicate()` — D2 reuses it for cached upstream images.

**Book:** extend B1's section — synchronous vs asynchronous replication, why "succeed with one copy plus a heal loop" beats "fail the push", and cloning `Arc`s across the axum state boundary.

---

### B3 — GC loop with the M2 TOCTOU fix

**Tests first:**

- Unit (`src/pickle/types.rs`): `apply_gc_report_never_removes_last_holder` — a `GcReport` that would empty a holder set is skipped for that layer. Then the deterministic race: apply two `GcReport`s (node 1 then node 2, each removing itself from a 2-holder layer) — exactly one is applied; the set never empties.
- Unit (`src/pickle/gc.rs`): blobs with an empty holder set but mtime younger than the grace period are protected (mid-push protection).
- Integration: the loop deletes an unreferenced blob only *after* its removal is committed; a blob the state machine refused (sole holder) is **not** deleted locally.

**Implementation:**

- `ManifestCatalog::apply_gc_report` (state machine side): skip removals that would leave a layer with zero holders. Raft serialises the two reports, so the second node's removal is refused deterministically — this is the whole M2 fix.
- Split `gc_sweep` into a pure `plan_gc(...) -> Vec<Digest>` (candidates) and the act phase. New flow per tick: plan → propose `RaftRequest::GcReport { node_id, removed_layers: candidates }` → await commit → re-read the catalog → **delete locally only the layers where self is no longer a holder**.
- Add `grace_period: Duration` (default 1h) to `GcConfig`; skip empty-holder blobs younger than it (file mtime from `BlobStore`).
- Wire `gc_retain_tags`/`gc_retain_days` from `[images]` into `GcConfig`; active image set from the council desired state (scheduled app specs' image refs).
- Spawn `run_gc_loop` in `src/bin/bun.rs` next to the Pickle server: `tokio::time::interval` (default 300s, config `gc_interval_secs`), `CancellationToken`, skipped entirely in standalone mode without a council (nothing to serialise against — log once and exit the task).

**Book:** new section "Deleting data without losing it" — the TOCTOU story as the centrepiece: why check-then-act breaks across machines and how routing the *decision* through Raft (not the deletion itself) fixes it. This is the chapter's best distributed-systems lesson; give it room.

---

### B4 — replication audit loop (rarest-first heal)

**Tests first:**

- Unit: `audit_orders_rarest_first` — given layers with 1, 2, 3 holders and redundancy 3, the work list is ordered by holder count ascending and capped at the per-tick limit.
- Integration (roadmap): "under-replicated image auto-heals when a new node joins" — registry A holds an image with `redundancy = 2` and no peers; registry B appears in the peer list; after one audit tick B holds the layers and `UpdateLayerLocations` recorded it.

**Implementation:**

- Pure planner in `src/pickle/replication.rs`: `plan_audit(catalog, self_node, redundancy, max_per_tick) -> Vec<Digest>` — layers where `holders.len() < redundancy` **and** `self ∈ holders` (only holders push; avoids two nodes healing the same layer redundantly — mention the remaining benign race in the book), rarest first, capped (default 10/tick).
- Loop (same task as B3 or a sibling; default 60s + ±10% jitter via the existing `rand` dep): for each planned layer, `select_peers` non-holders → `replicate_layer_to_peer` → propose `UpdateLayerLocations { added }`.

**Book:** extend B3's section — self-healing as a loop invariant ("every tick, the world gets a bit closer to `redundancy`"), and why you cap per-tick work (a new empty node must not trigger a cluster-wide replication storm).

---

### C1 — P2P download planner (pure) + property tests

**Tests first** (new `src/pickle/p2p.rs`):

- `plan_orders_rarest_first` — layers sorted by ascending holder count.
- `plan_balances_across_sources` — 6 layers all held by peers X and Y → neither peer is assigned more than 3 ("parallel source balancing": greedy pick of the holder with the fewest assignments so far).
- `plan_dedups_digests` — the same digest appearing twice in the manifest (config blob = layer blob edge case) yields one fetch.
- `plan_skips_local_layers` — already-local digests excluded (caller passes the `has_blob` set).
- Property tests (proptest, in the same module): arbitrary topologies — up to 40 layers, up to 8 peers, arbitrary holder sets — (a) every layer with ≥1 live holder gets exactly one assignment; (b) no layer is assigned to a non-holder; (c) max assignments per peer ≤ ceil(assigned_layers / distinct_holding_peers) + 1.

**Implementation:**

```rust
pub struct LayerFetch { pub digest: Digest, pub peer: Peer }

pub struct DownloadPlan {
    pub fetches: Vec<LayerFetch>,          // rarest-first order
    pub unavailable: Vec<Digest>,          // no live holder — caller falls back
}

pub fn plan_downloads(
    needed: &[Digest],
    local: &HashSet<Digest>,
    catalog: &ManifestCatalog,
    peers: &[Peer],
    self_node: u64,
) -> DownloadPlan;
```

Pure function, no I/O, no clock. "Rarest-first" earns its keep at fan-out time: when ten nodes pull simultaneously, starting with the scarcest layers spreads copies of exactly the blobs whose loss hurts most.

**Book:** new section "Rarest first, like BitTorrent" — the planner as a pure function (test without a network!), proptest strategies explained for a Go/Python reader (compare to Hypothesis/quickcheck), and why we chose layer granularity over byte ranges (rejected alternative §2.5).

---

### C2 — parallel executor and agent pull integration

**Tests first:**

- Unit: a failed fetch retries once against an alternate holder; exhausted holders land the digest in the error.
- Integration (`tests/pickle_cluster.rs`): registry A holds a 5-layer image (5 × 20 MB of incompressible `rand` bytes = 100 MB); `ensure_image_local` on B fetches everything, `image_available_locally` is true, and wall-clock < 5s (roadmap target; localhost makes this comfortable — assert it anyway to catch accidental serialisation). Second test with 3 holders: the executed plan spread fetches across ≥2 peers.

**Implementation:**

- `pull_layers_parallel(plan, store, client, concurrency, timeout)` in `p2p.rs`: `tokio::task::JoinSet`, at most `concurrency` (default 4, config `p2p_concurrency`) in flight, each task calling the existing `pull_layer_from_peer` (`pull.rs:19`). One retry pass: failed digests re-planned against remaining holders. Everything wrapped in explicit `tokio::time::timeout`.
- `ensure_image_local(image_ref, catalog, peers, store, ...) -> Result<bool>`: resolve the manifest from the catalog; `false` if absent (caller falls through to the external path / D2); otherwise plan + fetch missing layers and return `true`.
- Agent wiring: in the image-prepare path (`src/grill/image.rs:74-180` callers in `src/bun/agent.rs`), call `ensure_image_local` before any external pull. Signature verification stays exactly where it is (schedule time, manifest level) — add a comment at the call site saying why P2P doesn't need its own check.

**Book:** extend C1's section — `JoinSet` introduced properly (structured concurrency vs Python's `gather`/Go's `errgroup`), bounded concurrency as backpressure, and the retry-with-alternate-holder pattern.

---

### D1 — upstream client and cache decision logic

**Tests first** (new `src/pickle/upstream.rs`):

- `cache_decision_hit` / `_miss` / `_stale_same_digest` / `_stale_new_digest` — pure decision function against a fabricated catalog: absent → `Miss`; present and fresh → `Hit`; present, older than `recheck`, upstream HEAD digest unchanged → `Hit` (and refresh the timestamp); digest changed → `Refetch`.
- `cached_repository_naming` — `docker.io/library/redis:7` caches under repository `cache/docker.io/library/redis`.
- `credentials_resolved_from_secret` — an `ExternalRegistry` with `password_secret` resolves through the injected secret lookup; absent → anonymous.

**Implementation:**

```rust
pub trait UpstreamRegistry: Send + Sync {
    async fn head_manifest_digest(&self, image: &Reference) -> Result<Digest, PickleError>;
    async fn fetch_manifest(&self, image: &Reference) -> Result<(Vec<u8>, Digest), PickleError>;
    async fn fetch_blob(&self, image: &Reference, digest: &Digest) -> Result<Vec<u8>, PickleError>;
}

pub struct OciUpstream { /* oci_distribution::Client + RegistryAuth */ }
pub struct MockUpstream { /* scripted responses + AtomicUsize hit counters */ }

pub enum CacheDecision { Hit, Refetch }
pub fn decide(catalog: &ManifestCatalog, cached_repo: &str, tag: &str,
              now: SystemTime, recheck: Duration,
              upstream_digest: impl FnOnce() -> ...) -> CacheDecision;
```

(Trait justified: two implementations from day one.) `OciUpstream` wraps the existing `oci-distribution` dependency; auth from `[images] external_registries` (`ExternalRegistry { host, username, password_secret }` — the struct exists at `types.rs:384-394`, never instantiated), password resolved through the Sesame secret store at startup. Freshness rides on `ImageManifest.pushed_at`; `cache_recheck_secs` config, default 3600. Digest-pinned references (`@sha256:...`) are immutable — always `Hit` once present.

**Book:** new section "Caching other people's registries" — mutable tags as the whole problem (`redis:7` moves), HEAD-before-refetch, and testing an internet-facing feature with zero internet (the mock + counter pattern).

---

### D2 — pull-through wiring and integration

**Tests first:**

- Integration (`tests/pickle_cluster.rs`): an in-process Pickle registry acts as the *upstream* (it speaks OCI). Wrap it in a counting `MockUpstream`-style shim (or count via a middleware on its router). Node A `ensure_image_local` for `upstream-host/app:v1`: fetches from upstream (counter = manifest + blobs), commits to the catalog under `cache/...`, replicates per redundancy. Node B pulls the same ref: **zero upstream hits** — served by catalog + P2P ("first pull cached, second pull served locally", roadmap line 572).
- Scheduler exemption test (`src/meat/scheduler.rs`): with `require_signatures = true`, an unsigned manifest under `cache/...` passes; an unsigned manifest elsewhere is still refused (decision §2.8).

**Implementation:**

- Extend `ensure_image_local`'s miss path: if the reference has a registry host and `[images] pull_through = true` (new bool, default true): `decide()` → on `Refetch`, `fetch_manifest` + `fetch_blob`s → `store.write_blob` → `commit_and_replicate()` (from B2) under the `cache/<host>/<repo>` repository.
- Concurrent-fetch guard: a per-image `tokio::sync::Mutex` keyed map (or a single mutex around the fetch — simplest correct thing) so two instances landing at once don't double-download.
- Scheduler exemption per §2.8, with a `// TODO(Phase N)` pointing at upstream trust policy.
- Manual acceptance (not CI): on the Lima cluster, deploy `docker.io/library/alpine`-based app on two nodes; confirm node 2's pull never touches Docker Hub (registry logs).

**Book:** extend D1's section — the read-through cache shape (miss → fill → serve), the double-download guard, and the signature-exemption judgement call spelled out honestly.

---

### E0 — wire `VolumeManager` (fixes review M21; prerequisite for E1/E2)

**Tests first:**

- Unit: the prepare path calls `create_managed_volume` for each managed `VolumeSpec` (no `source`) and skips host-path volumes.
- Integration (Lima, `RELIABURGER_RUNC_TESTS=1`): deploy `examples/phase-1/container-volumes.toml` — the container starts (today it fails ENOENT), writes to `/data`, survives a restart with data intact.
- macOS-runnable: `proc-volumes.toml` equivalent through `ProcessGrill` — the managed directory exists after prepare.

**Implementation:**

- Construct `VolumeManager` from `config.storage.volumes` (killing that dead-config entry) and invoke it in the container prepare flow immediately before mount-entry generation (`src/grill/oci.rs:285-306` call site — create first, then generate the entries).
- Delete path: remove the managed volume tree when the app is deleted (not on restarts). Record which size backend was used (needed by E1's teardown: loop mounts must be unmounted before removal).
- Update `docs/progress.md`'s "dead config" line for `[storage] volumes`.

**Book:** new section "Volumes that actually mount" — a short honest one: the library-not-wired trap (write the code, forget the caller), and how the integration test that *boots the example config* is the only test that would ever have caught it.

---

### E1 — Btrfs subvolume quotas

**Tests first:**

- Unit (new `src/grill/btrfs.rs`): command generation for `subvolume create` / `quota enable` / `qgroup limit {bytes} {path}`; backend-selection decision function with an injected `is_btrfs` answer (btrfs+root+size → `BtrfsQgroup`; ext4+root+size → `LoopMount`; no size → `Plain`).
- Integration (Lima, **new gate `RELIABURGER_BTRFS_TESTS=1`**, root): the test provisions its own filesystem — `truncate -s 1G`, `mkfs.btrfs`, `mount` to a tempdir — then creates a managed volume with `size = "10Mi"` and asserts writing 11 MiB fails with ENOSPC (roadmap line 575), then cleans up (unmount, delete image).

**Implementation:**

- `src/grill/btrfs.rs`: `is_btrfs(path)` via `nix::sys::statfs` comparing `f_type` to `BTRFS_SUPER_MAGIC` (0x9123_683E) — Linux-only with a `#[cfg(not(target_os = "linux"))] false` stub; `create_subvolume_with_quota(path, bytes)` shelling `btrfs` (same `tokio::process` style as netns.rs).
- `VolumeManager::create_managed_volume`: choose the backend via the decision function; record it:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeBackend { Plain, LoopMount, BtrfsQgroup }
```

  persisted in a sidecar `volume.json` next to the volume (delete/snapshot paths read it).
- `relish dev` changes: add `btrfs-progs` to the VM provisioning package list and `RELIABURGER_BTRFS_TESTS=1` to the `dev test` env block (`src/relish/dev.rs:782-791`).

**Book:** new section "Quotas without loop devices" — what a Btrfs subvolume and qgroup are, the loop-mount trick they replace (sparse file + mkfs + mount) and its costs, `statfs` as the first taste of asking the kernel a question through nix, and the enum-not-trait backend choice (three variants, one `match` — no premature abstraction).

---

### E2 — volume snapshots (Btrfs-only) with API and CLI

**Tests first:**

- Unit (new `src/grill/snapshot.rs`): snapshot naming (`{RFC3339-compact}-{suffix}` — no `Date::now` in tests; inject the clock), `SnapshotMeta` serde round-trip, restore-guard (restore while instances run → `SnapshotError::AppRunning`), `UnsupportedFilesystem` on non-Btrfs.
- Integration (Lima, btrfs gate, root — roadmap line 573 verbatim): create volume on the loopback btrfs, write `v1`, snapshot, overwrite with garbage, restore, read `v1` intact.
- API tests (tower, macOS): endpoints return 501-style `UnsupportedFilesystem` cleanly on a non-btrfs volumes dir; auth required.

**Implementation:**

- Layout: `{volumes_dir}/.snapshots/{ns}/{app}/{mount-path-slug}/{name}` — read-only snapshots via `btrfs subvolume snapshot -r <live> <dest>`, each with a `meta.json` (`SnapshotMeta { namespace, app, volume_path, name, created_at, size_bytes, uploaded: bool }`).
- Restore (app must be stopped; agent checks the supervisor): `btrfs subvolume delete <live>` then `btrfs subvolume snapshot <snap> <live>` (writable).
- API (behind the auth layer, `src/bun/api.rs`): `POST /v1/snapshots/{ns}/{app}` (create, optional `{"volume": "/data"}` body — default: all managed volumes), `GET /v1/snapshots/{ns}/{app}` (list), `POST /v1/snapshots/{ns}/{app}/restore` (`{"name": ...}`), `DELETE /v1/snapshots/{ns}/{app}/{name}`.
- CLI: `relish snapshot create|list|restore|delete <app> [-n <ns>] [--name <snap>]` via `BunClient`.
- Requires E1's `SizeBackend::BtrfsQgroup` (only subvolumes snapshot; a plain dir on btrfs won't) — create managed volumes as subvolumes whenever the filesystem is btrfs, even without a size limit.

**Book:** new section "Point-in-time for free" — CoW snapshots as O(1) metadata operations, why restore requires a stopped app, and the deliberate "Btrfs only, loud error elsewhere" scope decision (rejected: rsync-style copy fallback — silently *slow* "snapshots" are worse than an honest error).

---

### E3 — scheduled snapshots and object-store upload

**Tests first:**

- Unit: retention pruning (`retain = 3` keeps the newest 3 per volume — pure function over `SnapshotMeta` lists); scheduler due-logic driven **manually** (no `tokio::spawn` under `start_paused` — construct the loop's tick body as a function and call it; memory feedback item).
- Integration (macOS-runnable for the upload half): with a fake snapshot directory tree, the upload pass tars it (tar + flate2 — both already deps) and writes `{prefix}/{ns}/{app}/{name}.tar.gz` through `object_store` to a `file://` destination; `uploaded` flips true in `meta.json`; a second pass uploads nothing (checkpoint semantics, borrowed from `ExportCheckpoint`'s pattern).
- Integration (Lima, btrfs gate): 1s interval loop produces a snapshot of a live app's volume and prunes past `retain`.

**Implementation:**

- Config, new section (parsed in `src/config/node.rs`):

```toml
[storage.snapshots]
interval_secs = 86400   # 0 = disabled (default)
retain = 7
upload_url = "s3://bucket/prefix"   # optional; file:// and gs:// also accepted
```

- `Cargo.toml`: `object_store = { version = "0.12", features = ["fs", "aws", "gcp"] }`. Build the store via `object_store::parse_url`; credentials come from the standard env vars (document in `docs/README.md`).
- Loop spawned from the agent startup (interval + `CancellationToken`): snapshot every managed volume of currently registered apps → prune → upload un-uploaded snapshots (`tokio::task::spawn_blocking` for the tar; stream the file to the store).
- Failure policy: upload errors log and retry next tick (the `uploaded` flag is the checkpoint); snapshot errors for one app don't abort the sweep.

**Book:** extend E2's section — the interval-vs-cron decision (§2.7) argued openly, `spawn_blocking` for CPU-bound tar work (never block the runtime), and `object_store` as the "one trait, many clouds" abstraction with the `file://` test trick.

---

### F1 — wire `SubmitBatch`

**Tests first:**

- Unit: `BatchSubmitRequest`/`BatchSubmitResponse` serde; capacity maths (total minus supervisor-known allocations, not live usage — deterministic).
- Integration (TestHarness, macOS, ProcessGrill): submit 3 proc jobs to a single node → `202` with a batch id; poll `GET /v1/batch/{id}` until `done`, `completed = 3`. Failure path: a job whose command exits non-zero ends `failed = 1`. Dispatch path: a second harness receives `POST /v1/batch/run` directly and reports back to the first via the callback URL (full two-node flow without cluster plumbing).

**Implementation** (new `src/bun/batch.rs` for types + handlers; routes replace the 501s at `api.rs:1761-1775`):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchSubmitRequest { pub jobs: Vec<BatchJobSubmission> }   // full specs, not names
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchJobSubmission { pub name: String, pub spec: AppSpec } // the [job.*] section, is_job semantics

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchSubmitResponse { pub batch_id: u64, pub assigned: usize, pub unschedulable: Vec<String> }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCapacityReport {
    pub node_id: String,
    pub cpu_millicores_total: u64, pub cpu_millicores_allocated: u64,
    pub memory_bytes_total: u64,   pub memory_bytes_allocated: u64,
    pub gpus: u32,
    pub has_buildah: bool,   // F2 uses this
}
```

Flow on `POST /v1/batch`:

1. Gather capacity: `GET /v1/capacity` (new endpoint, sysinfo totals minus the supervisor's registered allocations) from every membership node, using the exact fan-out pattern of `api.rs:516-584` (membership → URLs → `state.http_client` + service token, 10s timeout). Unreachable nodes are simply excluded.
2. `schedule_batch` (`meat/batch.rs:51`) over the reports → `BatchAllocation`.
3. `BatchTracker::register` (tracker lives in the agent as `Arc<Mutex<BatchTracker>>`; GC'd daily with the existing `gc(max_age)`).
4. Group assignments by node; self-assignments start locally via the existing job path (supervisor, `is_job = true`); remote groups get `POST /v1/batch/run { batch_id, callback_base_url, jobs }` (service-token-authenticated, same middleware).
5. The running node watches each job to its terminal state and posts `POST /v1/batch/{id}/report { job_name, status: "completed" | "failed" }` to the callback; the tracker marks accordingly. Local jobs short-circuit without HTTP.
6. `GET /v1/batch/{id}` → `BatchSummary` (exists) as JSON. Unknown id → 404.

CLI: `relish batch <file>` now serialises the full `[job.*]` specs into the request (`src/relish/commands.rs:924-938`, `client.submit_batch` signature changes) and prints the batch id + unschedulable list. New `relish batch-status <id>`.

**Gotcha:** don't touch the lib-only reporting tree or `meat/scheduler.rs` dispatch (L1) — this is a parallel, self-contained path; leave a `// TODO` noting L1 may consolidate onto it.

**Book:** new section "A thousand jobs, fifty envelopes" — the dispatch design decision in full (HTTP vs Raft-log allocations vs reporting tree, §2.6), greedy bin-packing recap, and the callback-URL completion pattern with its failure mode (submitter restarts → orphaned reports 404 harmlessly; tracker GC cleans up).

---

### F2 — wire `SubmitBuild`

**Tests first:**

- Unit: builder selection (self if `has_buildah`, else first capable peer, else `NoBuilderAvailable`); `BuildState` machine (`Pending → Running → Completed(BuildResult) | Failed(String)`); request serde.
- Integration (Lima, **new gate `RELIABURGER_BUILDAH_TESTS=1`**): `relish build` a trivial context (`FROM scratch` + `COPY hello.txt /`) against the in-VM agent — context lands in Pickle via the registry port (X1 fixed), the build runs, the manifest appears in the catalog **signed**, and `GET /v1/build/{id}` reports `completed` with layer count and size.
- macOS-runnable: builder-selection and 503-when-no-builder tests against TestHarness (no buildah on the mac).

**Implementation:**

1. **Fix X1:** `src/relish/commands.rs:880` uploads the context to the Pickle registry port (from the config file / a `--registry-port` flag defaulting to the configured `registry_port`), not 9117. The URL helper `context_upload_url` (`pickle/build.rs:226`) already expects the registry.
2. `POST /v1/build { name, context_digest, destination }` (replacing the 501 at `api.rs:1777-1791`): pick a builder via `NodeCapacityReport.has_buildah` (probed once at startup: `buildah --version` succeeds); dispatch to it (`POST /v1/build/run`, same auth pattern as F1) or run locally; return `202 { build_id }`.
3. Builder execution (`src/bun/build_runner.rs`): fetch the context blob (local `BlobStore`, else P2P via C2's machinery, else `GET` from the submitter's registry) → extract the tar to a tempdir → run `BuildahJob.build_cmd` then `push_cmd` (both already generated by `execute_build`) via `tokio::process` with a timeout (config `build_timeout_secs`, default 900) → the push lands in the *local* registry, which replicates and Raft-commits via B2 → sign with the existing `sign_image_manifest_with_external_key` (`agent.rs:2365-2398`) → report `BuildResult` back (callback, as F1).
4. Track in a small `BuildTracker` (plain `HashMap<u64, BuildState>` behind the agent — `BatchTracker`'s shape doesn't fit a single-job-with-payload lifecycle; don't force it). `GET /v1/build/{id}`.
5. CLI: `relish build` polls `/v1/build/{id}` to a terminal state and prints the `BuildResult`.
6. `relish dev`: add `buildah` to VM provisioning and `RELIABURGER_BUILDAH_TESTS=1` to the `dev test` env block.

**Book:** new section "Building where the tools are" — capability-based placement, `tokio::process` with timeouts, and the tidy loop this closes: build → push → replicate → sign → verify at schedule → deploy, every arrow now real.

---

### G — chapter assembly and docs sweep (final step)

1. `docs/book/12-squeezing-every-drop.md`: write the chapter intro ("why optimise now — everything works, now make it not embarrass us at scale") and the closing "Lessons learned" (the M2 TOCTOU, the library-not-wired trap from M21/X1, argv-vs-shell quoting, testing internet features offline). Verify every section from steps A-F landed; fix cross-references; one pass for the style guide (no em-dash spray, no "Notably,", vary sentence length).
2. `docs/progress.md`: tick every Phase 12 box (lines 321-333), including flipping `[~]` book to `[x]`; clear the `[lib-only]`-style caveats this phase resolved (M21 volumes, L10 pickle, M2, X1); note the batch/build 501s are gone (line 190).
3. `docs/README.md` + top-level `README.md`: new CLI commands (`relish snapshot ...`, `relish batch-status`), new endpoints table rows (`/v1/capacity`, `/v1/batch*`, `/v1/build*`, `/v1/snapshots*`), the uniform-registry-port constraint, snapshot config example, updated test counts, Phase 12 marked complete.
4. `docs/review-2026-07.md` stays untouched (it's a point-in-time review) — progress.md is the living tracker.

## 6. Config additions summary

| Key | Section | Default | Step |
|-----|---------|---------|------|
| `redundancy` | `[images]` | 2 | B2 (comes alive) |
| `push_sync` | `[images]` | true | B2 (comes alive) |
| `gc_retain_tags` / `gc_retain_days` | `[images]` | 10 / 30 | B3 (come alive) |
| `gc_interval_secs` | `[images]` | 300 | B3 (new) |
| `audit_interval_secs` | `[images]` | 60 | B4 (new) |
| `p2p_concurrency` | `[images]` | 4 | C2 (new) |
| `pull_through` | `[images]` | true | D2 (new) |
| `cache_recheck_secs` | `[images]` | 3600 | D1 (new) |
| `external_registries` | `[images]` | [] | D1 (comes alive) |
| `volumes` | `[storage]` | /var/lib/reliaburger/volumes | E0 (comes alive) |
| `interval_secs` / `retain` / `upload_url` | `[storage.snapshots]` | 0 / 7 / none | E3 (new) |
| `build_timeout_secs` | `[images]` | 900 | F2 (new) |

Every new key gets a doc comment, a default, and an example in `docs/README.md`. No key is added without a reader.

## 7. Endpoint changes summary

| Endpoint | Change | Step |
|----------|--------|------|
| `GET /v1/capacity` | new (protected) | F1 |
| `POST /v1/batch` | 501 → real | F1 |
| `POST /v1/batch/run` | new (service-token; node-to-node) | F1 |
| `POST /v1/batch/{id}/report` | new (service-token; callback) | F1 |
| `GET /v1/batch/{id}` | new | F1 |
| `POST /v1/build` | 501 → real | F2 |
| `POST /v1/build/run` | new (service-token) | F2 |
| `GET /v1/build/{id}` | new | F2 |
| `POST/GET/DELETE /v1/snapshots/...` | new (4 routes) | E2 |

All behind the existing `auth_middleware` route layer.

## 8. Test inventory (roadmap "Tests (write first)" → where they land)

| Roadmap test (roadmap.md:556-582) | Step | Where it runs |
|---|---|---|
| nftables map generation: syntax, incremental, rollback | A1 | everywhere (unit) |
| nftables maps behaviour parity + 1000-port stress | A2 | Lima (netns gate) |
| P2P chunk selection: rarest-first, balancing, dedup | C1 | everywhere (unit) |
| P2P property: arbitrary topologies complete | C1 | everywhere (proptest) |
| P2P: multi-layer pull from another node < 5s / 100 MB | C2 | everywhere (integration, localhost) |
| P2P: under-replicated auto-heal on node join | B4 | everywhere (integration) |
| Pull-through: manifest resolution, hit/miss/stale | D1 | everywhere (unit) |
| Pull-through: first pull cached, second served locally | D2 | everywhere (integration, in-process upstream) |
| Btrfs quota: creation, enforcement (write-beyond fails) | E1 | Lima (btrfs gate) |
| Volume snapshots: create/corrupt/restore intact | E2 | Lima (btrfs gate) |
| Volume snapshots: scheduled job + object-store upload | E3 | upload: everywhere; live loop: Lima |
| Parquet bloom filter construction/lookup/FPP | done | `src/ketchup/log_store.rs` |
| Zstd round-trip + >5x + random access | done | `src/ketchup/log_store.rs` |
| Batch submit/dispatch/track/status | F1 | everywhere (TestHarness) |
| Build end-to-end (context → buildah → signed manifest) | F2 | Lima (buildah gate) |

Note the deliberate deviations already recorded in progress.md:330-331: bloom filters are on `app`/`namespace` equality (blooms can't answer substring `LIKE` on `line`), and zstd is Parquet's native codec rather than a separate seekable-frame container. Both are settled — don't reopen them.

## 9. Gotchas checklist (read before each step)

1. **C4:** nothing in slice A touches `reliaburger_fw`; keep `ruleset_uses_isolated_table_name` green.
2. **nft argv:** pass `{ 30001 : 10.0.2.2 . 8080 }` as one argv element via `Command::args`; never build a shell string.
3. **Standalone mode:** every Pickle change must keep working with `council: None` (single-node, no cluster). The local catalog is the fallback, not legacy.
4. **M2 fix lives in the state machine** (`apply_gc_report` refuses to empty a holder set), not in the GC loop's read — the loop's re-read-after-commit is the *consequence*, not the guard.
5. **Signatures:** P2P and pull-through move blobs; verification stays manifest-level at schedule time. The only policy change is the `cache/` exemption (§2.8) — nothing else.
6. **Capacity = allocations, not usage:** subtract the supervisor's registered reservations; sysinfo's live numbers jitter and break test determinism.
7. **Loop-mount teardown:** volumes created with `SizeBackend::LoopMount` must be unmounted before directory removal; btrfs subvolumes need `subvolume delete`, not `rm -rf`.
8. **No `Date.now` patterns in schedulers' tests:** inject clocks; drive interval loops manually (start_paused pitfall).
9. **Lima VM provisioning** gains `btrfs-progs` and `buildah`; `dev test` gains `RELIABURGER_BTRFS_TESTS=1 RELIABURGER_BUILDAH_TESTS=1`. If the VM image predates the change, `relish dev clean` + recreate.
10. **`cargo tree -d` after the `object_store` feature change** — `aws`/`gcp` pull new transitive deps; make sure we don't grow duplicate versions of `reqwest`/`quick-xml` majors we already ship.
11. Uniform `registry_port` across the cluster (§2.9) — assert it in docs, not code.
12. Book section per step; `make ci` per commit; ask before committing; never amend.

## 10. Acceptance runbook

Run after slice D and again after G:

1. **macOS:** `make ci` — everything green, including the new unit/integration tests (P2P, pull-through, batch all run locally).
2. **Lima gated tests:** `relish dev test` — netns (incl. 1000-port stress), btrfs quota + snapshot, buildah build, existing runc/eBPF suites.
3. **Live 3-node cluster:** `relish dev create --nodes 3`, then inside node 1:
   - push an image to the local registry → `relish` on nodes 2/3 shows it in the catalog (B1) and holders ≥ 2 (B2);
   - deploy it with placement on another node → layers arrive P2P, container starts (C2);
   - deploy a `docker.io` image on two nodes → second node makes no upstream connection (D2);
   - create/restore a snapshot of a volume-backed app (E2);
   - `relish batch` 10 short jobs → `relish batch-status` shows them spread and completed (F1);
   - `relish build` the demo context → signed image deployable (F2);
   - kill and restart a node → catalog intact, GC/audit loops resume, no double-delete (B3).
4. Update `docs/progress.md`, both READMEs, and confirm chapter 12 reads end-to-end.

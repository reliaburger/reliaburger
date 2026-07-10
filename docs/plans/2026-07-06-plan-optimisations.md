# Phase 12 Implementation Plan — Optimisations ("Squeezing Every Drop")

**Date:** July 2026, revised 2026-07-09. **Executor note:** this plan is prescriptive; follow it step by step and do not skip the tests-first ordering. All paths are relative to the repo root. Originally verified against `stage-3b-transport-hmac-signatures` (2026-07-06); **revised against `phase-12-optimisations`** after the Stage 4 wiring merge (#71, "Wire all the things!") and Phase 14 (self-upgrade) landed — both changed this plan's ground truth substantially. Slice B as originally written is **already done** (differently than planned — see §3.2 and step B5); slice F is half-done.

Items **already done — do not redo them**:

- Parquet bloom filters on `app`/`namespace` (1% FPP) — `src/ketchup/log_store.rs:47-64`, tests at `log_store.rs:663` onwards.
- Zstd compression for archived logs via Parquet's per-row-group ZSTD codec — same file, `zstd_parquet_is_over_5x_smaller_than_raw_text` and `zstd_archive_round_trips_through_remote_query`.
- **Slice B (Pickle catalog via Raft, replication, GC)** — landed in the Stage 4 wiring merge. What remains is the B5 hardening step below.
- **X1** (`relish build` context upload to the wrong port) — fixed; the context now goes to the registry port (`src/relish/commands.rs:961-981`).

The book chapter (`docs/book/12-squeezing-every-drop.md`) already contains the logs section. Every step below names the book section it must add — write it in the same commit, not at the end.

## 0. Ground rules (non-negotiable)

1. **Tests first.** Every step lists its tests; write them, watch them fail, then implement.
2. **`make ci` before every commit** (fmt --check, clippy -D warnings, test). No exceptions.
3. **One commit per step, no amends.** Within this approved staged plan, show the commit message and summary before each commit and proceed without re-asking; never amend.
4. **Book alongside code.** Each step updates `docs/book/12-squeezing-every-drop.md`. The audience knows C/Python/Go, not Rust: explain each new Rust concept on first appearance (this phase introduces `JoinSet`, `statfs` via nix, proptest strategies, feature-gated dependencies).
5. **Update `docs/progress.md`** (Phase 12 section) as items complete; update `docs/README.md` and the top-level `README.md` (test counts, new CLI commands, new endpoints) at the end of the phase.
6. **British English** in prose and doc comments; serde derives stay American.
7. **No `unwrap()`/`expect()`/`panic!()` in production code**; thiserror enums per subsystem; anyhow only in binaries.
8. **tokio-only sync primitives; `CancellationToken` shutdown** for every new long-lived task.
9. **tokio `start_paused` pitfall:** never combine `tokio::spawn` with a paused clock in tests — drive loops manually with `try_recv`/explicit ticks (this bit us before; see the scheduler tests in step E3).

## 1. What Phase 12 delivers

Six work slices, ordered:

| Slice | Item | Status |
|-------|------|--------|
| A | nftables named maps for O(1) port mapping | not started |
| B | Pickle catalog via Raft + replication + GC | **done as-built (#71)**; B5 hardens the heal loop |
| C | P2P multi-source image downloads (rarest-first, parallel, dedup) | not started |
| D | Pull-through cache for external registries | not started |
| E | Managed-volume wiring (M21), Btrfs subvolume quotas, volume snapshots + scheduled upload | not started (M21 explicitly deferred here by the wiring plan) |
| F | Wire `SubmitBatch` and `SubmitBuild` into the agent | `/v1/build` half-done (sync, local-only); `/v1/batch` still 501 |

**Milestone (roadmap.md:597):** port mapping uses O(1) nftables maps, images download from multiple peers in parallel, logs compress 5x with random-access reads (already done). All Phase 12 tests pass.

## 2. Scope decisions (settled — do not relitigate)

1. **Batch and build wiring are in scope** for this plan (steps F1/F2), even though they're "wiring" — the user confirmed both.
2. **Linux-only features (nftables maps, Btrfs) are tested via the Lima setup.** Unit tests for command/rule generation run everywhere; live tests are env-gated and run inside the test VM via `relish dev test` (`src/relish/dev.rs:760-819`), which already exports `RELIABURGER_RUNC_TESTS`, `RELIABURGER_NETNS_TESTS`, `RELIABURGER_EBPF_TESTS`. This phase adds `RELIABURGER_BTRFS_TESTS` and `RELIABURGER_BUILDAH_TESTS` to that list, plus `btrfs-progs` and `buildah` to the VM provisioning.
3. **Volume snapshots are Btrfs-only.** On any other filesystem, snapshot operations return a clear `UnsupportedFilesystem` error. No copy fallback. Live testing on Lima against a loopback Btrfs mount the tests create themselves.
4. **Snapshot upload targets the `object_store` abstraction** (already a dependency). Tests run against the local filesystem backend; real S3/GCS is a manual acceptance step. Enable the `aws` and `gcp` features so `s3://`/`gs://` URLs genuinely work.
5. **P2P chunk = layer.** No sub-layer HTTP range splitting this phase. The roadmap's "chunk selection" tests operate at layer granularity. Rejected: range-splitting layers > 100 MB across peers — real complexity, marginal win at our layer sizes; note it as future work in the book.
6. **Batch/build dispatch is direct HTTP** from the leader to target nodes' Bun APIs with the service token. The original rejection of "commit per-node allocations to Raft" has been overtaken by events: Stage 4 landed exactly that design **for deploys** (`DesiredState.placements` + polling reconcilers, `src/cluster/orchestrate.rs:65-483`). Batch must still NOT ride the placements path: the reconciler *converges desired state* — it fingerprint-diffs and issues `Stop` on unassignment (`orchestrate.rs:465-480`), which is wrong for run-to-completion jobs (a finished job looks like drift; a moved assignment kills a running job). Direct dispatch + completion callbacks fit run-to-completion semantics; say so in the book.
7. **Snapshot scheduling is interval-based** (`interval_secs`), not cron expressions. Rejected: the `cron` crate — it drags in chrono for a feature nobody asked for by name; the roadmap's "scheduled snapshot job runs on cron" is satisfied by a scheduled loop. Note the trade-off in the book.
8. **Cached external images are exempt from `require_signatures`.** Today the scheduler passes external (non-Pickle) images through unverified (`src/meat/scheduler.rs:199-260` returns `Ok` when the manifest isn't in the catalog; production caller `src/bun/agent.rs:2546-2560`). Once the pull-through cache stores them in Pickle they would suddenly fail a `require_signatures = true` policy. Exempt manifests whose repository starts with `cache/` — they are upstream content and were never signable by us. Upstream trust policy (digest pinning, cosign verification of upstream sigs) is future work; say so in the book.
9. **Uniform `registry_port` across the cluster.** Gossip carries only `address`; peers' Pickle URLs are derived as `http://<gossip-ip>:<registry_port>` using the local node's configured port. Document this constraint in `docs/README.md`. Related: `registry_bind` defaults to `127.0.0.1` (`src/config/node.rs:463`), which silently disables all peer replication/P2P in cluster mode — B5 adds a startup warning.
10. **F1 gets capacity from the wired reporting pipeline, not a new endpoint.** The original plan invented `GET /v1/capacity` because L6 was unwired; L6 landed (StateReports carry real capacity commitments, `src/reporting/worker.rs:214-221`). Batch handlers leader-forward (the `/v1/apply` pattern) and the leader maps `AggregatedState` + membership → `NodeCapacity`, exactly as the deploy scheduler's `build_cluster_cache` does (`orchestrate.rs:320-365`).
11. **F2 refactors the existing synchronous build handler to async.** The sync handler strands the CLI: `BunClient` has a hard 300s timeout (`src/relish/client.rs:128`) while `build_timeout_secs` defaults to 900. The existing handler body is correct and becomes the runner task.
12. **E0 never deletes managed volumes on Stop.** `AgentCommand::Stop` is issued by users *and* by the placements reconciler on routine rebalances, and Phase 14 adoption re-attaches instances across upgrades — deleting volumes on Stop would destroy data on a rebalance. Orphaned trees are left in place; explicit cleanup (`relish volume rm` or a GC) is future work.

## 3. Verified ground truth (revised 2026-07-09; re-verify only if compilation disagrees)

### 3.1 Port mapping today

- `src/grill/netns.rs:356-431` `add_port_mapping()`: root mode runs `nft add rule ip reliaburger prerouting tcp dport {hp} dnat to {ip}:{cp}` — **one rule per container port**, O(n) chain traversal. Rootless mode spawns a tokio TCP proxy (untouched by this plan).
- **Discovered during A2:** `add_port_mapping` had **no production callers** — only the gated netns test used it (the M21 pattern again). A2 therefore also wires it: the port pair rides on a new `OciSpec.port_mapping` field (`#[serde(default)]` for pre-existing instance records), `RuncGrill::create` installs the mapping next to the netns setup, teardown/adoption handle it symmetrically. Out of scope and recorded honestly: prerouting DNAT covers host-inbound traffic only; locally-originated traffic to `container_ip:host_port` and the cross-node container-IP story are part of the node-local control-plane gap in `docs/plans/2026-07-09-review-design-discrepancies.md`.
- Rule removal (`netns.rs:521-563`) lists all rules with `nft -a list`, parses handles from text. Fragile and O(n); the map design removes it entirely.
- `ensure_nft_table()` (`netns.rs:460-517`) creates table `ip reliaburger`, chains `prerouting` (NAT hook, priority -100) and `postrouting` (masquerade). Idempotent.
- `nft` is invoked via `tokio::process::Command::new("nft")` (`netns.rs:524`); pure rule-text helper `nft_dnat_rule()` at `netns.rs:659` with unit tests at 806/812.
- **C4 guard (critical):** the perimeter firewall uses a *separate* table `reliaburger_fw` (`src/firewall/rules.rs`) and its reconcile deletes/recreates only that table. The test `ruleset_uses_isolated_table_name` (`rules.rs:245`) asserts it never touches `ip reliaburger`. Nothing in slice A may weaken this.

### 3.2 Pickle today — slice B as shipped

The Stage 4 merge implemented the original B1-B4 goals with a simpler, async design. **This is the design; the book chapter describes it, and C/D build on it:**

- `PickleState` (`src/pickle/api.rs:22-35`): `store`, `catalog: Arc<RwLock<ManifestCatalog>>`, `node_raft_id: u64`, `council: Option<Arc<CouncilNode>>`, `persist_path: Option<PathBuf>`. `record_commit()` (`api.rs:46-78`) applies a `ManifestCommit` to the local catalog, persists it to `{data}/pickle/catalog.json` (restart-safe), and proposes `RaftRequest::ManifestCommit` to the council best-effort. Push commits with `holder_nodes = {node_raft_id}`.
- **Replication is not push-synchronous.** A leader-only loop (`src/bin/bun.rs:1113-1230`, 60s tick) derives peers from gossip membership + `registry_port`, finds under-replicated manifests, calls `select_peers`/`replicate_manifest` (`src/pickle/replication.rs:46,:175`), and proposes `UpdateLayerLocations`. It doubles as the heal loop. Two known gaps, closed by **B5**: no rarest-first ordering / per-tick cap, and it only heals manifests **the leader itself fully holds** (`bun.rs:1167-1176`) — images pushed to (or cache-filled on) non-leader nodes never gain redundancy. Note: the whitepaper still advertises synchronous push-time replication — recorded as discrepancy **D11** in `docs/plans/2026-07-09-review-design-discrepancies.md`; this phase keeps the async design (B5 narrows the window) and the whitepaper gets reconciled separately.
- **GC is two-phase** and M2 is fixed: `gc_candidates` nominates (`src/pickle/gc.rs:74-130`, with a 1h orphan grace period), the node proposes `RaftRequest::GcReport`, the state machine's `apply_gc_report` **refuses to empty a holder set** (`src/pickle/types.rs:296-320` — Raft serialises the two-nodes-two-copies race), and only approved deletions are executed (`bun.rs:1012-1111`). Config: `gc_retain_days`, `gc_interval_hours`.
- Peer pull (`src/pickle/pull.rs`: `pull_layer_from_peer` :19, `pull_manifest_layers` :70 — **sequential**, `find_peer_for_layer` :103, `image_available_locally`) exists with unit tests; the parallel executor is C2's job.
- Signing/verification is complete (`src/pickle/signing.rs`); verification is manifest-digest-level, enforced at schedule time (`src/meat/scheduler.rs:199-260`, called from `src/bun/agent.rs:2546-2560` against the council catalog). P2P moves blobs, not manifests — it cannot bypass verification. Keep it that way.
- Existing integration tests in `tests/pickle_cluster.rs`: `push_records_real_holder_id`, `catalog_survives_restart`, `replication_copies_layers_to_peer`, `pull_fetches_missing_layers_from_peer`. The roadmap's "under-replicated image auto-heals when a new node joins" does **not** exist yet — the heal tick is an inline closure in `bun.rs`, untestable until B5 extracts it.
- `oci-distribution 0.11` is already a dependency — use it for the upstream client; do not add a new OCI crate.

### 3.3 Volumes today

- `src/grill/volume.rs`: `VolumeManager::create_managed_volume()` (:41-79) creates `{volumes_dir}/{ns}/{app}/{mount_path}` and, on Linux as root with a `size` limit, `setup_loop_mount()` (:85-141): `fallocate` sparse file → `mkfs.ext4` → `mount -o loop`. **No production callers (review M21, explicitly deferred to Phase 12 by the wiring plan)** — `src/grill/oci.rs:318-325` computes managed-volume paths for mount entries but never creates them, so runc bind-mounts fail with ENOENT.
- `VolumeSpec { path, source: Option<PathBuf>, size: Option<String> }` (`src/config/types.rs`); `[storage] volumes` config parsed but the agent hardcodes the default (dead config until E0).
- Size parsing (`"10Gi"` → bytes) and `check_usage()` exist with unit tests. No integration tests.
- The deploy flow that E0 hooks into: reconciler (`src/cluster/orchestrate.rs:373-483`) → `AgentCommand::Deploy` → agent `drive_instance_startup` (`src/bun/agent.rs:2610-2681`) → `generate_oci_spec` with `Some(&self.volumes_dir)` → `RuncGrill::create`.
- Example configs: `examples/phase-1/container-volumes.toml`, `proc-volumes.toml`.

### 3.4 Object store and scheduling today

- `object_store = { version = "0.12", features = ["fs"] }` — fs only; E3 adds `aws` + `gcp`.
- Ketchup export (`src/ketchup/export.rs:59-111`) copies Parquet files with an `ExportCheckpoint` (JSON `HashSet`) — E3 borrows the checkpoint pattern only.
- No cron infrastructure. The house pattern is `tokio::time::interval` in a spawned loop with a `CancellationToken` (e.g. `src/mayo/rollup_worker.rs`).

### 3.5 Batch and build today

- `/v1/batch` returns 501 (`src/bun/api.rs:2614-2628`). `relish batch` sends **job names only** (`src/relish/commands.rs`, `client.submit_batch(&job_names)`) — F1 changes the request to carry full specs.
- `/v1/build` (`api.rs:2630-2770`) is **synchronous but real**: context fetch from the registry (X1 fixed), local buildah build + push, 200 on completion. Missing vs F2: async tracking, builder selection across nodes (501 if no local buildah), signing after push, status endpoint, remote dispatch. The handler body is correct — F2 lifts it into a runner task.
- `schedule_batch(jobs, nodes) -> BatchAllocation` (`src/meat/batch.rs:51-116`) and `BatchTracker` (`src/meat/batch_tracker.rs`) are library-complete with tests; unwired.
- **Remote dispatch for deploys exists (L1, wired):** leader writes `DesiredState.placements` to Raft (`src/cluster/orchestrate.rs:65-250`); workers poll `GET /v1/placements/{node_id}` every 2s and reconcile (`orchestrate.rs:373-483`, spawned `bun.rs:482`). Batch does NOT use it (decision §2.6).
- **Capacity reporting exists (L6, wired):** workers report commitments (per-instance requests summed, `src/reporting/worker.rs:214-221`; totals from `[resources]` via `agent.set_node_capacity`, `bun.rs:366-369`); the leader's `AggregatedState` feeds `build_cluster_cache` (`orchestrate.rs:320-365`).
- **Leader-forwarding pattern:** `cluster_apply` (`src/bun/api.rs:842-882`): `is_leader()` → `leader_api_url` → proxy with `service_token`, 503 when no leader known. F1's handlers copy this shape.
- `ApiState` has `council`, `membership`, `http_client`, `service_token`, `api_port`. It does **not** yet have the aggregated-state watch receiver — F1 adds `aggregated_rx` (the receiver already exists in the orchestration tuple, `bun.rs:354-358`).

### 3.6 The Lima test rig

- `relish dev test [filter]` (`src/relish/dev.rs:749-819`): ensures the build VM, runs `sudo -E cargo test -j 2 --features ebpf` inside it with `RELIABURGER_RUNC_TESTS=1 RELIABURGER_NETNS_TESTS=1 RELIABURGER_EBPF_TESTS=1`, `--test-threads=1`. Env-gated tests print a skip message when the variable is absent (pattern: `tests/ebpf.rs:29,82`).
- `relish dev create --nodes 3` builds a real multi-node cluster in Lima VMs — the acceptance environment (§10).
- The VM must be recreated once this phase (new packages: `btrfs-progs`, `buildah`).

## 4. Relationship to the wiring track — RESOLVED

The original plan assumed a parallel wiring effort; that effort (Stage 4, #71) has **landed in full**: L1 placements + reconcilers, L2 deploy orchestrator, L6/L11 real capacity + rollups, C5 auth middleware, L10/M2 Pickle catalog via Raft + two-phase GC, X1 build-context port fix, L16 egress, L13 GitOps. Consequences for this plan:

- Slice B shrank to **B5** (heal hardening).
- F1 reuses the reporting pipeline and leader-forwarding instead of inventing `/v1/capacity`.
- F2 starts from a working synchronous handler rather than a 501.
- E0's volume creation must respect the reconciler's Stop-on-rebalance semantics (decision §2.12) and Phase 14 instance adoption (`src/bun/agent.rs:706-851`).

## 5. Work breakdown

Fourteen implementation steps after the plan revision (P0, this document). Order within a slice is mandatory; A and E are independent; **B5 must precede C2 and D2** (shared peers helper + heal coverage); C1 → C2 → D1 → D2; F1 → F2 last.

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
2. **Legacy cleanup:** after ensuring the map rule, list `prerouting` once and delete any leftover per-port `dnat to` rules that aren't the map rule (upgrade path from the old scheme). This is the last time we ever parse `nft -a list` output; delete the handle-parsing removal path (:521-563) afterwards.
3. `add_port_mapping()` root path calls `portmap::element_add`; `PortMapHandle` now stores just `host_port`; teardown calls `element_delete` — O(1), no listing.
4. Production `NftExecutor` impl wraps the existing `tokio::process::Command::new("nft")` call site (:524).

Rootless TCP-proxy mode is untouched.

**Book:** extend A1's section with the migration story (running clusters have per-port rules; the one-time sweep) and the C4 war story — why container NAT and the perimeter firewall live in separate tables and what happened when they didn't.

---

### B5 — heal-loop hardening (replaces the original B1-B4)

The original B1-B4 shipped in Stage 4 (§3.2). This step closes the three real gaps and makes the heal loop testable.

**Tests first:**

- Unit (`src/pickle/replication.rs`): `audit_orders_rarest_first` — candidate manifests ordered by ascending minimum holder count, capped at the per-tick limit (default 10).
- Integration (roadmap line 571, `tests/pickle_cluster.rs`): "under-replicated image auto-heals when a new node joins" — registry A holds an image with `redundancy = 2` and no peers; a peer B appears; one `heal_tick` later B holds all layers and the returned `UpdateLayerLocations` records it.
- Integration: leader-pull-first — a manifest whose layers live only on B (leader A lacks them): `heal_tick` on A first pulls the layers from B, then A counts as a holder.

**Implementation:**

1. **Extract the tick body** from `src/bin/bun.rs:1125-1228` into `src/pickle/replication.rs`: `pub async fn heal_tick(catalog, store, self_node, peers, redundancy, max_per_tick, client) -> Vec<UpdateLayerLocations>`. `bun.rs` keeps only: leadership check, peers derivation, proposing the returned updates.
2. **Peers helper:** extract the inline derivation (`bun.rs:1139-1147`) into `src/cluster/` (next to `identity::raft_id_from_name`): `pub fn pickle_peers(members: &[…], registry_port: u16) -> Vec<pickle::replication::Peer>`. Shared by the heal loop, C2, and D2. Lives in `cluster/`, not `pickle/`, so pickle doesn't depend on mustard membership types.
3. **Rarest-first + cap:** sort candidates by ascending min-holder-count; process at most `max_per_tick` (10) per tick — replication-storm protection when a fresh node joins.
4. **Leader-pull-first:** when the leader lacks a candidate's layers, fetch them from a holder via the existing `pull_manifest_layers` (`pull.rs:70`) and include the leader's own holdership in the returned updates. This is what makes non-leader pushes and D2 cache fills eventually redundant.
5. **Loopback warning:** at cluster startup, warn when `registry_bind` is `127.0.0.1` — peer replication and P2P silently no-op otherwise (§2.9). The warning text must also say what binding wider means: the registry currently has **no auth/TLS** (codex review 2026-07-09), so a non-loopback bind should sit behind the perimeter firewall's cluster-node allowlist. Registry auth/mTLS is out of Phase 12 scope — `// TODO` it.

Accept as-built (do not change): the GC shape, config key names (`gc_retain_days`/`gc_interval_hours`), the hardcoded 60s tick.

**Book:** new section "Deleting data without losing it" — the M2 TOCTOU story and the two-phase GC through Raft as shipped; plus "self-healing as a loop invariant" — rarest-first, the per-tick cap, and why the leader pulls before it pushes.

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

### C2 — parallel executor and the `ClusterImageSource` seam

**Tests first:**

- Unit: a failed fetch retries once against an alternate holder; exhausted holders land the digest in the error.
- Integration (`tests/pickle_cluster.rs`): registry A holds a 5-layer image (5 × 20 MB of incompressible `rand` bytes = 100 MB); `ensure_image_local` on B fetches everything, `image_available_locally` is true, and wall-clock < 5s (roadmap target; localhost makes this comfortable — assert it anyway to catch accidental serialisation). Second test with 3 holders: the executed plan spread fetches across ≥2 peers.

**Implementation:**

- `pull_layers_parallel(plan, store, client, concurrency, timeout)` in `p2p.rs`: `tokio::task::JoinSet`, at most `concurrency` (default 4, config `p2p_concurrency`) in flight, each task calling the existing `pull_layer_from_peer` (`pull.rs:19`). One retry pass: failed digests re-planned against remaining holders. Everything wrapped in explicit `tokio::time::timeout`.
- `ensure_image_local(image_ref, ...) -> Result<bool>`: resolve the manifest from the catalog; `false` if absent (caller falls through to the external path / D2); otherwise plan + fetch missing layers and return `true`.
- **The seam is `ImageStore::pull_and_unpack`** (`src/grill/image.rs:182-271`), *before* the external client is built at :191. Give `ImageStore` an optional `ClusterImageSource` (builder method `with_cluster_source(...)` — pickle `BlobStore` handle, catalog access, peers via B5's `pickle_peers`), constructed in `src/bin/bun.rs` (grill construction ~:1310-1340, threaded through `src/grill/mod.rs:400-423`). On catalog hit: fill missing layers P2P into the pickle blob store, unpack the rootfs from pickle blob paths, skip the external client. On miss: existing external path (D2 intercepts later). **Note:** `pull_and_unpack` hardcodes `ClientProtocol::Https` (:192) — this seam is what makes cluster-pushed images deployable on other nodes at all, not just an optimisation. `ProcessGrill`/`AppleContainerGrill` untouched.
- Signature verification stays exactly where it is (schedule time, manifest level) — add a comment at the call site saying why P2P doesn't need its own check.

**Book:** extend C1's section — `JoinSet` introduced properly (structured concurrency vs Python's `gather`/Go's `errgroup`), bounded concurrency as backpressure, the retry-with-alternate-holder pattern, and the HTTPS-hardcode discovery (an "optimisation" that turned out to be a correctness fix).

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
```

(Trait justified: two implementations from day one.) `OciUpstream` wraps the existing `oci-distribution` dependency. **`external_registries` is a new config key** (the plan previously said "comes alive" — the struct no longer exists in config; add `ExternalRegistry { host, username, password_secret }` fresh to `[images]`), password resolved through the Sesame secret store at startup. Freshness rides on `ImageManifest.pushed_at`; `cache_recheck_secs` config, default 3600. Digest-pinned references (`@sha256:...`) are immutable — always `Hit` once present.

**Book:** new section "Caching other people's registries" — mutable tags as the whole problem (`redis:7` moves), HEAD-before-refetch, and testing an internet-facing feature with zero internet (the mock + counter pattern).

---

### D2 — pull-through wiring and integration

**Tests first:**

- Integration (`tests/pickle_cluster.rs`): an in-process Pickle registry acts as the *upstream* (it speaks OCI), wrapped with request counters. Node A `ensure_image_local` for `upstream-host/app:v1`: fetches from upstream (counter = manifest + blobs), commits to the catalog under `cache/...` with holders = {A}. Node B pulls the same ref: **zero upstream hits** — served by catalog + P2P ("first pull cached, second pull served locally", roadmap line 572).
- Scheduler exemption test (`src/meat/scheduler.rs`): with `require_signatures = true`, an unsigned manifest under `cache/...` passes; an unsigned manifest elsewhere is still refused (decision §2.8).

**Implementation:**

- Extend `ensure_image_local`'s miss path: if the reference has a registry host and `[images] pull_through = true` (new bool, default true): `decide()` → on `Refetch`, `fetch_manifest` + `fetch_blob`s → `store.write_blob` → **`record_commit`** (exposed as `pub(crate)` or a `PickleState` method — the original plan's `commit_and_replicate()` helper is dropped; the leader heal loop (B5) provides redundancy for cache fills, exactly as it does for pushes) under the `cache/<host>/<repo>` repository with holders = {self}.
- Concurrent-fetch guard: a per-image `tokio::sync::Mutex` keyed map (or a single mutex around the fetch — simplest correct thing) so two instances landing at once don't double-download.
- Scheduler exemption per §2.8, with a `// TODO(Phase N)` pointing at upstream trust policy.
- Manual acceptance (not CI): on the Lima cluster, deploy a `docker.io/library/alpine`-based app on two nodes; confirm node 2's pull never touches Docker Hub (registry logs).

**Book:** extend D1's section — the read-through cache shape (miss → fill → serve), the double-download guard, and the signature-exemption judgement call spelled out honestly.

---

### E0 — wire `VolumeManager` (fixes review M21; prerequisite for E1/E2)

**Tests first:**

- Unit: the prepare path calls `create_managed_volume` for each managed `VolumeSpec` (no `source`) and skips host-path volumes.
- Integration (Lima, `RELIABURGER_RUNC_TESTS=1`): deploy `examples/phase-1/container-volumes.toml` — the container starts (today it fails ENOENT), writes to `/data`, survives a restart with data intact.
- macOS-runnable: `proc-volumes.toml` equivalent through `ProcessGrill` — the managed directory exists after prepare.

**Implementation:**

- Construct `VolumeManager` from `config.storage.volumes` (killing that dead-config entry) and invoke it in the container prepare flow immediately before mount-entry generation (`src/grill/oci.rs:318-325` computes the paths; create first, then generate the entries; the agent call site is `drive_instance_startup`, `src/bun/agent.rs:2610-2681`).
- **No deletion on Stop** (decision §2.12): `AgentCommand::Stop` fires on user stops, reconciler rebalances, and around Phase 14 upgrades (instance adoption re-attaches, `agent.rs:706-851`). Orphaned volume trees stay; add a `// TODO(Phase 15)` for explicit cleanup.
- Update `docs/progress.md`'s "dead config" line for `[storage] volumes`.

**Book:** new section "Volumes that actually mount" — the library-not-wired trap (write the code, forget the caller), how the integration test that *boots the example config* is the only test that would ever have caught it, and why deletion is deliberately not wired (the reconciler-rebalance data-loss trap).

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

- Unit (new `src/grill/snapshot.rs`): snapshot naming (`{RFC3339-compact}-{suffix}` — inject the clock in tests), `SnapshotMeta` serde round-trip, restore-guard (restore while instances run → `SnapshotError::AppRunning`), `UnsupportedFilesystem` on non-Btrfs.
- Integration (Lima, btrfs gate, root — roadmap line 573 verbatim): create volume on the loopback btrfs, write `v1`, snapshot, overwrite with garbage, restore, read `v1` intact.
- API tests (tower, macOS): endpoints return `UnsupportedFilesystem` cleanly on a non-btrfs volumes dir; auth required.

**Implementation:**

- Layout: `{volumes_dir}/.snapshots/{ns}/{app}/{mount-path-slug}/{name}` — read-only snapshots via `btrfs subvolume snapshot -r <live> <dest>`, each with a `meta.json` (`SnapshotMeta { namespace, app, volume_path, name, created_at, size_bytes, uploaded: bool }`).
- Restore (app must be stopped; agent checks the supervisor — works for Phase 14 adopted instances too, since adoption seeds the supervisor): `btrfs subvolume delete <live>` then `btrfs subvolume snapshot <snap> <live>` (writable).
- API (behind the auth layer, `src/bun/api.rs`; route table checked — no collisions with `/v1/upgrade/*` or anything else): `POST /v1/snapshots/{ns}/{app}` (create, optional `{"volume": "/data"}` body — default: all managed volumes), `GET /v1/snapshots/{ns}/{app}` (list), `POST /v1/snapshots/{ns}/{app}/restore` (`{"name": ...}`), `DELETE /v1/snapshots/{ns}/{app}/{name}`.
- CLI: `relish snapshot create|list|restore|delete <app> [-n <ns>] [--name <snap>]` via `BunClient`.
- Requires E1's subvolume-backed volumes (only subvolumes snapshot) — create managed volumes as subvolumes whenever the filesystem is btrfs, even without a size limit.

**Book:** new section "Point-in-time for free" — CoW snapshots as O(1) metadata operations, why restore requires a stopped app, and the deliberate "Btrfs only, loud error elsewhere" scope decision (rejected: rsync-style copy fallback — silently *slow* "snapshots" are worse than an honest error).

---

### E3 — scheduled snapshots and object-store upload

**Tests first:**

- Unit: retention pruning (`retain = 3` keeps the newest 3 per volume — pure function over `SnapshotMeta` lists); scheduler due-logic driven **manually** (no `tokio::spawn` under `start_paused` — construct the loop's tick body as a function and call it).
- Integration (macOS-runnable for the upload half): with a fake snapshot directory tree, the upload pass tars it (tar + flate2 — both already deps) and writes `{prefix}/{ns}/{app}/{name}.tar.gz` through `object_store` to a `file://` destination; `uploaded` flips true in `meta.json`; a second pass uploads nothing (checkpoint semantics).
- Integration (Lima, btrfs gate): 1s interval loop produces a snapshot of a live app's volume and prunes past `retain`.

**Implementation:**

- Config, new section (parsed in `src/config/node.rs`):

```toml
[storage.snapshots]
interval_secs = 86400   # 0 = disabled (default)
retain = 7
upload_url = "s3://bucket/prefix"   # optional; file:// and gs:// also accepted
```

- `Cargo.toml`: `object_store = { version = "0.12", features = ["fs", "aws", "gcp"] }`. Build the store via `object_store::parse_url`; credentials from the standard env vars (document in `docs/README.md`). Run `cargo tree -d` after — no duplicate majors.
- Loop spawned from the agent startup (interval + `CancellationToken`): snapshot every managed volume of currently registered apps → prune → upload un-uploaded snapshots (`tokio::task::spawn_blocking` for the tar; stream the file to the store).
- Failure policy: upload errors log and retry next tick (the `uploaded` flag is the checkpoint); snapshot errors for one app don't abort the sweep.

**Book:** extend E2's section — the interval-vs-cron decision (§2.7) argued openly, `spawn_blocking` for CPU-bound tar work (never block the runtime), and `object_store` as the "one trait, many clouds" abstraction with the `file://` test trick.

---

### F1 — wire `SubmitBatch`

**Tests first:**

- Unit: `BatchSubmitRequest`/`BatchSubmitResponse` serde; the `AggregatedState` + membership → `Vec<NodeCapacity>` mapping (deterministic — capacity is commitments, not live usage).
- Integration (TestHarness, macOS, ProcessGrill): submit 3 proc jobs to a single node → `202` with a batch id; poll `GET /v1/batch/{id}` until `done`, `completed = 3`. Failure path: a job whose command exits non-zero ends `failed = 1`. Dispatch path: a second harness receives `POST /v1/batch/run` directly and reports back to the first via the callback URL (full two-node flow without cluster plumbing).

**Implementation** (new `src/bun/batch.rs` for types + handlers; routes replace the 501 at `api.rs:2614-2628`):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchSubmitRequest { pub jobs: Vec<BatchJobSubmission> }   // full specs, not names
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchJobSubmission { pub name: String, pub spec: AppSpec } // the [job.*] section, is_job semantics

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchSubmitResponse { pub batch_id: u64, pub assigned: usize, pub unschedulable: Vec<String> }
```

Flow on `POST /v1/batch`:

1. **Leader-forward** (copy the `cluster_apply` shape, `api.rs:842-882`): followers proxy the raw body to `{leader}/v1/batch` with the service token; 503 when no leader. `GET /v1/batch/{id}` forwards the same way; the `BatchTracker` lives leader-side (`Arc<Mutex<BatchTracker>>`; GC'd daily with the existing `gc(max_age)`; tracker loss on leader change orphans batch ids harmlessly — accepted).
2. Capacity: map the leader's `AggregatedState` + membership → `Vec<NodeCapacity>` — the same ~10 lines as `build_cluster_cache` (`src/cluster/orchestrate.rs:320-365`). New `ApiState` field `aggregated_rx: Option<watch::Receiver<AggregatedState>>` (the receiver already exists in the orchestration tuple, `bun.rs:354-358`).
3. `schedule_batch` (`meat/batch.rs:51`) → `BatchAllocation` → `BatchTracker::register`.
4. Group assignments by node; self-assignments start locally via the existing job path (supervisor, `is_job = true`); remote groups get `POST /v1/batch/run { batch_id, callback_base_url, jobs }` (service-token-authenticated, same middleware). Batch does NOT ride the placements reconciler (decision §2.6).
5. The running node watches each job to its terminal state and posts `POST /v1/batch/{id}/report { job_name, status: "completed" | "failed" }` to the callback; the tracker marks accordingly. Local jobs short-circuit without HTTP.
6. `GET /v1/batch/{id}` → `BatchSummary` (exists) as JSON. Unknown id → 404.

CLI: `relish batch <file>` serialises the full `[job.*]` specs into the request (`src/relish/commands.rs`, `client.submit_batch` signature changes) and prints the batch id + unschedulable list. New `relish batch-status <id>`.

**Book:** new section "A thousand jobs, fifty envelopes" — the dispatch design in full: the Raft-placements alternative *now exists for deploys* and why run-to-completion jobs still don't use it (the reconciler would kill moving jobs and re-run finished ones), greedy bin-packing recap, and the callback-URL completion pattern with its failure mode.

---

### F2 — wire `SubmitBuild` (sync → async)

The Stage 4 merge left a working **synchronous** build handler (`api.rs:2630-2770`): context fetch from the registry (X1 fixed), local buildah, push, 200. F2 refactors it to the async shape — the sync version strands the CLI (300s client timeout vs 900s builds) and can't select a remote builder or sign.

**Tests first:**

- Unit: builder selection (self if buildah present, else first peer reporting `has_buildah`, else `NoBuilderAvailable`); `BuildState` machine (`Pending → Running → Completed(BuildResult) | Failed(String)`); request serde.
- macOS-runnable (this is why async wins): submit → 202 + id; poll → terminal state; unknown id → 404; no-builder → 503. None need buildah installed.
- Integration (Lima, **new gate `RELIABURGER_BUILDAH_TESTS=1`**): `relish build` a trivial context (`FROM scratch` + `COPY hello.txt /`) — the build runs, the manifest appears in the catalog **signed**, `GET /v1/build/{id}` reports `completed` with layer count and size.

**Implementation:**

1. Lift the existing handler body (context fetch → extract → buildah build+push) into a `build_runner` task (`src/bun/build_runner.rs` or alongside the handler); wrap in `tokio::time::timeout` (config `build_timeout_secs`, default 900). Keep `registry_port` in the request body as-built.
2. `POST /v1/build` returns `202 { build_id }`; track in a `HashMap<u64, BuildState>` behind the agent (`BatchTracker`'s shape doesn't fit a single-job-with-payload lifecycle; don't force it). `GET /v1/build/{id}`.
3. Builder selection: probe `buildah --version` once at worker startup; report it as a new `has_buildah: bool` (`#[serde(default)]`) field on the StateReport/worker snapshot — no separate capability endpoint. Prefer local when capable; else dispatch `POST /v1/build/run` to a capable peer (service token); else 503.
4. Sign after push via the existing `AgentCommand::SignImage` path (`src/bun/agent.rs:3619-3650`) — the pushed manifest must pass a `require_signatures` policy.
5. CLI: `relish build` polls `/v1/build/{id}` to a terminal state and prints the `BuildResult`.
6. `relish dev`: add `buildah` to VM provisioning and `RELIABURGER_BUILDAH_TESTS=1` to the `dev test` env block.

**Book:** new section "Building where the tools are" — capability-based placement, why the synchronous version had to go (client timeouts as a design forcing function), `tokio::process` with timeouts, and the tidy loop this closes: build → push → replicate (heal loop) → sign → verify at schedule → deploy.

---

### G — chapter assembly and docs sweep (final step)

1. `docs/book/12-squeezing-every-drop.md`: write the chapter intro ("why optimise now") and the closing "Lessons learned" (the M2 TOCTOU, the library-not-wired trap from M21, the HTTPS-hardcode correctness surprise, argv-vs-shell quoting, testing internet features offline, the plan that had to be re-verified against a moving tree). Verify every section from steps A-F landed; fix cross-references; one pass for the style guide.
2. `docs/progress.md`: tick every Phase 12 box; flip `[~]` book to `[x]`; note M21 resolved.
3. `docs/README.md` + top-level `README.md`: new CLI commands (`relish snapshot ...`, `relish batch-status`), new endpoints, the uniform-registry-port and non-loopback `registry_bind` constraints, snapshot config example, updated test counts, Phase 12 marked complete.
4. `docs/plans/2026-07-02-review-codebase.md` stays untouched (point-in-time review) — progress.md is the living tracker.

## 6. Config additions summary

As-built `[images]` already has: `max_storage`, `redundancy`, `gc_retain_days`, `gc_interval_hours`, `registry_port`, `registry_bind`, `trust_policy`. The original plan's `push_sync`, `gc_retain_tags`, `gc_interval_secs`, `audit_interval_secs` are **dropped** (the async design made them moot; the heal tick stays hardcoded 60s).

| Key | Section | Default | Step |
|-----|---------|---------|------|
| `p2p_concurrency` | `[images]` | 4 | C2 (new) |
| `pull_through` | `[images]` | true | D2 (new) |
| `cache_recheck_secs` | `[images]` | 3600 | D1 (new) |
| `external_registries` | `[images]` | [] | D1 (new — the struct no longer exists in config) |
| `build_timeout_secs` | `[images]` | 900 | F2 (new) |
| `volumes` | `[storage]` | /var/lib/reliaburger/volumes | E0 (comes alive) |
| `interval_secs` / `retain` / `upload_url` | `[storage.snapshots]` | 0 / 7 / none | E3 (new) |

Every new key gets a doc comment, a default, and an example in `docs/README.md`. No key is added without a reader.

## 7. Endpoint changes summary

| Endpoint | Change | Step |
|----------|--------|------|
| `POST /v1/batch` | 501 → real (leader-forwarding) | F1 |
| `POST /v1/batch/run` | new (service-token; node-to-node) | F1 |
| `POST /v1/batch/{id}/report` | new (service-token; callback) | F1 |
| `GET /v1/batch/{id}` | new (leader-forwarding) | F1 |
| `POST /v1/build` | sync 200 → async 202 | F2 |
| `POST /v1/build/run` | new (service-token) | F2 |
| `GET /v1/build/{id}` | new | F2 |
| `POST/GET/DELETE /v1/snapshots/...` | new (4 routes; no collisions — checked against the full route table incl. `/v1/upgrade/*`) | E2 |

All behind the existing `auth_middleware` route layer. The original plan's `GET /v1/capacity` is **dropped** (decision §2.10).

## 8. Test inventory (roadmap "Tests (write first)" → where they land)

| Roadmap test (roadmap.md:556-582) | Step | Where it runs |
|---|---|---|
| nftables map generation: syntax, incremental, rollback | A1 | everywhere (unit) |
| nftables maps behaviour parity + 1000-port stress | A2 | Lima (netns gate) |
| P2P chunk selection: rarest-first, balancing, dedup | C1 | everywhere (unit) |
| P2P property: arbitrary topologies complete | C1 | everywhere (proptest) |
| P2P: multi-layer pull from another node < 5s / 100 MB | C2 | everywhere (integration, localhost) |
| P2P: under-replicated auto-heal on node join | B5 | everywhere (integration) |
| Pull-through: manifest resolution, hit/miss/stale | D1 | everywhere (unit) |
| Pull-through: first pull cached, second served locally | D2 | everywhere (integration, in-process upstream) |
| Btrfs quota: creation, enforcement (write-beyond fails) | E1 | Lima (btrfs gate) |
| Volume snapshots: create/corrupt/restore intact | E2 | Lima (btrfs gate) |
| Volume snapshots: scheduled job + object-store upload | E3 | upload: everywhere; live loop: Lima |
| Parquet bloom filter construction/lookup/FPP | done | `src/ketchup/log_store.rs` |
| Zstd round-trip + >5x + random access | done | `src/ketchup/log_store.rs` |
| Batch submit/dispatch/track/status | F1 | everywhere (TestHarness) |
| Build end-to-end (context → buildah → signed manifest) | F2 | Lima (buildah gate) |

Note the deliberate deviations already recorded in progress.md: bloom filters are on `app`/`namespace` equality (blooms can't answer substring `LIKE` on `line`), and zstd is Parquet's native codec rather than a separate seekable-frame container. Both are settled — don't reopen them.

## 9. Gotchas checklist (read before each step)

1. **C4:** nothing in slice A touches `reliaburger_fw`; keep `ruleset_uses_isolated_table_name` green.
2. **nft argv:** pass `{ 30001 : 10.0.2.2 . 8080 }` as one argv element via `Command::args`; never build a shell string.
3. **Standalone mode:** every Pickle change must keep working with `council: None` (single-node, no cluster). The local catalog is the fallback, not legacy.
4. **The M2 fix lives in the state machine** (`apply_gc_report` refuses to empty a holder set) — already shipped; don't "improve" it from the loop side.
5. **Signatures:** P2P and pull-through move blobs; verification stays manifest-level at schedule time. The only policy change is the `cache/` exemption (§2.8) — nothing else.
6. **Capacity = commitments, not usage:** the reporting pipeline already sums per-instance requests (`reporting/worker.rs:214-221`); don't switch to live sysinfo numbers (nondeterministic tests).
7. **Batch must not ride the placements reconciler** (§2.6) — it Stops workloads on unassignment; completed jobs would look like drift.
8. **Volumes are never deleted on Stop** (§2.12) — Stop fires on rebalances and upgrades; deletion destroys data.
9. **Loop-mount teardown:** volumes created with `SizeBackend::LoopMount` must be unmounted before directory removal; btrfs subvolumes need `subvolume delete`, not `rm -rf`.
10. **No wall-clock in schedulers' tests:** inject clocks; drive interval loops manually (start_paused pitfall).
11. **Lima VM provisioning** gains `btrfs-progs` and `buildah`; `dev test` gains `RELIABURGER_BTRFS_TESTS=1 RELIABURGER_BUILDAH_TESTS=1`. Recreate the VM once (`relish dev clean` + recreate).
12. **`cargo tree -d` after the `object_store` feature change** — `aws`/`gcp` pull new transitive deps; no duplicate majors.
13. **`registry_bind` loopback default** silently disables replication/P2P in cluster mode — B5's startup warning; document in README (§2.9).
14. Book section per step; `make ci` per commit; show commit details, no re-asking within this staged plan; never amend.

## 10. Acceptance runbook

Run after slice D and again after G:

1. **macOS:** `make ci` — everything green, including the new unit/integration tests (P2P, pull-through, batch all run locally).
2. **Lima gated tests:** `relish dev test` — netns (incl. 1000-port stress), btrfs quota + snapshot, buildah build, existing runc/eBPF suites.
3. **Live 3-node cluster:** `relish dev create --nodes 3`, then inside node 1:
   - push an image to the local registry → catalog visible cluster-wide, holders ≥ 2 after a heal tick (B5);
   - deploy it with placement on another node → layers arrive P2P, container starts (C2);
   - deploy a `docker.io` image on two nodes → second node makes no upstream connection (D2);
   - create/restore a snapshot of a volume-backed app (E2);
   - `relish batch` 10 short jobs → `relish batch-status` shows them spread and completed (F1);
   - `relish build` the demo context → signed image deployable (F2);
   - kill and restart a node → catalog intact, GC/heal loops resume, no double-delete (as-built B + B5).
4. Update `docs/progress.md`, both READMEs, and confirm chapter 12 reads end-to-end.

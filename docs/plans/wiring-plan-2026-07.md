# Stage 4 Implementation Plan: Wiring the Library-Only Subsystems

**Date:** July 2026. **Executor note:** this plan is prescriptive; follow it sub-stage by sub-stage, in order, on ONE branch. Every sub-stage: tests first (red) → implement (green) → `make ci` → book section → tick progress.md → ask the user → commit. All findings referenced by ID come from [review-2026-07.md](review-2026-07.md). Verified against the codebase as of 2026-07-06.

## Scope decisions (already made — do not relitigate)

1. One sequential branch; each sub-stage lands green with an integration test that drives the **binary path** (BunAgent + `api::router()` / `cluster::runtime`), never just the library.
2. Full eBPF wiring (L8) is in scope; Linux-only behaviour is tested through the Lima dev cluster (`relish dev test`). The DNS responder (L9) is wired cross-platform with the complete M8 hardening.
3. Excluded from this plan: X6 (bare `relish` TUI — see [tui-plan-2026-07.md](tui-plan-2026-07.md)), C5(b) mTLS + L17 CRL (deferred from Stage 3b), X2/X7 (fixed in Stage 3), L12 Ketchup flat-file store (partially addressed by Stage 1 H9/H10; the rest is not a Stage 4 item).
4. Book chapters are updated **alongside** each sub-stage in the affected phase's existing chapter (mapping given per sub-stage). British English, no AI-tell transitions.

## 0. Verified ground truth (trust this; do not re-derive)

**Cluster infrastructure that exists and works** (`bun --cluster` → `cluster::runtime::start()`, `src/cluster/runtime.rs:89-305`):

- Gossip: UDP `UdpMustardTransport`, HMAC-signed (Stage 3b-t), membership via `membership_rx` watch.
- Raft council: durable log/vote (Stage 2), TCP RPC, `council: Option<Arc<CouncilNode>>` on `ClusterHandle` (`src/bun/agent.rs:285-296`), leader-only reconciler grows the council (max 7).
- Reporting flat-star: every node's `ReportWorker` (`src/reporting/worker.rs`) sends a `StateReport` (built from an `AgentSnapshot` fetched over `snapshot_rx`) to a council member every 5 s; the aggregator (`src/reporting/aggregator.rs`) publishes `aggregated_rx` — **held unread** at `src/cluster/runtime.rs:55`. StateReport resource usage is currently zeroed (L6).
- Auth: bearer middleware + per-route roles + a node **service token** (Stage 3b auth) — internal node-to-node HTTP can authenticate today.
- Multi-node in-process test harness pattern: `tests/cluster_gossip.rs` (3 real nodes, real UDP/TCP, unique temp dirs, distinct port blocks, poll-until helpers, `#[tokio::test(flavor = "multi_thread")]`).

**The central fact of Stage 4**: the Raft state machine already models desired state — `DesiredState { apps: HashMap<AppId, AppSpec>, … }` and `RaftRequest::AppSpec { app_id, spec: Box<AppSpec> }` (`src/council/types.rs:94-196`, applied in `src/council/state_machine.rs`) — but **nothing ever proposes an AppSpec** (zero call sites in `src/bun`, `src/bin`, `src/cluster`). Deploys are handled entirely locally by the receiving agent (`src/bun/agent.rs:938-1120`). There is no mechanism for one node to start an instance on another. W6 builds this.

**Per-subsystem entry points** (all library-complete, unit-tested, no production callers):

| ID | Entry point | Location | Key inputs |
|---|---|---|---|
| L1 | `Scheduler::schedule_app(&mut self, app_id, spec) -> Result<SchedulingDecision, ScheduleError>` | `src/meat/scheduler.rs:52-66` | `ClusterStateCache` (node capacities/instances), labels, image locality. H8: `WEIGHT_BIN_PACK=50` vs `WEIGHT_SPREAD=10` (`src/meat/score.rs:16-20`) |
| L2 | `DeployDriver` trait (8 methods, **synchronous**), `DeployOrchestrator<D>`, `execute_blue_green` | `src/meat/orchestrator.rs:17-47,68-140`; `src/meat/blue_green.rs` | M16: rollback leaks, dead-end states, `max_surge`/`max_unavailable` ignored |
| L3 | `run_autoscale_loop(tracker, app_provider, metric_provider, decision_tx, cancel)`, 30 s tick | `src/meat/autoscaler.rs:213` | `AutoscaleSpec` config exists (`src/config/app.rs:239+`); no consumer of `AutoscaleDecision` |
| L4 | `ReconstructionController::{new, on_leader_elected, on_report_received, check_timeout}`; `Correction::{MissingApp, ExtraApp, UnknownNode}` | `src/reconstruction/controller.rs` | `[reconstruction]` config parsed (`src/config/node.rs:200-226`), never read |
| L6/L11 | `RollupWorker` (60 s `NodeRollup` → council member); aggregator takes `rollup_store: Option<…>` — hardcoded `None` at `src/cluster/runtime.rs:~262`; `/v1/metrics/cluster` reads ApiState's store (also `None`); `fan_out_cluster_query`, `scrape.rs` uncalled | `src/mayo/rollup_worker.rs`, `src/mayo/query_fanout.rs` | |
| L7 | `run_proxy(WrapperConfig, Arc<RwLock<RoutingTable>>, CancellationToken)` | `src/wrapper/proxy.rs:33` | Agent already maintains the routing table (`src/bun/agent.rs:314`, rebuilt at 2095-2098). Missing: `[ingress]` config (TODO at `src/config/node.rs:31`) + spawn |
| L8 | `OnionEbpf::load(program_dir, cgroup_path)` behind `ebpf` feature | `src/onion/ebpf/loader.rs` | `onion_ebpf` always `None` (`src/bun/agent.rs:332,366`); `write_fault_bpf_entry` is a logging stub (`agent.rs:886-911`); `.bpf.o` never built/installed |
| L9 | `run_dns_responder(DnsConfig{listen_addr, upstream}, Arc<RwLock<ServiceMap>>, cancel)` | `src/onion/dns.rs:45` | Agent has `service_map` (`agent.rs:312`). M8 hardening list in W3 |
| L10/M2 | `gc_sweep(store, catalog, active_images, GcConfig)` (TOCTOU at `gc.rs:96-104`); `replicate_manifest(...)`; `pull_manifest_layers(...)` | `src/pickle/{gc,replication,pull}.rs` | Council state machine **already** holds `manifest_catalog` with Raft entries `ManifestCommit`/`UpdateLayerLocations`/`GcReport`/`AttachSignature`/`DeleteTag` + `council.manifest_catalog()` accessor (Stage 3b-t). But `src/bin/bun.rs:~472` creates a SEPARATE unsynced `Arc<RwLock<ManifestCatalog>>` for the Pickle API; push handlers hardcode `holder_nodes={0}` (`src/pickle/api.rs:339,376,462`) |
| L13 | `execute_sync(repo, GitOpsConfig, current_apps, autoscale_overrides, last_applied_sha) -> SyncOutcome` (pure) | `src/lettuce/sync.rs:39` | `GitOpsConfig` optional in NodeConfig (`node.rs:30`), never read; `/v1/gitops/webhook` exists, `gitops_webhook_tx` always `None` (`api.rs:69,107,1798-1814`); H12: `is_key_trusted` falls through to `true` (`src/lettuce/verify.rs:78-87`) |
| L14/L15 | `evaluate_safety(request, SafetyContext)` (`src/smoker/safety.rs:13`) never called; real primitives exist: `kill_process`/`pause_process`/`resume_process` (`src/smoker/process.rs`), disk-IO throttle + memory/CPU pressure (`src/smoker/resource.rs`); transport blocklists exist and are consulted (`src/mustard/transport.rs:202`, `src/council/network.rs:334-376`) but never populated — `InjectPartition`/`InjectFault` only insert into the registry (`agent.rs:721-767`) | | |
| L16 | `resolve_egress_entries(allow_list)`, `egress_to_bpf_entries(cgroup_ids, resolved)`, `re_resolve_egress_async` | `src/sesame/egress.rs` | Supervisor sets `egress: None` (`src/bun/supervisor.rs:~488`) |

**Quick-fix facts**:

- H11: `append_section`/`append_fields` in `src/relish/fmt.rs:49-74` don't recurse — `[app.web.health]` round-trips to invalid TOML written over the user's file (`commands.rs:704-706`).
- M17: `src/relish/k8s_import.rs` drops main-container `command`/`args` (~390-493), ignores `env.valueFrom` (~436-442), never sets `namespace` (~557).
- X4: `--grep/--since/--json-field` parsed then bound to `_grep`/… (`src/bin/relish.rs:~49-55`; `commands.rs:517-519`). Server already supports `grep`/`start`/`end` (`LogsQuery`, `api.rs:380-386`; also `/v1/logs/entries` + `/v1/logs/query`).
- X5: apply/deploy fall back to dry-run and return `Ok(())` when the agent is unreachable (`commands.rs:46-53,535-542`).
- X3: `rollback` fetches history and prints advice (`commands.rs:584-617`); `DeployHistoryEntry` (`src/meat/deploy_types.rs:383-392`) has `image` but NOT the spec.
- X1: `relish build` posts context to :9117 (`commands.rs:880` → `context_upload_url` in `src/pickle/build.rs:226`) and `/v1/build` is a permanent 501 (`api.rs:1687`). `build.rs` has `validate_build`/`tar_context`/`execute_build`.

## 1. Design decisions (settled here; the executor implements, not re-designs)

**D1 — Remote dispatch = Raft desired state + per-node reconciliation (not push-RPC).**
The leader schedules and records placements in the Raft state machine; every node reconciles itself against "what is assigned to me". Chosen over a push `POST /v1/internal/instances` RPC because: the desired-state machinery already exists and is durable; reconciliation is idempotent and self-heals after crashes/partitions (a push RPC needs retry/ordering/failure bookkeeping the state machine gives us for free); and reconstruction (W9) and GitOps (W10) then reuse the same write path. Concretely:

- New Raft entry `RaftRequest::Placements { app_id: AppId, placements: Vec<PlacementEntry> }` with `PlacementEntry { node: NodeId, replica_index: u32 }`; `DesiredState` gains `placements: HashMap<AppId, Vec<PlacementEntry>>` (**`#[serde(default)]` — snapshot compatibility**).
- Workers are not Raft members (council ≤ 7), so they learn assignments by **polling the leader**: new protected endpoint `GET /v1/placements/{node_id}` (service-token auth from Stage 3b) served from the leader's state machine, returning `{ apps: [{ app_id, spec_hash, replicas_for_node, spec }] }`. 2 s poll from each node's agent; ETag/`spec_hash` short-circuits no-ops.
- Each agent runs a **placement reconciler**: diff desired-for-me vs supervisor instances → start missing / stop extra, through the existing local deploy machinery.
- `POST /v1/apply` under `--cluster`: the receiving node proposes `AppSpec` to Raft (forwarding to the leader via `CouncilNode` — use its existing client-write path; grep `CouncilNode` for the propose/`client_write` method) instead of deploying locally. Single-node (no `--cluster`) keeps today's local path unchanged.

**D2 — `DeployDriver` goes async.** The trait is synchronous and cannot drive HTTP/agent calls without blocking. Convert `DeployDriver` + `DeployOrchestrator`/`execute_blue_green` to `async fn` (plain `async fn` in trait + generics, same pattern as elsewhere in the codebase; update the existing lib tests mechanically). The production `ClusterDeployDriver` talks to target nodes' authenticated APIs and to Raft.

**D3 — Keep the flat-star reporting topology.** Wiring the full consistent-hash tree (assignment.rs multi-level) buys nothing below ~thousands of nodes; the council cap is 7 and reports are 5 s apart. W4 keeps flat-star, fills in the zeroed resource usage, and documents the tree as deferred (book note in ch. 11).

**D4 — M2 GC fix: Raft serialises deletions.** `gc_sweep` only *nominates* candidates. The node proposes a `GcReport` to Raft; the state machine (single-threaded, serialised by the log) decides which deletions are safe (holder count stays ≥ 1 after this node's removal, no in-flight manifest references) and the applied entry tells the node what it may physically delete. No local deletion before the Raft ack. Blobs with empty holder sets are only deletable when older than a grace window (mid-push protection).

**D5 — Egress is eBPF-only.** No nftables fallback in this stage (the perimeter firewall in `src/firewall` is a different table/lifecycle; mixing per-app egress into it risks re-introducing C4). Non-Linux/non-eBPF builds get a logged warning that `[egress]` is unenforced. Documented in ch. 10.

**D6 — X1 build: wire the minimal honest path, descope gracefully.** Read `execute_build` first. If it shells out to a builder available in the dev VMs, wire: `relish build` → tar context → upload via the standard Pickle blob endpoints (not the phantom `/v2/_buildcontext`) → `POST /v1/build` executes `execute_build` in `spawn_blocking` → image lands in Pickle. If the builder dependency is absent, implement the honest error path (`/v1/build` returns a clear "requires <builder>" message, `relish build` exits non-zero) and leave `TODO(Phase 12)`. Either way the current lie (silent 501 + wrong port) is removed.

**D7 — Fault types map to mechanisms honestly.** kill/pause → signals via supervisor PIDs (cross-platform); CPU burn → spawn_blocking burn loops (cross-platform); memory pressure + disk-IO throttle → cgroups (Linux); delay/drop/dns/bandwidth → eBPF (W12; until then the API **rejects** these with "requires the ebpf feature" instead of recording fake success); partition → transport blocklists (cross-platform, real).

## 2. Sub-stages

Execute strictly in order W1 → W12. Common loop for every sub-stage: write the named tests first; implement; `make ci`; update the named book chapter section; tick the progress.md items; ask the user; commit.

---

### W1 — CLI/library quick fixes (H11, M17, X4, X5)

**Goal:** clear the four self-contained fixes before any wiring. No cluster dependencies, high test leverage.

Tests first:

- `fmt_roundtrips_nested_health_ingress_autoscale_tables` — format `examples/`-style configs with `[app.web.health]`, `[app.web.deploy]`, nested `[app.*.ingress]`; output must re-parse to an identical `toml::Value` tree.
- `fmt_refuses_to_write_when_roundtrip_fails` — inject a formatter bug via a crafted value; file on disk untouched, non-zero exit.
- `k8s_import_preserves_command_args_valuefrom_namespace` — import a Deployment fixture with `command`+`args`, `env.valueFrom` (SecretKeyRef + FieldRef), `metadata.namespace: staging`; assert `AppSpec.command == command ⧺ args`, namespace set, valueFrom entries surfaced as warnings (not silently dropped).
- `logs_flags_reach_the_server` — TestHarness: `--grep` filters lines server-side (assert via the client call params + returned lines), `--since 5m` maps to `start`, `--json-field` extracts client-side.
- `apply_exits_nonzero_when_agent_unreachable` — no agent running: `relish apply` returns an error (plan still printed), exit ≠ 0; new explicit `--dry-run` flag returns 0.

Implementation:

- `src/relish/fmt.rs`: make section emission recursive (`append_section` recurses into nested tables building dotted headers); before writing, parse the produced string and compare `toml::Value`s — on mismatch, error out without touching the file (H11).
- `src/relish/k8s_import.rs`: concatenate container `command`+`args` into `AppSpec.command`; populate `namespace` from `metadata.namespace`; for `env.valueFrom` emit an explicit warning list in the command output and a `# TODO: map secret/configmap ref` comment in the generated TOML — do NOT invent a secrets mapping (M17).
- `src/bin/relish.rs` + `src/relish/{commands,client}.rs`: pass `grep` through to `/v1/logs` (param exists), parse `--since` (humantime-style "5m"/"2h" — write a small parser, no new crate) into epoch `start`, apply `--json-field` extraction client-side (X4).
- `commands.rs`: unreachable-agent fallback returns `Err(RelishError::AgentUnreachable(...))` after printing the plan; add `--dry-run` to `apply`/`deploy` for the intentional plan-only path (X5). Call out in the commit message that this changes exit-code behaviour.

Files: `src/relish/fmt.rs`, `src/relish/k8s_import.rs`, `src/relish/commands.rs`, `src/relish/client.rs`, `src/bin/relish.rs`.
Book: ch. 9 "The Full Package" — new subsection on honest CLI failure modes (exit codes as API) and the TOML round-trip guard.
Progress: tick `H11`, `M17`, and the X4/X5 parts of the X-series item (note X1/X3 pending W5/W7, X6 → TUI plan).
Commit: `relish: fix fmt recursion, k8s import fidelity, logs flags, dry-run exit codes`. ~350 LOC.

---

### W2 — L7: bind the Wrapper ingress listener

**Goal:** a real HTTP(S) reverse proxy in front of deployed apps, fed by the routing table the agent already maintains.

Tests first:

- `ingress_routes_request_to_healthy_backend` — TestHarness + `[ingress] enabled` on ephemeral ports; deploy an app with an `ingress` host; `GET` the proxy port with `Host:` header → app's response.
- `ingress_returns_502_when_no_backends` — route exists, zero healthy backends.
- `ingress_rate_limit_returns_429` — configure the route's rate limit low; burst; assert 429s.
- `ingress_disabled_by_default` — no `[ingress]` section → no listener bound (port closed).

Implementation:

- Add `[ingress]` section to `NodeConfig` (`src/config/node.rs`, replacing the TODO at line 31): `enabled: bool` (default false), `http_port` (default 8080), `https_port` (default 8443), `tls_cert`/`tls_key: Option<PathBuf>`, `max_connections`. Ports are configurable so tests bind ephemeral.
- `src/bin/bun.rs`: when enabled, spawn `wrapper::proxy::run_proxy(config.into_wrapper_config(), agent_routing_table, cancel)` — the agent's `Arc<RwLock<RoutingTable>>` must be shared out of `BunAgent` (add an accessor or construct the Arc in `bun.rs` and pass it in, matching how other shared stores are handled).
- Verify the deploy path populates `ingress_configs` from `AppSpec.ingress` (it feeds `rebuild_routing_table` at `agent.rs:2095-2098` — confirm inserts happen on deploy; wire if missing).

Files: `src/config/node.rs`, `src/bin/bun.rs`, `src/bun/agent.rs` (accessor), `src/wrapper/types.rs` (config conversion).
Book: ch. 3 "Talking to Each Other" — "Switching the proxy on": why the listener lives in the binary not the agent task, sharing state via `Arc<RwLock<…>>` across subsystems.
Progress: tick `L7`. Commit: `bun: bind the Wrapper ingress listener behind [ingress] config`. ~300 LOC.

---

### W3 — L9: DNS responder, hardened (M8)

**Goal:** `.internal` names resolve to service VIPs from any container; the responder survives hostile input.

Tests first (unit against the packet path + integration on an ephemeral socket):

- `resolves_registered_app_to_vip`; `unmatched_internal_returns_nxdomain_and_never_hits_upstream` (fake upstream socket asserts zero packets); `aaaa_query_returns_empty_noerror_not_a_record`; `upstream_reply_with_wrong_id_or_source_is_dropped`; `recv_error_does_not_kill_the_loop` (send a malformed/oversized datagram, then a valid query still answers); `truncated_response_sets_tc_bit`.

Implementation (all in `src/onion/dns.rs`):

- Loop: `recv_from` errors → log + `continue` (never `?` out).
- Upstream forwarding: per-query `tokio::spawn` with a `Semaphore` (e.g. 64 permits); validate reply source addr + query ID before relaying; 2 s timeout → SERVFAIL.
- Buffer 1232 bytes (EDNS0 practical limit); set TC when answers don't fit.
- Honour QTYPE: A answered from the service map; AAAA for `.internal` → empty NOERROR; other types for `.internal` → NOTIMP; non-`.internal` → forward.
- Unmatched `.internal` → NXDOMAIN locally (no upstream leak).
- Config: `[dns] enabled/listen/upstream` in `NodeConfig`; spawn from `bun.rs` with the agent's service map.
- Container `resolv.conf`: for runc, write `nameserver <bridge ip>` when generating the netns/rootfs config (`src/grill/netns.rs` / `oci.rs`); ProcessGrill/apple documented as using host DNS (book note).

Files: `src/onion/dns.rs`, `src/config/node.rs`, `src/bin/bun.rs`, `src/grill/netns.rs`.
Book: ch. 3 — "A DNS server that doesn't fall over": UDP failure modes, spoofing basics, why NXDOMAIN-not-forward for the internal zone.
Progress: tick `L9` half of the L8/L9 item + note M8 closed. Commit: `onion: start the DNS responder with M8 hardening`. ~350 LOC.

---

### W4 — L6/L11: rollups + real resource reporting

**Goal:** `/v1/metrics/cluster` answers; StateReports carry real usage (prerequisite for the scheduler and autoscaler).

Tests first:

- `rollup_worker_delivers_node_rollups_to_the_leader` — 3-node in-process cluster (cluster_gossip.rs pattern); poll leader's rollup store until it holds entries from all nodes.
- `metrics_cluster_endpoint_serves_aggregated_rollups` — same cluster; `GET /v1/metrics/cluster` on the leader returns non-empty aggregates (and a clear "not the leader, ask <addr>" or fan-out on followers — pick fan-out via `fan_out_cluster_query` if trivially wireable, otherwise leader-redirect and document).
- `state_reports_carry_nonzero_capacity_and_usage` — aggregated view shows real cpu/mem capacity (honouring `[resources] reserved_*`).

Implementation:

- `src/cluster/runtime.rs`: create `Arc<RwLock<RollupStore>>` on council nodes; pass to the aggregator (replace the hardcoded `None` at ~262); spawn `RollupWorker` per node with the node's `MayoStore` handle (exported from `bun.rs` where the store is built).
- Plumb the rollup store into `ApiState` so `metrics_cluster_handler` finds it.
- Fill `StateReport` resource usage: extend the `AgentSnapshot` reply with capacity (from `[resources]` config + system totals) and per-instance usage the agent already tracks; `[resources] reserved_cpu/reserved_memory` are now read (dead-config item cleared).
- `[metrics] rollup_*`/`scrape_interval_secs`: read what the RollupWorker/scrape loop actually uses; delete the rest from `NodeConfig` (remove-or-wire rule).

Files: `src/cluster/runtime.rs`, `src/bin/bun.rs`, `src/reporting/{worker,aggregator}.rs`, `src/bun/api.rs`, `src/config/node.rs`.
Book: ch. 11 "Eyes Everywhere" — "Rolling it up": flat-star justification (D3), why the tree is deferred.
Progress: tick the `L6/L11` item. Commit: `mayo+reporting: spawn rollup workers, live cluster metrics, real capacity reports`. ~400 LOC.

---

### W5 — L10/M2: one Pickle catalog, replication, GC (+ X1 decision)

**Goal:** the image catalog survives restarts and is cluster-consistent; layers replicate to `redundancy` holders; GC cannot lose the last copy.

Tests first:

- `catalog_survives_restart` — single node: push image, restart the pickle state (new router from same data dir / council), manifest still listed.
- `push_records_real_holder_and_replicates_to_peer` — 2-node cluster: push to node A; poll until node B holds the layers and the catalog shows 2 holders.
- `deploy_pulls_image_from_peer_when_missing_locally` — image only on A; deploy on B; B pulls layers from A before create (binary-driven: through the placement reconciler once W6 lands — until then, through B's local deploy with a pull step).
- `gc_never_deletes_the_last_copy` — unit + state-machine test reproducing the M2 race: two concurrent GcReports each claiming one of two holders; state machine must reject the second.

Implementation:

- **Single source of truth**: on cluster nodes the Pickle API's catalog IS the council's (`council.manifest_catalog()` accessor from Stage 3b-t + a watch/refresh on applied entries); pushes propose `ManifestCommit`/`UpdateLayerLocations` with the node's real raft id as holder (kill the `{0}` hardcodes at `pickle/api.rs:339,376,462`). Single-node mode: persist the local catalog to `{data_dir}/pickle/catalog.json` on change, load on boot.
- **Replication loop** (leader-only task): every `[images].replication_interval`, for manifests with holders < `[images].redundancy`, `select_peers` from membership → `replicate_manifest` → propose `UpdateLayerLocations`.
- **GC** per D4: per-node timer (`[images].gc_*`) runs `gc_sweep` → proposes `GcReport`; physical deletion only for blobs the applied entry confirms; empty-holder blobs deletable only past a grace window.
- **Pull-on-deploy**: before `grill` create, if `!image_available_locally`, resolve a holder from the catalog and `pull_manifest_layers`.
- **X1** per D6: read `execute_build`, wire the minimal path or the honest error; either way point `relish build` at the Pickle port with the standard blob upload.
- `[images]` config (`max_storage`/`redundancy`/`gc_*`) now read; delete anything still unused.

Files: `src/bin/bun.rs`, `src/pickle/{api,gc,replication,pull}.rs`, `src/council/state_machine.rs` (GcReport safety logic), `src/relish/commands.rs`, `src/config/node.rs`.
Book: ch. 5 "Where the Images Live" — "One catalogue, many holders": Raft as the arbiter of deletion (M2), the TOCTOU story.
Progress: tick `L10/M2` and the X1 part of the X-series item. Commit: `pickle: raft-backed catalog, replication and safe GC`. ~550 LOC.

---

### W6 — L1: scheduler → placements → per-node reconciliation (+ H8)

**Goal:** `relish apply` against any cluster node places replicas across the cluster; each node starts exactly what is assigned to it. **This is the structural core — take it slowly.**

Tests first:

- `replicas_spread_across_distinct_nodes` (H8, unit) — 3 equal nodes, replicas=3 → three distinct nodes. Fix the existing `schedule_fixed_replicas_places_all` to assert distinctness.
- `apply_on_any_node_places_across_the_cluster` — 3-node in-process cluster + ProcessGrill; apply replicas=3 via node A's HTTP API; poll until each node's `/v1/status` shows exactly one instance.
- `placement_reconciler_is_idempotent` — re-deliver the same placements; no restarts (instance ids stable).
- `node_failure_reschedules_after_membership_change` — kill node C's runtime; poll until the leader reassigns C's replica and A or B starts it. (Generous timeouts; poll-until pattern.)
- `single_node_mode_unchanged` — existing integration suite stays green (no `--cluster` → local path).

Implementation (per D1):

- `src/meat/score.rs`: `WEIGHT_SPREAD` 10 → 40 (spread beats bin-pack for same-app replicas; leave the other weights). Justify in the book.
- Raft: add `RaftRequest::Placements` + `DesiredState.placements` (**`#[serde(default)]`**); apply logic in the state machine.
- Leader scheduling task (in `cluster::runtime`, gated on leadership via `raft_metrics_rx`): on AppSpec/membership change, build `ClusterStateCache` from membership + aggregated reports (capacities from W4), run `Scheduler::schedule_app`, propose `Placements`. Node labels from `[node] labels` config flow into membership/StateReport here (dead-config item cleared).
- `GET /v1/placements/{node_id}` (protected, service-token role): serves specs+assignments for that node from the leader's state machine.
- Agent placement reconciler: 2 s poll of the leader endpoint (leader address from membership + known API port); diff vs supervisor; start/stop through the existing local instance machinery. Skip cycles when `spec_hash`es match.
- `/v1/apply` under `--cluster`: propose `AppSpec` to Raft (forward to leader via `CouncilNode`), return the SSE stream reporting proposal + placement progress (poll the state machine for placements before closing the stream).

Files: `src/meat/score.rs`, `src/council/{types,state_machine}.rs`, `src/cluster/runtime.rs`, `src/bun/{agent,api}.rs`, `src/relish/client.rs` (if apply response shape changes).
Book: ch. 2 "Finding Friends" — "The scheduler finally schedules": desired state vs push RPC (D1 rationale), reconciliation as the universal primitive; H8 post-mortem (a weight table nobody integration-tested).
Progress: tick `L1`+`H8`. Commit: `meat+council: cluster scheduling via raft placements and per-node reconciliation`. ~700 LOC. **Largest sub-stage; expect two review rounds.**

---

### W7 — L2: production deploy orchestration (+ M16, X3)

**Goal:** rolling and blue-green deploys execute cluster-wide with surge/unavailability limits; `relish rollback` actually rolls back.

Tests first:

- `rolling_deploy_respects_max_unavailable` — 3-node cluster, replicas=3, `max_unavailable=1`; during redeploy, poll: never fewer than 2 healthy backends.
- `blue_green_swaps_routing_atomically` — old backends serve until swap; after swap only new.
- `failed_deploy_rolls_back_and_leaks_nothing` (M16) — new image fails health; old instances still serving; no orphan instance rows; deploy history records `Failed`.
- `rollback_endpoint_restores_previous_spec` (X3) — deploy v1, deploy v2, `relish rollback` → v1 spec redeployed (image + spec fields), history records the rollback.

Implementation (per D2):

- Convert `DeployDriver`/`DeployOrchestrator`/`execute_blue_green` to async; fix M16 in the library while there (rollback stops the leaked step instance; `RoutingSwitching` error path → terminal `Failed`; enforce `max_surge`/`max_unavailable` as step gates).
- `ClusterDeployDriver` (new, `src/meat/` or `src/bun/`): `start_instance` → propose a placement for the target node and wait for the reconciler to report it (reuse W6; per-instance granularity via a `pending` placement flag), `await_healthy` → poll target node `/v1/status/{app}/{ns}`, routing ops → the service-map/routing rebuild path, `drain_instance` → `DrainTracker`, `current_placements` → state machine.
- Leader deploy task: when a proposed AppSpec has a `deploy` strategy and existing placements, run the orchestrator instead of blunt re-placement.
- **X3**: store the spec with history — add `spec: Box<AppSpec>` to `DeployHistoryEntry` (`#[serde(default)]` + `Option` if snapshot-serialised); `POST /v1/rollback/{app}` re-proposes the previous successful entry's spec; `relish rollback` calls it and exits non-zero on failure.

Files: `src/meat/{orchestrator,blue_green,deploy_types}.rs`, `src/cluster/runtime.rs`, `src/bun/api.rs`, `src/relish/{commands,client}.rs`.
Book: ch. 7 "Ship It" — "Deploys grow up": async trait conversion, why the driver proposes placements instead of talking to grills.
Progress: tick `L2` and the X3 part. Commit: `meat: production deploy orchestrator with rollback endpoint`. ~600 LOC.

---

### W8 — L3: autoscale loop

Tests first: `autoscaler_scales_up_on_high_metric` (cluster test: seed rollup store with high cpu for an app min=1 max=3 → placements grow to 3); `autoscaler_respects_cooldown_and_hysteresis` (unit, existing tracker tests extended); `gitops_overrides_visible_in_desired_state` (override recorded where W10 will read it).

Implementation: leader-only spawn (leadership watch) of `run_autoscale_loop`; `app_provider` reads `DesiredState.apps` filtered to `autoscale.is_some()`; `metric_provider` queries the leader's rollup store (W4) for the app's average over `evaluation_window`; consumer task receives `AutoscaleDecision` → proposes new Raft entry `RaftRequest::AutoscaleOverride { app_id, replicas }` (`DesiredState.overrides: HashMap<AppId, u32>`, `#[serde(default)]`) → scheduler (W6) reads effective replicas = override ∨ spec, re-places. Overrides list also feeds `execute_sync`'s `autoscale_overrides` argument in W10.
Files: `src/cluster/runtime.rs`, `src/council/{types,state_machine}.rs`, `src/meat/autoscaler.rs` (only if signatures need Arc-ing).
Book: ch. 9 — "The loop that resizes": hysteresis, and why overrides live beside (not inside) the spec.
Progress: tick `L3`. Commit: `meat: spawn the autoscale loop on the leader`. ~250 LOC.

---

### W9 — L4: state reconstruction after leader election

Tests first: `new_leader_learns_state_before_scheduling` (3-node cluster; kill the leader; new leader enters Learning, holds placement changes until threshold, then Active); `missing_app_correction_reschedules_it` (desired app absent from all reports → correction → placements re-proposed); `extra_app_correction_stops_it` (instance running with no desired entry → stopped via reconciler).

Implementation: instantiate `ReconstructionController` in the leader task (W6's) from `[reconstruction]` config (now read — dead-config cleared); on leadership gain call `on_leader_elected(alive_count)`; feed each `aggregated_rx` update through `on_report_received(aggregated, desired, alive)`; periodic `check_timeout`; gate the scheduler on phase == Active; consume `Correction`s: MissingApp → re-run scheduling for the app; ExtraApp → remove placement (reconciler stops it); UnknownNode → exclude from `ClusterStateCache` until it reports.
Files: `src/cluster/runtime.rs`, `src/reconstruction/controller.rs` (only if handle shapes need adjusting).
Book: ch. 2 — "Amnesia and recovery": why a fresh leader must listen before it acts.
Progress: tick `L4`. Commit: `reconstruction: wire the learning period into leader startup`. ~250 LOC.

---

### W10 — L13: GitOps sync loop (+ H12)

Tests first: `is_key_trusted_rejects_unlisted_key` (H12, unit — currently impossible to fail); `sync_loop_applies_repo_changes` (local bare git repo fixture with app TOML; leader polls; app appears in desired state); `webhook_triggers_immediate_sync` (`POST /v1/gitops/webhook` → sync happens before the poll interval); `unsigned_commit_rejected_when_verification_required`.

Implementation: fix H12 first (`src/lettuce/verify.rs:78-87` — the fall-through returns `false`; empty `trusted_keys` with verification enabled = reject). Leader-only sync task: `[gitops]` config (now read — dead-config cleared) → `GitRepo` operations in `spawn_blocking` (git shells out; never on the async runtime; pass `--` separators per the low-severity arg-injection note) → `execute_sync(repo, cfg, desired.apps, desired.overrides, last_sha)` → apply `SyncOutcome` by proposing `AppSpec`/removal entries through the W6 path → record `last_applied_sha` via the existing `GitOpsSyncUpdate` Raft entry. Webhook: create the mpsc in `bun.rs`, hand `tx` to `ApiState.gitops_webhook_tx` (kills the 503), `rx` nudges the sync task.
Files: `src/lettuce/{verify,sync,git}.rs`, `src/cluster/runtime.rs`, `src/bin/bun.rs`, `src/bun/api.rs`.
Book: ch. 7 — "Git as the source of truth": leader-only sync, commit signing as supply-chain defence (H12 lesson: a security check that always passes is worse than none).
Progress: tick `L13`+`H12`. Commit: `lettuce: leader gitops sync loop, webhook, real key trust`. ~400 LOC.

---

### W11 — L14/L15: real fault injection + chaos partitions

Tests first: `fault_injection_rejected_when_quorum_at_risk` (safety rail, binary-driven: 3-node cluster, request partition of 2 council members → 403 with reason); `kill_fault_actually_kills_the_instance` (TestHarness + ProcessGrill app; inject kill; PID gone; supervisor restarts it; fault summary reflects it); `pause_and_resume_signals_delivered` (SIGSTOP/SIGCONT observed via /proc state or process probe); `partition_isolates_a_node_for_real` (L15: 3-node cluster; partition C; A/B mark C Suspect/Dead within the SWIM timeout; heal; C Alive again); `unsupported_fault_types_error_honestly` (delay/bandwidth without ebpf → clear error, nothing recorded as active). Also FIX the misleading chaos test flagged by the review ("worker isolation" must now assert a real fault).

Implementation (per D7):

- `InjectFault`/`InjectPartition` handlers: build `SafetyContext` (council size from raft metrics, alive nodes from membership, active faults from registry, replica counts from supervisor/desired state) → `evaluate_safety` → reject unless override allows.
- Injection by type: Kill → `kill_process(pid)`; Pause/resume → `pause_process`/`resume_process` (PIDs from supervisor instances); CPU → burn tasks via `spawn_blocking` sized by `CpuBurn`; Memory/disk-IO → cgroup writers (Linux-gated, honest error elsewhere); delay/drop/dns/bandwidth → honest "requires ebpf" error until W12.
- Partition: `ClusterHandle` gains the gossip transport's and Raft network's blocklist handles (`transport.blocklist()`, `council/network.rs:334`); `InjectPartition` resolves target nodes → SocketAddrs from membership and populates both; heal/expiry clears them. Fault registry entries now describe *applied* state.
Files: `src/bun/agent.rs`, `src/cluster/runtime.rs` (handle plumbing), `src/smoker/*` (only adapters), `tests/chaos*.rs`.
Book: ch. 8 "Breaking Things on Purpose" — "Now it actually breaks": the gap between recording a fault and injecting one; safety rails as the difference between chaos engineering and vandalism.
Progress: tick `L14/L15` + the "Throughout" misleading-test item. Commit: `smoker: real fault injection with safety rails; chaos partitions via transport blocklists`. ~500 LOC.

---

### W12 — L8 + L16: eBPF in production + egress allowlists (Linux/Lima)

Tests first (Linux-gated `#[cfg(all(target_os = "linux", feature = "ebpf"))]` + Lima):

- `ebpf_programs_load_and_attach_on_boot` (in-VM: bun starts with `[ebpf]` config, `is_attached()` true, service resolution rewrites connects).
- `egress_denied_by_default_allowed_when_listed` (in-VM: app with `[egress] allow=["1.1.1.1:443"]` connects there, blocked elsewhere).
- `delay_fault_measurably_delays_traffic` (in-VM: inject delay; request latency rises; clear; recovers).
- Cross-platform: `default_build_stays_green_without_ebpf` (CI already covers — assert stub paths log the D5 warning).

Implementation:

- Build pipeline: Makefile target compiling the `.bpf.o` objects (clang/bpf toolchain in the Lima build VM), installed to `{data_dir}/bpf/`; `[ebpf] enabled/program_dir` in `NodeConfig`; document in docs/README.
- `bun.rs`: under the feature + config, `OnionEbpf::load(program_dir, cgroup_root)` at startup; hand `Arc<Mutex<OnionEbpf>>` to the agent (replaces the permanent `None` at `agent.rs:332,366`).
- Make `write_fault_bpf_entry`/`delete_fault_bpf_entry` real: translate `FaultRule` → map writes via `src/smoker/bpf_maps.rs`; delay/drop/dns/bandwidth faults switch from W11's honest error to actual injection when attached.
- Egress (L16): supervisor passes `AppSpec.egress` through (kill the `egress: None` at `supervisor.rs:~488`); at instance start, `resolve_egress_entries` (in `spawn_blocking` — DNS), map cgroup id, `egress_to_bpf_entries` → map writes; spawn `re_resolve_egress_async` refresh loop; cleanup on stop. Non-eBPF builds: warning per D5.
- Extend `relish dev test` with the three in-VM tests; document that `make ci` does NOT cover them (run via Lima before ticking).
Files: `Makefile`, `src/config/node.rs`, `src/bin/bun.rs`, `src/bun/{agent,supervisor}.rs`, `src/smoker/bpf_maps.rs`, `src/onion/ebpf/loader.rs`, Lima test scripts.
Book: ch. 3 (eBPF loading story) + ch. 10 "Locking It Down" (egress: default-deny philosophy, DNS re-resolution).
Progress: tick the `L8` half of L8/L9 and `L16`. Commit: `onion+sesame: load eBPF in production, enforce egress allowlists`. ~500 LOC + build scripts.

---

## 3. Cross-cutting rules

- `make ci` before every commit; ask the user before each commit; never amend.
- Tests first, named as behaviour sentences; integration tests drive `BunAgent` + `api::router()` or the in-process cluster runtime — **never call the library entry point directly from the test when a binary path exists**.
- No `unwrap`/`expect` outside tests; thiserror per subsystem; tokio-only sync; `CancellationToken` for every spawned loop; `spawn_blocking` for git/tar/cgroup/nft/DNS-resolution/builder work.
- **Dead-config ledger** (remove-or-wire; each cleared in the named sub-stage): `[resources]` → W4 · `[node] labels` → W6 · `[reconstruction]` → W9 · `[gitops]` → W10 · `[images]` → W5 · `[metrics]` → W4 · `[ebpf]` (new) → W12. Remaining dead after Stage 4 (document, don't delete silently): `[process_workloads]` (M23), `[storage] volumes` (M21), `[logs] max_file_size_mb` — out of Stage 4 scope, note in progress.md.
- Clear the corresponding `[lib-only]` tags in progress.md phase sections as each subsystem is genuinely wired (the "Throughout" checklist item).
- progress.md, docs/README.md and top-level README.md updated at each sub-stage that changes user-visible behaviour (new config sections, endpoints, CLI semantics), full refresh at the end.

## 4. Risks and pitfalls (explicit, for the executor)

1. **Raft snapshot compatibility.** Every `DesiredState` field added (placements, overrides, history specs) MUST be `#[serde(default)]`; add a state-machine test deserialising a pre-change snapshot fixture. Never reorder existing `RaftRequest` variants.
2. **Multi-node test hygiene.** Copy the `tests/cluster_gossip.rs` pattern exactly: unique temp dirs, per-node port blocks from an ephemeral base, `multi_thread` flavour, poll-until-with-timeout helpers. Never fixed `sleep`s; never `start_paused` with real sockets (known project pitfall).
3. **Don't break single-node mode.** Every wiring is gated on `--cluster` and/or its config section; the existing integration suite is the regression net — run it per sub-stage, not just the new tests.
4. **Leadership churn.** Leader-only tasks (scheduler, autoscaler, replication, GitOps, reconstruction) must watch `raft_metrics_rx` and stop cleanly on leadership loss — pattern: one `select!` loop per task on `{leadership watch, work timer, cancel}`. No task may assume it stays leader across an await.
5. **Reconciler restraint.** The placement reconciler must be idempotent and must NOT fight the deploy orchestrator (W7): orchestrated apps carry a "managed deploy in progress" marker in desired state that pauses blunt reconciliation for that app.
6. **Blocking the runtime.** git, tar, builder execution, cgroup writes, blocking DNS resolution → `spawn_blocking`, no exceptions. clippy will not catch this; review each new call site.
7. **Feature-gate hygiene.** Default build (no `ebpf`) must stay green at every commit — stubs behind `#[cfg]` with the same signatures; CI runs the default matrix, Lima runs the rest. Never let an `ebpf`-only type leak into a shared signature.
8. **Auth on new endpoints.** `/v1/placements/*`, `/v1/rollback/*`, `/v1/build` go in the protected router block; placements additionally require the service-token role (mirror how Stage 3b routes check roles). Add one negative test per endpoint.
9. **Port and holder identity consistency.** Holder ids in the Pickle catalog are raft ids (u64); node ids in placements are `NodeId` — never conflate; write the conversion in ONE place.
10. **X5 is a behaviour break.** Scripts relying on exit-0 dry-run fallback will fail; the commit message and README must say so.
11. **Blocklist symmetry.** A partition must block BOTH directions and BOTH transports (gossip UDP + Raft TCP) or SWIM half-detects and tests flake; heal must clear both.
12. **Lima tests are manual-ish.** `relish dev test` runs are not in `make ci`; W12 (and the in-VM parts of W5) are not "done" until the Lima suite passes — record the run output in the PR/commit description.

## 5. Definition of done for Stage 4

- All 14 Stage 4 checklist items in progress.md ticked, with X6 annotated "→ tui-plan-2026-07.md" and X2/X7 already closed by Stage 3.
- Review IDs closed: L1–L4, L6–L11, L13–L16, H8, H11, H12, M2, M8, M16, M17, X1, X3, X4, X5.
- All `[lib-only]` tags cleared from progress.md phase sections (or re-tagged with an honest remaining-gap note).
- Dead-config ledger resolved per §3; remaining dead sections explicitly documented.
- Full suite green (`make ci`) on macOS default build; Lima suite green for W12; misleading tests from the review replaced with behaviour-asserting ones.
- Book chapters 2, 3, 5, 7, 8, 9, 10, 11 updated with their Stage 4 sections; READMEs refreshed.

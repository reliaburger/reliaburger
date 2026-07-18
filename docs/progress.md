# Implementation Progress

Single source of truth for what's done and what's next. Check off an item only when it compiles, passes tests, and is committed. See [roadmap.md](roadmap.md) for full details on each phase.

> **Review note (July 2026):** a full verification pass ([2026-07-02-review-codebase.md](plans/2026-07-02-review-codebase.md))
> found that many checked items are **library-only** — implemented and unit-tested, but never
> wired into the `bun`/`relish` binaries. Those are tagged **`[lib-only]`** below with their
> finding ID (e.g. `L7`, `C5`). `[x]` still means "code exists + tests pass"; `[lib-only]` means
> "not reachable from the running binary". Critical bugs in *wired* paths are tagged with their
> ID too (e.g. `C4`). See the review doc for `file:line` and the staged fix plan.

---

## Phase 1: Foundation

- [x] Cargo workspace setup (binary `bun`, library `reliaburger`, test fixtures)
- [x] TOML config parsing (App, Job, Secret, ConfigFile, Volume, Permission, Namespace)
- [x] Grill container runtime interface (runc/Apple/process backends, driven directly — no containerd; OCI extraction, ports, cgroups)
- [x] Bun agent core (process supervisor, health checks, restart logic, GPU detection) — **restart re-drive broken for apps on all runtimes (`H1`); ~~GPU detector is a stub~~ (12b.6: real `NvidiaGpuDetector`; `gpu_enabled` now effective, GPU-request refused when unbacked)**
- [x] Relish CLI skeleton (`apply`, `status`, `logs`, `exec`, `inspect`)
- [x] ProcessGrill (cross-platform process-based runtime)
- [x] RuncGrill (Linux-only, calls runc CLI)
- [x] AppleContainerGrill (macOS-only, calls Apple container CLI)
- [x] HTTP health probing (reqwest-based probe with timeout)
- [x] Bun agent event loop (tokio::select, command channels, lifecycle driver)
- [x] Bun local HTTP API (axum on localhost:9117)
- [x] Relish HTTP client (live agent calls with dry-run fallback)
- [x] Integration tests (lifecycle, health checks, restart, CLI)
- [x] `command` field on AppSpec (run custom processes via ProcessGrill)
- [x] TestApp standalone binary (`cargo run --bin testapp`)
- [x] Job execution (deploy, run-to-completion, retry with backoff, failure)
- [x] Init container execution (sequential run, failure prevents main start) — **no timeout: a non-exiting init wedges the agent (`H3`)**
- [x] Restart re-drive (health check and job restarts re-start instances) — **`H1` only ProcessGrill jobs work; apps wedge in `Preparing`, old process leaks**
- [x] Exit code tracking on Grill trait (ProcessGrill, MockGrill) — **`H13` runc/apple return `None`; successful jobs there get retried then Failed**
- [x] Example configs (minimal-app, restarts, job-success, job-failure, init-container, volumes, multi-app, full-featured)
- [x] OCI image pulling from Docker Hub (oci-distribution, content-addressed cache, layer unpacking with whiteouts) — **`C1` CRITICAL: whiteout path traversal deletes host files outside rootfs**
- [x] Rootless runc (user namespaces, UID/GID mapping, rootless cgroups v2, no-sudo containers) — **`M22` resource limits ~~silently dropped~~ (12b.6: a rootless node now *refuses* a limit-requiring workload rather than dropping the limit); slirp4netns still unwired → empty netns, no connectivity (deferred to the runc-create seam)**
- [x] Streaming apply progress via SSE (real-time deploy feedback instead of blocking response)
- [x] HostPath-style volumes (dual-mode: explicit source for hostPath, managed for auto-provisioned storage) — ~~`M21` managed mode half-wired~~ **fixed in Phase 12 E0**: the agent creates managed volume dirs (and loop mounts) before the OCI spec's bind mounts reference them; `[storage] volumes` config wired; volumes never deleted on Stop (reconciler rebalances would destroy data)
- [x] Relish init command (scaffold reliaburger.toml and app.toml from defaults)
- [x] Log tailing (`--tail N`) and streaming (`--follow`/`-f`) — **`H3` `follow_logs` blocks the whole agent event loop; runc has no `logs`/`follow_logs` (empty)**
- [x] Relish exec command (run commands in running instances) — **`H13` unimplemented on runc (always errors)**
- [x] All Phase 1 tests green (321 tests)

## Phase 2: Cluster Formation

- [x] Shared types: `NodeId`, `AppId`, `Resources`, `NodeCapacity`, `SchedulingDecision` (`src/meat/types.rs`)
- [x] Mustard state machine: NodeState enum, incarnation conflicts, membership table, piggyback dissemination — **`H4`–`H7` SWIM bugs: wrong incarnation on suspect, no Dead refutation, suspicion timed from `last_ack`, watch fires on count only**
- [x] Mustard transport and protocol: `MustardTransport` trait, SWIM probe cycle, gossip convergence tests
- [x] Indirect probe (PING-REQ) ACK routing, proptest for conflict resolution, broadcast count lambda=3
- [x] Dead node reap timer (cleanup_timeout=60s), graceful leave protocol (Left state broadcast on shutdown)
- [x] Raft integration (openraft): storage, network, and state machine adapters; leader election and log replication — **`C3` CRITICAL: log/vote in-memory only → split-brain on restart**
- [x] Council selection: stability/zone diversity scoring, deterministic tiebreak, size bounds 3–7 — **`[lib-only]` `L5` never called; runtime uses naive sort-by-id+truncate (can demote leader). Wire format carries no resources/labels, so scoring inputs are empty anyway**
- [x] Reporting tree: `StateReport` to council member every 5s, consistent hash assignment, `watch` channel — **`L6` flat-star only; leader's aggregated view read by nothing; StateReport resource usage all zeroed**
- [x] State reconstruction: learning period after leader election, 95% threshold or 15s timeout, diff/correction — **`[lib-only]` `L4` never invoked; no `Correction` consumer**
- [x] Meat scheduler: Filter → Score → Select → Commit pipeline, bin-packing, labels, daemon mode, quotas — **`[lib-only]` `L1` nothing in the binary schedules onto a remote node (deploys are always local); `H8` all replicas land on one node; quotas never integrated**
- [x] Scheduler image locality scoring — prefers nodes with cached images — **`[lib-only]` (part of `L1`)**
- [x] Scheduler stability scoring — prefers nodes with longer uptime — **`[lib-only]` (part of `L1`)**
- [x] Agent integration: wire cluster subsystems into `BunAgent`, extend config, cluster API endpoints
- [x] CLI extensions: `relish nodes`, `relish council` (stub responses, full pipeline)
- [x] CLI extensions: `relish join`
- [x] Chaos tests: council partition, worker isolation (full council loss deferred to Phase 4/8) — Stage 4 W11 replaced the no-op "worker isolation" test with `chaos_isolated_member_misses_writes_until_healed` (a real router partition), and added `partition_isolates_a_node_for_real` driving the actual runtime's transport blocklists through the HTTP API
- [x] Book chapter + docs: `02-finding-friends.md`, update README and progress (588 tests)

### Cluster runtime wiring (the subsystems above were library-only until here)

Phases 2–11 built the cluster subsystems but the `bun` binary always ran single-node (`BunAgent::new`, never `with_cluster`); they were exercised only by in-memory harnesses. Now wired into the binary behind `bun --cluster`:

- [x] Gossip: `cluster::runtime` binds a real `UdpMustardTransport`, joins by address (no phantom members), membership converges (`tests/cluster_gossip.rs`)
- [x] Raft council: real `serve_raft_rpc` over TCP, bootstrap, a leader-only selection loop grows the council from gossip membership (per-peer Raft address derived from each node's gossip port + offset)
- [x] Reporting tree (flat-star MVP): every node reports state to the leader; the leader's aggregator collects the cluster view
- [x] `relish dev create` forms a real, verified cluster: launches `bun --cluster` under sudo (root for runc/netns and the log file), advertises each VM's inter-VM IP over Lima's `user-v2` network, and builds the binaries from the current tree in the build VM under `sudo -E` (no GitHub download; `--bun`/`--relish` skip the build). Verified end-to-end on a cold 3-node Lima cluster: the council elects a leader and `relish nodes`/`council` show all three members. The Phase 2.1 dev-cluster commands were single-node until here.
- [x] Perimeter firewall fixes (both blocked any multi-node cluster, not just dev): accept loopback before the port drops — local `relish` talks to `127.0.0.1:9117` and its SYN was being dropped, which looked like an API hang; and flush the table before re-applying — `nft -f` appends, so reconciling on every membership change stacked stale boot-time rules ahead of the fresh allow-members rule
- [x] `relish nodes` shows real council membership + leader, derived from the Raft metrics (the gossip-level `is_council`/`is_leader` flags are never set by the runtime), falling back to gossip when Raft isn't wired
- Canonical consistent-hash reporting tree (workers → council → leader, multi-level aggregation) — follow-up
- Durable Raft log + `wrapping_ikm` loading — follow-up

## Phase 2.1: Dev Cluster

- [x] Lima wrapper: VM lifecycle (create, start, stop, delete), platform detection, YAML generation
- [x] Node configuration: generate node.toml per VM with join addresses and cluster ports
- [x] CLI: `relish dev create`, `status`, `shell`, `stop`, `start`, `destroy`
- [x] GitHub release pipeline: cross-compile bun/relish for linux-aarch64 and linux-x86_64
- [x] Docs: whitepaper dev cluster section, README, book getting-started guide

## Phase 3: Networking

- [x] Per-container network namespaces (veth pairs, port mapping)
  - Switch port mapping from individual nftables rules to nftables maps for O(1) lookup at scale — Phase 12
- [x] Onion eBPF service discovery (DNS interception, connect() rewrite, service map)
  - [x] Userspace ServiceMap, VirtualIP allocation, `relish resolve` command
  - [x] Agent lifecycle wiring (deploy/health/stop → service map)
  - [x] eBPF C programs and Rust loader scaffolding (Linux only) — **`[lib-only]` `L8` `onion_ebpf` is permanently `None`; `ebpf` feature not in default build; `.bpf.o` never installed**
  - [x] Wire aya loader for connect rewrite (cgroup/connect4) — **`[lib-only]` `L8` only loaded inside the feature-gated test binary**
  - [x] Userspace DNS responder for `.internal` queries (replaces infeasible in-kernel DNS synthesis) — ~~`[lib-only]` `L9` `run_dns_responder` never spawned~~ (now spawned from `src/bin/bun.rs` when `[dns]` is enabled, and containers get their `resolv.conf` nameserver pointed at it)
  - [x] `relish dev test` runs Linux + eBPF tests from macOS via Lima
  - [x] eBPF integration tests (load/attach, map read/write, connect rewrite, DNS responder)
- [x] Wrapper ingress proxy (host/path routing, load balancing, draining, rate limiting) — **`[lib-only]` `L7` the proxy never runs: `run_proxy` has no caller, no listener is bound**
  - [x] Routing table (host/path → backend pool, longest prefix match, round-robin LB) — routing table itself is wired (answers `relish routes`)
  - [x] HTTP reverse proxy on dedicated tokio runtime (DDoS isolation, connection limit) — **`[lib-only]` `L7`; the "dedicated tokio runtime" does not exist**
  - [x] Per-client-IP token bucket rate limiting (429 + Retry-After) — **`[lib-only]` `L7` never instantiated**
  - [x] Connection draining protocol (zero-downtime deploys) — **`[lib-only]` `L7`**
  - [x] Agent wiring (routing table rebuilds on deploy/stop/health, `relish routes` command) — **`H2` redeploy leaves 0 backends; routing not rebuilt on health transitions (goes stale)**
  - [x] TLS termination with self-signed certs (rcgen + rustls, Phase 4 adds ACME + Sesame) — **`[lib-only]` `L7` no HTTPS listener**
  - WebSocket upgrade proxying — Phase 9
- [x] nftables perimeter firewall (cluster boundary rules, management access)
  - [x] Ruleset generation (targeted blocking of Reliaburger ports, policy accept)
  - [x] `apply_ruleset()` via `nft -f` (Linux), no-op on macOS
  - [x] Wire into agent (reconcile on gossip membership changes, auto-disabled in rootless mode) — **`C4` CRITICAL: reconcile flushes the shared nft table, wiping container DNAT; `M18` triggers on node count not membership, never applies in standalone mode, drops TCP only (gossip is UDP)**
- [x] All Phase 3 tests green (702 tests)

## Phase 4: Security

> **Phase 4/10 security caveat (`C5`):** the crypto primitives are implemented and sound, but
> almost nothing is **enforced** at runtime — see `L16`–`L18`, `C5`. The `bun`/`relish` binaries
> run with no API auth, no mTLS, no gossip auth, no at-rest encryption, no secret decryption, and
> no signature/CRL verification.

- [x] Sesame CA hierarchy (Root, Node, Workload, Ingress CAs — ECDSA P-256, HKDF key wrapping) — primitives real; `wrapping_ikm` hardcoded `None` at runtime so CA-key ops dead-end (`C5`)
- [x] Node mTLS (join tokens, certificate issuance, mTLS config builders, gossip HMAC) — **`[lib-only]` `C5` builders never used; Raft RPC is plaintext; gossip HMAC never signed/verified**
- Workload identity — deferred to Phase 10
- [x] API authentication (tokens, Argon2id hashing, roles: Admin/Deployer/ReadOnly, axum middleware) — **`[lib-only]` `C5` middleware never attached (and fails OPEN on empty token store)**
- [x] Secret encryption (age keypairs, `ENC[AGE:...]` decryption at container startup, namespace-scoped keys) — **`C5` decryption never invoked: containers get the literal `ENC[AGE:...]`**
- [x] eBPF firewall rules (`allow_from` resolution, cgroup-to-namespace mapping, BPF map wiring) — **`[lib-only]` no callers; nothing populates the BPF maps**
- [x] Raft log encryption at rest (AES-256-GCM, HKDF from node cert private key) — **`[lib-only]` dead code; the log is in-memory so there is nothing at rest to encrypt (`C3`)**
- [x] `relish init` generates full PKI + join token; `relish token create` — **`X2` `token create` is local-only (never persisted to Raft, can't be validated/listed); `X7` `secret pubkey` reads the wrong filename**
- `relish token list/revoke` — moved to Phase 10 (requires SecurityState in Raft)
- Join token validation in agent — moved to Phase 10 (requires SecurityState in Raft)
- [x] `relish secret pubkey` and `relish secret encrypt` CLI commands
- `relish secret rotate` — moved to Phase 10 (requires SecurityState in Raft)
- [x] Book chapter 4: "Trust No One"
- [x] All Phase 4 tests green (795 tests)

## Phase 5: Storage & Registry

- [x] Pickle types and Raft state extensions (Digest, ImageManifest, ManifestCatalog, Raft commands)
- [x] Pickle blob store (content-addressed, upload sessions, digest verification, atomic rename) — **`C2` CRITICAL: `upload_id` path traversal (arbitrary file append/exfil) on the unauthenticated `0.0.0.0` registry**
- [x] OCI Distribution API (push/pull: blob upload POST/PATCH/PUT, manifest PUT/GET, tag list)
- [x] Synchronous replication on push (peer selection, layer transfer via OCI API, mTLS) — **`[lib-only]` `L10` no callers; push stores locally with hardcoded holders; no mTLS**
- [x] Peer pull (fetch missing layers from peers via Raft layer_locations) — **`[lib-only]` `L10` catalog is `default()` per boot, not Raft-replicated (metadata lost on restart)**
- [x] Garbage collection (sole-copy protection, active reference safety, retention window, GcReport) — **`[lib-only]` `L10` `gc_sweep` never scheduled; `M2` TOCTOU can lose all copies**
- [x] Volume size enforcement (loop mount on Linux, soft warning on macOS)
- [x] `relish images` CLI command, `[images]` config section
- Pull-through cache (Phase 12), P2P downloads (Phase 12), image signing (Phase 10), volume snapshots (Phase 12)
- [x] Book chapter 5: "Where the Images Live"
- [x] All Phase 5 tests green (867 tests)

## Phase 6: Observability

- [x] Mayo TSDB (Arrow RecordBatches + DataFusion SQL + Parquet persistence via object_store) — **`H9` Parquet write-only & clobbered on restart, never reloaded, in-memory batches unbounded; `M1` SQL injection via `name=` param breaks tenant isolation**
- [x] System metrics collector (CPU, memory, disk, network via sysinfo)
- [x] Prometheus scraping (prometheus-parse crate, auto /metrics endpoint) — **`[lib-only]` `L11` `scrape_endpoint` never called**
- [x] Metrics API (`/v1/metrics`, `/v1/metrics/summary`, `/v1/metrics/keys`)
- [x] Alert evaluation (5 default rules, Inactive→Pending→Firing state machine)
- [x] Ketchup log collection (append-only files, sparse timestamp index, JSON detection) — **`[lib-only]` `L12` `KetchupStore` created then dropped; sparse index written but never read; `H10` container stdout/stderr never reaches any log store**
- [x] Ketchup queries (grep, tail, time range, JSON field filter) — **`M4` dedup only removes adjacent duplicates**
- [x] Brioche dashboard (server-rendered HTML, dark theme, auto-refresh)
- [x] `relish top` command, `relish logs --grep/--since/--json-field`
- [x] Config: `[metrics]` and `[logs]` sections with object_store_url
- Hierarchical aggregation, full Brioche UI, alert webhooks, PromQL — deferred to Phase 11
- [x] Cross-node log queries (fan-out to nodes, merge by timestamp, dedup)
- [x] Agent wiring (Mayo collection task, Ketchup store, AlertEvaluator, `/v1/alerts`)
- [x] `make observability-demo` for local testing
- [x] Book chapter 6: "Watching Everything"
- [x] LogStore: SQL over logs via Arrow/DataFusion/Parquet (same engine as metrics)
- [x] `/v1/logs/sql` endpoint for SQL log queries
- [x] All Phase 6 tests green (991 tests)

## Phase 7: GitOps & Deployments

- [x] Deploy state machine (9 phases: Pending → Rolling → Completed/RolledBack/Halted/Failed)
- [x] Rolling deploy orchestrator (DeployDriver trait, per-step health-gated replacement) — **`[lib-only]` `L2` orchestrator/`DeployDriver` only in tests; the wired path is the agent's inline redeploy (`H2`/`H3` bugs)**
- [x] Automatic rollback (revert upgraded instances on health failure) — **`M16` rollback leaks the failed step's new instance**
- [x] Dependency ordering (`run_before` jobs complete before rolling)
- [x] Deploy Raft persistence (active deploys + history capped at 50 per app) — **`[lib-only]` state-machine support only; the binary never writes deploy requests; history is a local `Vec` and only records on redeploy (empty until the 2nd deploy)**
- [x] CLI: `relish deploy`, `relish history`, `relish rollback`, `relish lint` — **`X3` `rollback` calls no endpoint (prints advice); `X5` dry-run fallback makes `apply`/`deploy` exit 0 when the agent is down**
- [x] API: `/v1/deploys/active`, `/v1/deploys/history/{app}` — **`/v1/deploys/active` hardcoded empty**
- [x] `make deploy-demo` for local testing
- [x] Book chapter 7: "Ship It"
- Autoscaling, Lettuce GitOps, blue-green, K8s migration — see Phase 9
- [x] All Phase 7 tests green (1039 tests)

## Phase 8: Advanced

- [x] Smoker fault injection (safety rails, registry, process/network fault plumbing, scripted scenarios and chaos tests) — **post-Phase-12 audit: Kill/Pause/Resume and eBPF faults have live paths, but memory, disk, drain and node-kill can still report success without an effect; CPU stress targets Bun rather than the workload and cleanup is incomplete. Phase 12b owns the correction.**
- [x] Network security (egress allowlists, eBPF enforcement in connect hook, namespace isolation) — **`[lib-only]` `L16` egress resolvers never called; supervisor sets `egress: None`**
- [x] Process workloads (exec/script apps and jobs, binary allowlist, ProcessManager, OCI spec wiring, validation) — **`M23` allowlist + `mount_isolation` never enforced (`with_process_config` no callers)**
- [x] High-throughput batch scheduling (greedy bin-packing `schedule_batch`, `BatchTracker`, 100K jobs in <1s)
- [x] Build jobs (BuildSpec config, pickle:// destination parsing, namespace-scoped push, buildah command construction)
- Live agent wiring for batch dispatch and build execution — deferred to Phase 12 (the `/v1/batch` and `/v1/build` endpoints return 501 until then)
- [x] All Phase 8 tests green (1263 tests)

## Phase 9: User Experience

- [x] Blue-green deploy strategy (parallel start, atomic routing swap, orchestrator dispatch) — **`[lib-only]` `L2` orchestrator never constructed outside tests; no production `DeployDriver`; `M16` swap error wedges non-terminal**
- [x] Autoscaling (evaluation logic, hysteresis, cooldown, Mayo query_avg, async task runner, Raft persistence) — **`[lib-only]` `L3` `run_autoscale_loop` never spawned**
- [x] Lettuce GitOps engine (sync loop, git ops, signature verification, diff engine, webhook endpoint, coordinator election, Raft integration, node config) — **`[lib-only]` `L13` nothing spawns the sync loop; webhook endpoint always 503; `[gitops]` config never read; `H12` trusted-key check always passes**
- [x] Kubernetes migration (`relish import`, `relish export` via k8s-openapi, resource correlation, migration reports, optional `kubernetes` feature) — **`M17` import silently drops `command`/`args`, `env.valueFrom`, and namespace**
- [x] `relish compile`, `relish diff`, `relish fmt` — **`H11` `relish fmt` corrupts nested-table configs (writes invalid TOML over the file)**
- [x] WebSocket upgrade proxying in Wrapper ingress (detection, dispatch, close frame, draining) — ~~`[lib-only]` `L7` proxy never runs; handshake stub drops the backend stream~~ (12b.4: the Wrapper proxy runs the 101 upgrade and splices the backend stream; a live splice holds the drain open until it ends — `src/wrapper/proxy.rs`, `draining.rs`, ING2/ING4)
- [x] Book chapter 9: "The Full Package"
- [x] All Phase 9 tests green (1271 tests)

## Phase 10: Advanced Security

- [x] Workload identity (SPIFFE certs, CSR, automatic rotation, OIDC JWTs) — **`L18` CSR path always fails at runtime (`wrapping_ikm: None`, empty SecurityState); rotation task re-CSRs through the same dead path; `M25` workload key world-readable**
- [x] Image signing (keyless via workload identity, cosign-compatible) — **`[lib-only]` `L17` `verify_signature` never called; `require_signatures` never consulted; "signing after build push" not exercisable (build is 501)**
- [x] SecurityState in Raft (prerequisite for the wiring items below) — **`L18` never populated at runtime; `bin/bun.rs` writes nothing; bootstrap file never loaded**
- [x] Wire agent-to-council CSR flow during deploy — **`L18` reachable but always dead-ends at "no wrapping IKM available"**
- [x] Wire automatic keyless signing after build job push — **not exercisable (build execution is 501)**
- [x] `relish sign` CLI command
- [x] `/v1/identity/jwks` and `/v1/identity/sign` API endpoints — **`L18` always 503 (`ApiState.council` is `None`)**
- [x] CRL distribution, egress DNS resolution — **`[lib-only]` `L17` CRL never enforced/served; egress never programmed**
- TPM sealing — deferred to v2 (requires hardware)
- [x] `relish token list/revoke` (SecurityState in Raft) — **`L18` always 503 (`ApiState.council` is `None`, even in `--cluster`)**
- [x] Join token validation in agent (SecurityState in Raft) — **`L18` join dead-ends at "no wrapping IKM available"**
- [x] `relish secret rotate` (dual-key transition window) — **`L18` endpoint always 503**
- [x] Book chapter 10: "Locking It Down"
- [x] All Phase 10 tests green (1448 tests)

## Phase 11: Advanced Observability

- [x] Hierarchical metrics aggregation via council (cluster-wide queries) — **`[lib-only]` `L11` `RollupWorker` never spawned; aggregator gets `rollup_store: None`; `/v1/metrics/cluster` returns "no rollup store configured" forever**
- [x] Full Brioche UI (app/node detail pages, HTMX auto-refresh, uPlot charts) — **partial: nodes table hardcoded empty, node detail hardcodes `alive` and lists all apps, per-app charts permanently empty (no per-process metrics collected)**
- [x] Alert webhooks (Slack, PagerDuty, generic HTTP) — **`M19` generic payload only; Slack/PagerDuty formats never applied (would be rejected). Dispatch loop is wired.**
- [x] Log export to S3/GCS (scheduled Parquet, `relish logs-search` for remote SQL) — **`M20` behaviour is still open: AWS/GCP `object_store` features are enabled, but `ketchup::export` still converts `s3://`/`gs://` to a `PathBuf` and calls `std::fs::copy`; `X8` `logs-export` also races the agent's checkpoint**
- [x] Cross-node log queries via Raft (leader fan-out, merge-sort) — **`[lib-only]` production always takes the single-node path (`ApiState.council`/`membership` are `None`); direct HTTP, not "via Raft"; `M4` dedup flaw**
- [x] Book chapter 11: "Eyes Everywhere"
- [x] All Phase 11 tests green (1595 tests)

## Phase 11b: Review & Tying the Loose Ends

The July 2026 verification pass ([2026-07-02-review-codebase.md](plans/2026-07-02-review-codebase.md)) found that the build
is clean, clippy is silent, and 1590 tests pass — but a large share of the "done" items above
are **library-only** (unit-tested, never wired into the `bun`/`relish` binaries), plus five
critical bugs and a long tail of correctness issues in the wired paths. This phase closes them.
IDs (`C1`, `H2`, `L7`, …) reference the finding table in the review doc. Same tests-first rule:
each wiring item lands with an integration test that drives the **binary**, not the library.

### Stage 0 — Security & data-loss stop-the-bleed

- [x] `C1` Reject `..` components in OCI whiteout unpacking (host-file deletion via malicious layer) — `safe_join` in `grill/image.rs`
- [x] `C2` Sanitise the pickle `upload_id` path segment (arbitrary file append/exfiltration) — `validate_upload_id` in `pickle/store.rs`, 400 in the handlers
- [x] `C4` Give the perimeter firewall its own nft table — perimeter rules moved to `ip reliaburger_fw`, leaving the container `ip reliaburger` table untouched
- [x] `C5(d)` Decrypt `ENC[AGE:...]` secrets in the wired container-startup path — plumbed via `generate_oci_spec_with_decryptor`; fails closed when no key is available (full decryption lights up with Stage 3 `wrapping_ikm`, L18)
- [x] Bind the Pickle registry to loopback / require auth — `[images] registry_bind` defaults to `127.0.0.1`; token auth deferred to Stage 3

### Stage 1 — Correct the wired single-node path

- [x] `H1` Fix restart re-drive: stop the old container before re-create, drive all runtimes (not just ProcessGrill jobs)
- [x] `H2` Rolling redeploy: track new backends/health/host-port so the service isn't left with 0 backends
- [x] `H3` Move `FollowLogs` off the event loop (spawned) + init-container timeout; deploy health-gate now early-exits (full async deploy is a follow-up)
- [x] `H13` Exit-code / `logs` / `follow_logs` / `exec` on RuncGrill (run-and-capture model) and AppleContainerGrill (`exit_code`) — runc behaviour is Linux-verified via `relish dev`
- [x] `H9` Reload Parquet on startup, stop clobbering flush files, bound in-memory batches (Mayo + Ketchup)
- [x] `H10` Pipe container stdout/stderr into the log store (per-instance forwarders → LogRecord channel → LogStore)
- [x] `M9` Crash detection for non-job apps + bounded HealthWait (`HealthWait → Failed` after a startup grace)
- [x] `M10` Release host ports on `remove_app` and redeploy rollback
- [x] `M11` Probe the instance's `container_ip` (loopback fallback) instead of hardcoded `127.0.0.1`
- [x] `M12` Handle SIGTERM; `shutdown_all` escalates SIGTERM → grace → SIGKILL
- [x] `M13` runc `delete --force` + netns/veth teardown on kill and natural exit (in the `H13` commit)

### Stage 2 — Cluster safety

- [x] `C3` Durable Raft log + vote + state-machine snapshot (redb); bootstrap only when the store is fresh; re-seed gossip from the restored membership on restart. **Closes the last of the five review criticals.** Lima-verified: killing + restarting the bootstrap node no longer forms a second leader — it restores its durable `{1,2,3}` membership and rejoins as a follower (all nodes agree on one leader).
- [x] `H4` SWIM: disseminate Suspect with the target's incarnation (not the prober's)
- [x] `H5` SWIM: refute being declared `Dead`, not just `Suspect`
- [x] `H6` SWIM: time suspicion from `state_changed`, not `last_ack`
- [x] `H7` SWIM: publish the membership watch on content change, not just member count
- [x] `M14` Return the allocated cert serial via `CouncilResponse::SerialAllocated` (fixes the read-back race)
- [x] `M15` Council reconciler can't wedge — non-blocking membership ops with timeouts + logged errors
- [x] `L5` Leader-safe, zone/age-aware council selection — reconciler retains current voters (never demotes the leader) and grows via `select_council_candidates`. **Live cluster-formation behaviour needs Lima verification**; resource-aware filtering awaits the reporting tree carrying real usage.

### Stage 3a — Load the security foundation (wire, don't enforce)

- [x] `[security]` config section (`master_key_path`, `bootstrap_path`) + `sesame::bootstrap` loaders (hex master key + JSON SecurityState, fail-closed on world-readable perms)
- [x] `L18` Load `wrapping_ikm` from the master key and seed the bootstrap `SecurityState` into Raft once (fresh bootstrap only; durable Raft makes it idempotent) — the already-wired CA/OIDC/secret-decryption machinery can now unwrap keys
- [x] `L18` Populate `ApiState.council` + a token store — `/v1/identity/jwks` and `/v1/token/list` light up (were 503); other `None` fields (rollup/membership/gitops) are Stage 4
- [x] `X2` Persist `relish token create` to Raft (server-side mint + `CreateApiToken`, plaintext shown once; unreachable agent is a hard error)
- [x] `X7` `relish secret pubkey` reads the real `*-security-bootstrap.json` and prints the cluster age key
- [x] `relish dev create` distributes the master key (every node) + security bootstrap (node 1) into `/etc/reliaburger` (0600) so a dev cluster boots with a real IKM + seeded SecurityState — the Stage 3a verification vehicle

### Stage 3b (auth) — Enforce API authentication

- [x] `C5(a)` Attach `auth_middleware` via a split router (public: `/`, health, `/ui/*`, JWKS; protected: everything else). Bootstrap window kept but keyed on real user tokens only, and documented
- [x] Derived side-channel service token (HKDF of the master key) so bun's own cross-node fan-out authenticates as a `__system` principal without tripping the bootstrap window
- [x] Live-refresh the token store from Raft (5 s) so a token created via `relish token create` engages enforcement without a restart
- [x] Per-route role authorisation (`authorize`): token/secret/identity-sign → Admin; apply/stop/exec/chaos/fault → Deployer; reads open to any valid token
- [x] Relish sends a Bearer token (`--token` flag > `RELIABURGER_TOKEN`) on every request

### Stage 3b (transport) — Enforce transport security

- [x] `C5(c)` Sign + verify the gossip HMAC — key derived from the master secret (every node has it, nothing new distributed); UDP transport signs on send, drops unverified datagrams on recv
- [x] `L17` (signatures) Enforce image-signature **verification** at deploy time — `verify_image_signature` actually verifies the bytes against the cluster root CA (not just presence); gated on the node trust policy, refuses unsigned/invalid Pickle images. Enforced locally in `deploy` (no central scheduler exists yet — that's Stage 4 L1)
The Stage 5 work (PR #77, merged) closed most of this: node identity is persisted and
delivered to joiners, and **mTLS runs on the Raft RPC, reporting and agent-API listeners**
with a Node-CA-pinned client verifier, a Raft-refreshed CRL, keyless-signature CRL checks
and read-only Brioche session auth. The remaining `C5(b)`/PKI items — Pickle registry
auth/TLS, API scope enforcement + bootstrap lockdown, atomic CSR-based join and
expected-peer binding — live in **Phase 12b** below (Themes "API authorisation and
Brioche", "Node PKI, join and mTLS", "Pickle storage and replication durability").

### Stage 4 — Wire the remaining library-only subsystems (one at a time, binary-driven test each)

Implementation plan: [docs/plans/2026-07-07-plan-wiring.md](plans/2026-07-07-plan-wiring.md)

- [x] `L1` Scheduler → placement → remote dispatch: `relish apply` under `--cluster` commits `AppSpec`s to Raft (followers forward to the leader); a leader-only scheduler places replicas and commits `SchedulingDecision`s; every node polls `/v1/placements/{node}` and reconciles its instances (idempotent). `H8` fixed (spread weight 60 > bin-pack 50; test now asserts distinct nodes). Flushed out a latent bug: durable Raft log + council TCP RPC used bincode, which can't drive the config types' `deserialize_any` — both switched to self-describing JSON (matching the snapshot). Binary-driven test in `tests/placement.rs`
- [x] `L2` / `M16` / `X3` — `M16`: orchestrator no longer leaks a failed step's own half-started instance (regression test asserts it's stopped). `X3`: `relish rollback` actually rolls back — deploy history now carries the full `AppSpec` (every path records it, including the first deploy), `POST /v1/rollback/{app}/{ns}` redeploys the previous successful spec via the apply path (Raft in cluster mode). Note: cluster-wide *staged* rollout (max_unavailable gating across nodes) rides on the W6 desired-state reconciler and the per-node rolling redeploy; the imperative `DeployOrchestrator` stays library-side (correct + unit-tested) rather than duplicated as a parallel cluster driver
- [x] `L3` Autoscale loop wired: leader-only task drives the tested pure functions (`evaluate`/`AutoscaleTracker`/`AutoscaleConfig::from_spec` — the library's sync `app_provider` closure can't read async Raft/rollup state), reads each `[autoscale]` app's metric from the rollup store, and commits `AutoscaleOverride` to Raft. The scheduler now targets *effective* replicas (override ∨ spec), so a scale flows through the same placement→reconcile path as apply. End-to-end test: high metric → override → grows to `max`
- [x] `L4` State reconstruction wired into the leader scheduler loop: on the leadership edge it calls `on_leader_elected`, runs a learning period (feeding reports through `on_report_received`/`check_timeout`), and **gates scheduling** until phase == Active. **Post-Phase-12 audit:** the returned `MissingApp`/`ExtraApp` corrections are discarded, the diff loses colocated replica counts and stale reports can satisfy coverage; correction is Phase 12b.
- [x] `L6` / `L11` Reporting + rollups wired: `RollupWorker` spawned per node, aggregator gets a real rollup store, `/v1/metrics/cluster` serves from it; StateReports carry real capacity (`[resources]` now read) and requested-resource usage. Flat-star kept by design (tree deferred, see ch. 11); fixed a latent DataFusion overflow (`unwrap_or(u64::MAX)` time ranges, 4 handlers)
- [x] `L7` Bind the Wrapper ingress listener — `[ingress]` node-config section (off by default), HTTP + HTTPS listeners (self-signed or disk certs), per-client rate limiting wired into the proxy path, WebSocket pass-through; drain-on-deploy integration lands with `L2` (W7)
- [x] `L8` / `L9` Load the Onion eBPF programs in production; start the DNS responder (fix `M8` fragility) — **`L9`+`M8` done**: `[dns]` config section (off by default), responder spawned from bun, full hardening (recv errors non-fatal, per-query spawned forwards behind a semaphore, connected sockets + transaction-ID checks, NXDOMAIN for unmatched `.internal` with no upstream leak, QTYPE honoured, SERVFAIL on dead upstream), runc containers get `resolv.conf` pointed at the responder. **`L8` done**: `[ebpf]` config section (off by default; `program_dir` defaults to the build-time `OUT_DIR` baked in via `build.rs` `RELIABURGER_BPF_DIR`, so dev/Lima builds self-locate their `.bpf.o`), `bun` loads + attaches `OnionEbpf` at startup (load failure logs and continues without enforcement; non-`ebpf` builds warn that enforcement is off). Verified in the `reliaburger-test` Lima VM: `cargo build --features ebpf` compiles the objects and all 9 `tests/ebpf.rs` integration tests pass (load/attach, backend-map read/write/remove, connect→VIP rewrite, no-backend deny `EPERM`, non-VIP passthrough, `.internal` DNS). Not covered by `make ci` (needs root + kernel 5.7+ + cgroup v2)
  - **Backend/fault/egress eBPF wiring landed** (Phase 11b follow-up, P0–P3): the agent writes the live `backend_map`, fault maps and DNS-refresh egress entries. Namespace firewall maps and rolling-deploy egress (with fail-closed programming) are closed in Phase 12b (NET5/NET6); IPv6/CIDR enforcement remains under the 12b network-policy theme.
- [x] `L10` / `M2` Pickle wired: catalog persists to disk + loads at boot; pushes record real raft-id holders and propose to Raft on council nodes (worker proposal forwarding lands with W6); leader replication loop keeps layers at `[images] redundancy`; scheduled two-phase GC — nominate → Raft-arbitrated approval (`CouncilResponse::GcApproved`) → delete, with an orphan grace window for in-flight pushes. `X1` fixed: `relish build` targets the registry port, `/v1/build` executes buildah for real (honest 501 without it)
- [x] `L13` / `H12` GitOps wired: new `src/lettuce/runner.rs` spawns a leader-only sync loop (clone → poll/webhook → `execute_sync` in `spawn_blocking` → apply changes as `AppSpec`/`AppDelete` Raft writes). Webhook endpoint gets a real channel (was unconditional 503); `[gitops]` config now read. `H12`: `is_key_trusted` no longer falls through to `true` — a valid signature from an unlisted key is rejected. Fixed a latent first-sync bug (a fresh clone has nothing to fetch but nothing applied either → now syncs when HEAD ≠ last-applied). Integration tests in `tests/gitops.rs` (real git repo → Raft; webhook triggers sync)
- [x] `L14` / `L15` Smoker safety context, process/network plumbing and chaos transport blocklists wired; Kill/Pause/Resume, eBPF network faults and partitions have binary-driven tests. **Post-Phase-12 audit:** several advertised resource/node faults are no-ops that return success, CPU stress runs in Bun's cgroup and clear/expiry does not reverse every effect; the measurable-effect/cleanup work is Phase 12b.
- [x] `L16` Initial IPv4 egress allowlist programming and DNS refresh wired and Lima-tested. Phase 12b (NET6) made it fail closed, extended it to rolling deploy and crash-restart, and deletes per-cgroup entries on stop; IPv6/CIDR enforcement remains under the 12b network-policy theme.
- [x] `M17` K8s import fidelity (`command`/`args` concatenated, `env.valueFrom` warned not dropped, namespace preserved, same-name-two-namespaces no longer overwrites)
- [x] `H11` Fix `relish fmt` for nested-table configs — recursive section emission + a round-trip guard that refuses to write output that re-parses differently
- [x] `X1`/`X3`/`X4`/`X5`/`X6` CLI mismatches: `X1` (build → registry port + real buildah execution), `X4` (logs `--grep`/`--since`/`--json-field` wired, server + client side) and `X5` (unreachable agent exits non-zero; explicit `--dry-run` flag added) done; `X3` rollback done (W7); `X6` no-args TUI is out of Stage 4 scope by design → [2026-07-06-plan-tui.md](plans/2026-07-06-plan-tui.md)

### Throughout

- [x] Fix the misleading tests — `L15` "worker isolation" (was a no-op) replaced with `chaos_isolated_member_misses_writes_until_healed`, which really partitions a council member and asserts the isolated node misses writes until healed. `H1` restart tests now assert real post-restart behaviour, not just a counter bump: `health_check_triggers_restart` checks the instance reached a live re-created state (`running`/`health-wait`/`unhealthy`, never stuck in `Preparing`), and `job_failed_retries_then_fails` asserts the terminal `failed` state after retries exhaust
- [x] Remove dead config or wire it — wired during Stage 4: `[resources]` (W4), `[reconstruction]` (W9), `[gitops]` (W10), `[images]` (W5), `[metrics]` (W4), new `[ingress]`/`[dns]`/`[ebpf]` (W2/W3/W12). **Post-Phase-12 correction:** node `labels` parse but never travel in gossip, `[process_workloads]` and `[logs] max_file_size_mb` remain dead, and several newer fields are unused; Phase 12b owns them. `[storage] volumes` (M21) was wired in Phase 12 E0.
- [x] Clear each `[lib-only]` tag from the phases above as its subsystem is genuinely wired — the Smoker fault-injection `[lib-only]` tag (Phase 8) is cleared; the eBPF network-fault enforcement + service-map→backend sync gaps were subsequently closed (P0–P3, see the `L8` item)

### Post-Stage-4 audit fixes (July 2026)

A follow-up audit of the wired Stage-4 code surfaced five issues **not** in the
original review. All fixed on this branch, tests-first (each drives the binary/agent path):

- [x] Redeploy stored `oci_spec: None`, so a replica that crashed after a redeploy could
  never restart (the crash-restart driver filters on `oci_spec.is_some()`). Redeploy now
  records the spec — `redeployed_instance_restarts_after_a_crash`.
- [x] `WorkloadInstance.container_ip` was never populated, leaving the M11 probe fix inert
  and every service-map/eBPF backend registered as loopback. Added `Grill::container_ip`
  (runc netns + Apple inspect) and populate it on deploy/redeploy/restart —
  `deploy_records_the_grills_container_ip_on_instance_and_backend`.
- [x] Ingress proxy forwarded hop-by-hop headers and copied the upstream's
  `Transfer-Encoding`/`Content-Length` onto a re-bodied response (framing mismatch). Now
  filters the RFC 7230 hop-by-hop set both directions — `ingress_reframes_chunked_backend_response`.
- [x] WebSocket pass-through was a stub (dropped the backend stream, omitted
  `Sec-WebSocket-Accept`). Now a real `hyper::upgrade` + `copy_bidirectional` splice —
  `ingress_proxies_websocket_handshake_and_bytes`.
- [x] Single global `Mutex<RateLimiter>` serialised every rate-limited request; replaced with
  a 16-way `ShardedRateLimiter`.

## Phase 12: Optimisations

> Detailed implementation plan: [2026-07-06-plan-optimisations.md](plans/2026-07-06-plan-optimisations.md)
> (revised 2026-07-09 after the Stage 4 wiring merge: slice B — Pickle catalog via Raft,
> replication and two-phase GC — landed as-built in #71; 14 remaining implementation steps,
> refreshed ground truth, config/endpoint/test inventories, Lima acceptance runbook).

- [x] Wire `SubmitBatch` into the agent — `bun::batch`: `POST /v1/batch` (leader-forwarded, full job specs in the request — the CLI used to send names only), capacity from the leader's `AggregatedState` (the deploy scheduler's source; standalone falls back to a local-only entry), `schedule_batch` → leader-side `BatchTracker` → direct HTTP dispatch `POST /v1/batch/run` + completion callbacks `POST /v1/batch/{id}/report` (NOT the placements reconciler — it stops "drifted" workloads, which kills run-to-completion jobs); `GET /v1/batch/{id}`; `relish batch` prints the id, new `relish batch-status`. Watcher distinguishes success from failure-in-backoff via a new `InstanceStatus.exit_code` (any exit maps to `stopped`; the first watcher version called failing jobs completed — caught by the failure-path test). 4 integration tests.
- [x] Wire `SubmitBuild` into the agent — `bun::build_runner`: the Stage 4 sync handler body becomes a spawned runner behind a build registry (`202 {build_id}` + `GET /v1/build/{id}`; the sync form strands the CLI past its 300s timeout on real builds); per-stage `[images] build_timeout_secs` (900); builder capability travels as a `has_buildah` StateReport flag (probed once at worker startup) and incapable nodes delegate to a capable peer via `/v1/build/run` with proxied status reads (`Delegated`); no builder anywhere = honest 503; after push the runner signs the manifest via `AgentCommand::SignImage` (best-effort — standalone has no council); `relish build` polls. Buildah-gated Lima test: trivial context → catalog; macOS tests: 503/404/registry lifecycle.
- [x] Switch port mapping from nftables rules to nftables maps (O(1) lookup at scale) — `grill::portmap` (argv generators, executor trait + recording mock, rollback/incremental `PortMapSet`, legacy-rule sweep parser); `ensure_nft_table` creates the map + single lookup rule with guarded probes (also fixing masquerade-rule duplication), removal is O(1) element delete (handle parsing deleted). **Also wired the mapping in** — `add_port_mapping` had zero production callers (the M21 pattern): the port pair rides `OciSpec.port_mapping`, runc installs it beside the netns, tears down on exit/delete, and adoption rebuilds handles without touching the kernel. Lima: 1000-port stress test. Known limits recorded: prerouting DNAT is host-inbound only; rootless proxies don't survive adoption.
- [x] Managed-volume wiring (E0, fixes review `M21`): the agent creates managed volume host dirs (loop-mounted when sized, Linux root) in `spawn_blocking` before spec generation, failing the deploy closed; `[storage] volumes` config wired via `set_volumes_dir`; **no deletion on Stop** (rebalances/upgrades send Stop; deleting would destroy data — explicit cleanup is future work)
- [x] Heal-loop hardening (B5): `pickle::replication::heal_tick` extracted from the bun binary (testable; `cluster::identity::pickle_peers` shared helper), rarest-first ordering + 10-manifest per-tick cap, leader-pull-first (non-leader pushes now gain redundancy), roadmap auto-heal integration test + 2 more, loopback `registry_bind` startup warning (registry has no auth/TLS — keep firewalled)
- [x] P2P multi-source image downloads — pure `pickle::p2p::plan_downloads` planner (rarest-first, least-loaded balancing, dedup, skip-local; proptested over arbitrary topologies) + bounded-`JoinSet` executor with alternate-holder retry, wired into `ImageStore::pull_and_unpack` via a late-injected `ClusterImageSource` (which also fixes cluster-pushed HTTP-only images being undeployable on other nodes — the external client is HTTPS-only); catalog-known images never fall back to external registries; 100MB/5-layer peer pull verified < 5s
- [x] Pull-through cache full wiring (upstream → Pickle → Raft) — `pickle::upstream` (pure `decide()` on `cache_recheck_secs`, HEAD-compare refresh, `UpstreamRegistry` trait over oci-distribution + counting mock, env-resolved `external_registries` credentials) + `ClusterSource::ensure_external_image` fill path (serialised fills with post-lock recheck; stale-serving when upstream is down; commits under `cache/<host>/<repo>` with holders={self}, heal loop replicates); `cache/` repos exempt from `require_signatures` by construction (pinned by test); peer blob-transfer URLs flatten multi-segment names (single-segment registry routes; blobs are content-addressed)
- [x] Volume snapshots (CoW, scheduled jobs, S3/GCS upload) — E3 adds `bun::snapshot_worker`: `[storage.snapshots] { interval_secs, retain, upload_url }` interval loop (cron-expression parser deliberately rejected), pure `prune_plan`, tar.gz in `spawn_blocking`, upload via `object_store` (`file://`/`s3://`/`gs://` — aws+gcp features enabled); the per-snapshot `uploaded` flag is checkpoint, retry policy, and audit column at once; sweeps report per-app failures without aborting. E2: `grill::snapshot` (Btrfs-only, read-only `-r` snapshots under `.snapshots/`, meta.json sidecars, injected-clock naming); restore = delete live + writable snapshot back, refused by the agent while the app has non-terminal instances (409); no-`--volume` snapshots every provisioned volume (sidecar discovery — works for stopped apps); 4 `/v1/snapshots` routes + `relish snapshot create|list|restore|delete`; roadmap create/corrupt/restore test on the Lima loopback-btrfs rig. Scheduled jobs + object-store upload land in E3.
- [x] Btrfs subvolume quotas (alternative to loop mount) — `grill::btrfs` (statfs detection, pure argv generators + decision table); volumes on Btrfs become subvolumes (qgroup limit when sized — subvolumes even unsized, so E2 can snapshot them); backend recorded in a `*.volume.json` sidecar (delete/snapshot dispatch) which also made creation idempotent (restarts were stacking loop mounts); Lima-gated quota test provisions its own loopback btrfs; `btrfs-progs` added to VM provisioning, `RELIABURGER_BTRFS_TESTS`/`RELIABURGER_BUILDAH_TESTS` gates added to `relish dev test`
- [x] Parquet bloom filters for archive equality pruning — on `app`/`namespace` (1% FPP), not `line` (bloom filters answer equality, not substring LIKE; `bloom_filter_on_read` enabled in remote_query)
- [x] Zstd compression for archived logs — via Parquet's native per-row-group ZSTD codec (random access preserved; >5x vs flat text), not a separate seekable-frame container
- [x] Book chapter 12: "Squeezing Every Drop" — complete: logs (zstd + bloom), nftables maps + the wiring discovery, the as-shipped Raft catalog/GC/heal design (M2 TOCTOU), P2P planner + executor, pull-through cache, volumes/quotas/snapshots/scheduled backups, batch + build, and phase-wide lessons
- [x] All Phase 12 tests green — 1,981 on macOS (`make ci`) + the full Lima gated run (`relish dev test`: netns map DNAT + 1000-port stress, btrfs quota ENOSPC, snapshot create/corrupt/restore, real buildah build into the catalog, plus the existing runc/eBPF suites). Remaining acceptance: the live 3-node runbook in [the plan §10](plans/2026-07-06-plan-optimisations.md)

## Phase 12b: Correctness, Security & Convergence

> Consolidated review: [2026-07-10-review-past-phase-12.md](plans/2026-07-10-review-past-phase-12.md).
> It reconciles the renamed [code walkthrough](plans/2026-07-09-review-codex.md),
> [design discrepancy register](plans/2026-07-09-review-design-discrepancies.md),
> the former "Beyond Phase 11b" backlog and the 24 Low findings. M7 and M21/codex-M2
> are fixed. M20 is still open: object-store features are compiled, but Ketchup export
> still uses PathBuf plus std::fs::copy. X6 remains only in Phase 13.

Every top-level checkbox below is one PR-sized theme. Write the binary-driven
acceptance test first, update the relevant book chapter in the same PR, and check
the theme only after default and platform-gated tests pass. Done findings within a
theme are ticked as nested `- [x]` items; a theme's own box is checked only when the
whole theme lands.

### 12b.1 — Stop the bleeding

- [x] **Internal API trust boundary** — central route/role/scope matrix; require node
  identity for batch/build run and report endpoints; make callback and registry
  destinations server-owned; never forward the service token to request data; strictly
  parse build digests/Dockerfile paths and bound, sandbox and clean archive extraction.
  Reject anonymous and ReadOnly execution, callback SSRF, path traversal, sparse/oversized
  contexts and a Buildah process that survives timeout (new JOB1-JOB2, H4/D8).
  - [x] `require_system` on `/v1/batch/run|report` and `/v1/build/run`, `authorize(Deployer)`
    on the submit endpoints, callback bounded to known members, and the cluster service token
    no longer forwarded to caller-controlled URLs (JOB1); build `context_digest` validated as a
    well-formed OCI digest before it becomes a temp path, killing the `sha256:../../x` traversal
    (JOB2).
  - [x] Server-owned registry destination (JOB2 residual): `registry_port` removed from
    `BuildSubmitRequest`, `#[serde(deny_unknown_fields)]` makes a smuggled port a 400, and the
    build runner reads `[images] registry_port` from its own config — a caller can no longer
    point a privileged Bun at an arbitrary localhost service.
  - [x] Bounded, sandboxed, self-cleaning context extraction (JOB6): `pickle::build::unpack_context`
    streams the download to disk under `[images] max_context_bytes` (default 256 MiB), rejects
    absolute/`..`/symlink/hardlink/device/FIFO entries, counts *written* bytes to defeat sparse
    bombs, caps the entry count and strips setuid/setgid bits; per-build `ScopedDir` (RAII `Drop`)
    with a random suffix gives concurrent same-digest builds distinct dirs and cleans up on every
    exit path.
  - [x] Dockerfile path confinement (JOB6): `validate_dockerfile_path` rejects absolute/`..`
    paths in `validate_build`, and `confine_dockerfile` canonicalises the resolved path inside
    the extracted context (catches symlink-directory escapes) before Buildah reads it.
  - [x] Kill Buildah on timeout (JOB6): each stage runs via `run_bounded` in its own Unix process
    group with `kill_on_drop`; on timeout the whole group gets SIGKILL, so Buildah's children die
    with it instead of orphaning — proven by a process-group shim test.
  - [x] Central route→principal matrix (H4/D8 groundwork): `bun::authz::ROUTE_MATRIX` maps every
    mounted route to `Public | AnyToken | Deployer | Admin | System`; a source-scan test asserts
    every `.route(…)` the router mounts has a matrix entry, so the auditable role list can't drift
    from the code. (Scope enforcement stays in 12b.3.)
- [x] **Secret and workload-identity safety** — make rotation generation-aware:
  encrypt with the newest key, decrypt with active generations, re-encrypt and acknowledge
  every stored secret before retiring the old key, and reject malformed/concurrent
  rotations. Issue exact validity windows, rebuild SANs server-side, store identity in
  per-instance tmpfs, preserve it through rolling/adoption and clean it on removal
  (new PKI6-PKI8, D9).
  - [x] Generation-aware secret rotation: encryption picks the newest non-read-only key,
    decryption tries every live generation, finalize refuses to empty a scope, and a malformed
    rotate body is rejected — so a secret sealed under generation N survives the rotation window
    (PKI8).
  - [x] Exact workload-certificate validity (PKI6): `validate_and_sign_csr` takes an injected
    clock and issues `now − 5 min` (skew backdate) to `now + 1 h` as timestamps via
    `time::OffsetDateTime`; the calendar-date helpers that gave a one-hour cert equal midnight
    bounds are deleted, and `cert::check_validity_at` pins the window in tests.
  - [x] Server-side SAN rebuild (PKI6): the signer takes only the public key from the CSR and
    rebuilds DN/usages/serial/SANs from the expected identity, so a CSR smuggling another
    workload's SPIFFE URI or a DNS SAN yields a certificate with exactly the expected URI
    (`smuggled_csr_sans_are_not_signed` parses the issued DER).
  - [x] Per-instance identity directories (PKI7): `{volumes}/.identity/{instance_id}` replaces
    the app-scoped dir (replicas no longer overwrite each other's keys — OCI mount source is
    per instance); prepared at the same pre-create seam as egress programming, tmpfs-backed
    (`mode=0700`, size-bounded) on Linux root with key/token chowned to the workload UID,
    plain `0700` dir elsewhere (documented gap); removed (and unmounted) on stop, rolling
    replacement and rollback; fresh deploys now provision identity at all (previously only
    the rolling path did).
  - [x] Restart-safe rotation (D9): a `meta.json` sidecar (SPIFFE URI + schedule, no secrets)
    lets adoption rebuild each instance's identity and rotation timetable from disk instead of
    `identity: None`; orphaned/legacy identity dirs are swept at adoption; a rate-limited
    retry provisions running instances that still lack an identity.
  - [x] Verify-before-retire (PKI8): applying an `AppSpec` records the sealing generation per
    encrypted env value (`SecurityState.secret_seals`, self-describing JSON with
    `#[serde(default)]`); finalize refuses (new `CouncilResponse::Refused`, surfaced as HTTP
    409) while any secret in scope is sealed under an older — or unknown/legacy — generation,
    naming the offenders; re-applying the re-encrypted spec unblocks it (legacy fixture test).
  - [x] One rotation at a time (PKI8): a second `RotateSecretKey` for a scope with an
    un-finalised rotation is refused with instructions; idempotent retries of the same
    rotation are deduped on the generation number.
- [x] **Pickle reference integrity** — include raw manifest/index blobs in holder,
  replication and GC reachability; validate JSON, media types, descriptor digest/size and
  referenced blob existence before returning Created; use canonical repository identity
  and immutable digests through policy, scheduling and runtime pull. Test
  push → GC → peer pull and reject the existing missing-layer Created behaviour
  (new REG1/REG3/IMG1, old Low manifest check).
  - [x] `ImageManifest::referenced_digests()` is the one authoritative "everything this
    tag pins" set (manifest blob + config + layers); holder commits, GC protection, the
    heal loop, replication, peer/P2P pulls and `image_available_locally` all use it, so a
    tagged manifest's own blob is never orphaned and heals to `[images] redundancy` like
    any layer; the pull-through fill stores the raw upstream manifest bytes too (REG1).
    Old persisted catalogues (no manifest-blob holders) load unchanged, GC keeps the blob
    and one heal tick restores redundancy — fixture-tested, no migration needed.
  - [x] `manifest_put` validates before Created: JSON parse, known media type (body or
    Content-Type header; OCI/Docker manifests and indexes), well-formed descriptor
    digests, descriptor sizes matching stored blobs, referenced blobs present (config +
    layers, or sub-manifests for an index), digest-reference PUTs matching the body —
    each rejection an OCI error body (`MANIFEST_INVALID`/`MANIFEST_BLOB_UNKNOWN`/
    `DIGEST_INVALID`), nothing stored or tagged on rejection; the misleading
    missing-layer test now asserts rejection, and manifest GET stays byte-identical
    (REG3, old Low manifest check).
  - [x] Trust lookups use the canonical repository path (no basename stripping: `team/app`
    hits its own policy, external `docker.io/library/app` can't alias a local `app`,
    `cache/…` exempt by construction); `verify_image_signature` returns the verified
    manifest digest and the agent deploys the digest-pinned `repo@sha256:…` reference,
    which parses through `ImageReference`/`ClusterSource` content-addressed — a tag moved
    between verify and pull cannot swap the image (IMG1). Acceptance test drives push →
    GC past grace → manifest GET 200 → peer pull in `tests/pickle_integrity.rs`.
- [x] **Network policy enforcement** — write namespace/cgroup firewall maps for every
  instance; program egress before process start and on rolling deploy; fail deployment
  closed when required policy cannot be installed; reconcile kernel truth and delete every
  per-cgroup entry. Add IPv6/connect6 and CIDR enforcement, safe nftables input, required-map
  validation and IPv4/IPv6 perimeter rules with timeouts (new NET5-NET8, D5, old BPF/nft Lows).
  - [x] Namespace-firewall maps (`cgroup_namespace_map` + `firewall_map`) written and
    reconciled on every deploy/redeploy/restart/stop, so the connect hook actually enforces
    cross-namespace isolation instead of failing open on empty maps (NET5); egress programmed on
    rolling redeploy and crash-restart through one shared `finish_instance_networking` helper,
    failing closed (deny-all) on any programming error, with stop/redeploy deleting the allow
    entries so a recycled cgroup id inherits nothing (NET6).
  - [x] IPv6 egress enforcement: a `cgroup/connect6` program mirrors the policy against an
    `egress6_map` (v4-mapped `::ffff:a.b.c.d` destinations judged against the IPv4 policy),
    the parser keeps AAAA records and accepts bracketed v6 entries, and a deploy with an
    allowlist is refused when connect6 cannot attach — a v4-only allowlist is bypassable
    over IPv6, so it is not enforced (NET7). Lima test pins the old bypass now denying.
  - [x] CIDR enforcement via per-family `BPF_MAP_TYPE_LPM_TRIE` maps keyed by big-endian
    cgroup id + network address; `merge_cidr_ports` folds enclosing prefixes' ports into
    more specific entries (longest-prefix match would otherwise shadow them) with a tested
    8-port cap; the parser validates prefix lengths per family and rejects host bits with
    the normalised form in the error (NET7).
  - [x] No BPF map panics or discarded errors: every `map_mut(...).unwrap()` replaced with a
    typed `BpfMapError`, update/remove results propagated to logging callers, and the loader
    validates all required maps and programs against one list at load time, failing with the
    full roster of what is missing (NET8).
  - [x] nftables hardening: admin CIDRs parsed and re-serialised before rendering (a value
    like `10.0.0.0/8; drop` is a parse error, never an injected rule), the perimeter renders
    both `ip` and `ip6` `reliaburger_fw` tables with family-appropriate sources, and every
    `nft` invocation is bounded by a 10s timeout (NET8, old nftables Lows).
  - [x] Start-window closed on root-mode runc: the agent creates the instance's cgroup
    directory itself, programs egress against its inode, then starts (create → program →
    start) in fresh deploy, rolling redeploy and crash-restart; programming errors delete
    the created container and fail the deploy closed. Gated by `Grill::honours_cgroup_path`;
    ProcessGrill/Apple keep the documented post-start path. Lima test proves pre-start
    programming via a pid-less mock grill.
  - [x] Kernel-truth sweep (`[ebpf] sweep_interval_secs`, default 60, 0 disables): enumerates
    enforcement flags and allow entries in the kernel, scrubs state whose cgroup no longer
    maps to a live instance, rebuilds bindings adopted instances lost across a Bun restart,
    rewrites live bindings (healing lost entries) and prunes stale `cgroup_namespace_map`
    keys; a no-op sweep is silent. Pure `plan_egress_sweep` tested in the default suite.
- [x] **Consensus persistence safety** — version and checksum snapshots, validate the
  snapshot/log boundary, propagate vote/log/initialisation errors and refuse startup when
  compacted state cannot be reconstructed. Test compact → corrupt → restart returns an
  error instead of an empty cluster state (new CP3).
  - [x] A present-but-undecodable Raft snapshot is now a hard startup error instead of a silent
    empty-state boot after log compaction; a genuinely absent snapshot still loads an empty
    default (CP3).
  - [x] Persisted snapshots carry a versioned envelope: format version + SHA-256 payload
    checksum written in the same redb transaction as the payload. Load rejects a checksum
    mismatch (naming both sums) and an unknown version (naming both versions); a pre-envelope
    legacy snapshot still loads with a warning and is rewritten enveloped on the next persist,
    pinned by a fixture test (CP3).
  - [x] Snapshot/log purge-boundary validation at startup (`council::validate_purge_boundary`,
    called from `cluster::runtime::open_raft_storage` before Raft starts): a log purged past
    what the snapshot covers — or purged with no snapshot at all — refuses startup with the
    exact purged/covered indices instead of booting with an unreconstructable gap (CP3).
  - [x] `DurableLogStore::is_fresh` returns `Result<bool, _>` instead of mapping read errors
    to "fresh": an unreadable store is fatal at startup, never a re-bootstrap (the C3
    split-brain through the error path); `truncate`/`purge` propagate row read errors instead
    of silently skipping keys (CP3).
  - [x] Acceptance test through the real startup seam (`tests/council_persistence.rs`): a
    single-node council on durable storage writes state, snapshots, purges the log, then a
    flipped payload byte or a deleted snapshot makes restart return an error, while a clean
    compact restores every entry (CP3).

### 12b.2 — Make the cluster converge

- [x] **Control-plane directory and reporting robustness** — publish authenticated leader
  API/reporting endpoints to every gossip member; make non-voters follow leader failover;
  replace weak/version-dependent node and parent hashes; evict departed/stale nodes; filter
  terminal workloads from running/capacity; stop closed-channel spins and supervise
  long-lived task failures. Acceptance: an 8+ node cluster reconciles and reports through
  leader failover (H1/D1, CP1/CP5/CP6/CP10).
  - [x] Gossip directory extension: every datagram carries the sender's advertised
    API/reporting endpoints plus the highest-term leader hint, HMAC-authenticated, appended
    AFTER the bincode message body so old and new peers keep gossiping in both directions
    (pinned by legacy-decoder tests; the 10k gossip test is untouched).
  - [x] Non-voters follow leader failover (H1/D1/CP1): the leader-target maintainer and the
    placement reconciler resolve the leader through Raft metrics OR the gossip directory
    (`cluster::directory::resolve_leader`); a newer gossip hint outranks a deposed leader's
    stale metrics; the derived `gossip-ip:9117` offset survives only as a propagation-lag
    fallback; workers re-report immediately on re-point.
  - [x] Epoch-scoped honest aggregation (CP5): report freshness judged on aggregator-side
    monotonic receive time (never sender wall clock); entries tagged with the leadership
    term and excluded once the epoch moves on; eviction of members gossip drops
    (Dead/Left) and of entries older than three stale windows.
  - [x] Terminal workloads out of running/capacity (CP6): `build_report` excludes
    Stopped/Failed instances from `running_apps`, request sums and allocated ports;
    Stopping still counts (draining holds resources).
  - [x] Version-stable hashes (CP10): reporting parent assignment moved from
    `DefaultHasher` to a local FNV-1a with the exact mapping pinned by test; Raft-id djb2
    documented and value-pinned (kept: changing it would break durable state and
    mixed-version clusters — see `cluster::identity`).
  - [x] No silent task death, no hot spins (CP10): report/rollup workers stop polling a
    closed leader-target watch (previously a 100%-CPU spin) but keep reporting to the
    last known target — only the shutdown token ends them; cluster runtime tasks wrapped
    in `spawn_supervised` (loud log on unexpected exit; respawn deliberately rejected —
    documented in `runtime.rs`).
  - [x] Acceptance (`tests/cluster_failover.rs`, gated `RELIABURGER_CLUSTER_TESTS=1`):
    nine fully wired in-process nodes (7 voters + 2 workers), all nine report to the
    leader, a daemonset converges onto the workers, the leader is killed, every survivor
    re-points at the new leader, coverage recovers in the new epoch and new placements
    still reconcile. Passes locally in ~18s.
- [x] **Council membership self-healing** — add replacement as learner, wait for catch-up,
  promote, transfer/avoid leadership as needed and remove the dead or unsuitable voter via
  joint consensus. Prove quorum recovers with healthy spares and never removes the active
  leader mid-change (H2/D2).
  - [x] Pure replacement planner (`council::selection::plan_council_action`): observes
    voters/learners, gossip health, ranked spares and replication progress; returns at most
    one action per tick (`AddLearner`/`Promote`/`RemoveVoter`/`RemoveLearner`/`Nothing`).
    Never proposes removing the leader, never plans a change live voters can't commit or
    that drops below `min_council_size`, holds entirely when quorum is already lost (the T3
    disaster-recovery seam), and prefers add-before-remove so quorum never depends on the
    dead node's vote. Proptest pins the invariants for arbitrary observations (H2).
  - [x] Hysteresis via `council::selection::HealthTracker`: eviction requires `dead_window`
    (30s) of continuous death/absence, candidacy requires `candidate_alive_window` (5s) of
    continuous life; observed transitions reset the clocks (flap-proof) and pre-tracker
    members seed from gossip `first_seen` so warm clusters don't re-wait after failover.
    A voter inside the dead window still holds its seat, so a flap adds no learner either.
  - [x] Catch-up gating: promotion compares the learner's replicated log index (openraft
    replication metrics) against the leader's last index within `max_promotion_lag` (64);
    missing metrics count as behind unless the log is empty (D2).
  - [x] Reconciler rework (`cluster::runtime::spawn_council_reconciler[_with_config]`):
    re-plans each tick from observed state (idempotent, timeout-bounded, errors logged —
    the M15 non-wedging property), executes exactly one action, feeds the health tracker
    on followers too so a new leader starts warm. Voter eviction uses non-retaining
    `change_membership` (`CouncilNode::change_membership_evicting`) so dead nodes leave the
    membership entirely; dead learners are dropped via `CouncilNode::remove_learner`.
  - [x] Tests: 17 planner + 5 tracker unit tests and the planner proptest; add-then-evict
    joint-consensus composition against the in-memory Raft harness; gated acceptance suite
    (`tests/council_self_healing.rs`, `RELIABURGER_CLUSTER_TESTS=1`) — kill-a-voter heals
    to three healthy voters with writes committing throughout and the leader never removed,
    a learner killed mid-catch-up (Raft-layer partition) blocks no healthy replacement, and
    a flapping node inside the window causes zero churn.
- [x] **Council disaster recovery** — full-council-loss recovery, encrypted external
  backup/restore, explicit reconstruction thresholds and disk-pressure council resignation.
  Black-box loss/recovery exercised, not just candidate-selection helpers (D21/CP12).
  - [x] Encrypted external backup: leader-only periodic export of the state-machine snapshot
    (`src/council/backup.rs`), sealed with an HKDF-derived key
    (`reliaburger-council-backup-seal-v1`) + AES-256-GCM over the #83 snapshot payload,
    uploaded via `object_store` (`file://`/`s3://`/`gs://`) with retention pruning.
    `[cluster.backup] { url, interval_secs, retain }`, off by default. Tests: seal→tamper
    refuses, round-trip restores identical `DesiredState`, retention prunes oldest, disabled
    by default, store upload/list/prune/latest.
  - [x] Full-council-loss recovery (`src/council/recovery.rs`): `relish council recover
    --data-dir --from <url> [--master-key] [--force]` restores from a sealed backup or the
    node's own durable snapshot, refuses when a live voter answers (unless `--force`), wipes
    the dead cluster's log, and stamps a **new recovery epoch** into `DesiredState`. Next
    start re-bootstraps a single-voter Raft that the #88 reconciler regrows. Tests:
    state-machine restore + re-bootstrap, live-council refusal guard, epoch bump across
    repeated recoveries.
  - [x] Explicit reconstruction thresholds: `[reconstruction]` gains bounds validation
    (`0 < coverage ≤ 100`, positive timeouts), validated at bun startup; the recovery path
    and the leadership edge share the same values. Tests: config parse + bounds.
  - [x] Disk-pressure council resignation: a sustained-pressure state machine with hysteresis
    (`src/bun/disk_pressure.rs`); a new `disk_pressured` planner input replaces a pressured
    follower add-before-remove (never below quorum — the existing proptest holds with the new
    input); a pressured leader deposes itself first via the verified `trigger().elect()`
    mechanism. Tests: resignation windowing, planner pressure cases, gated leader-deposition.
  - [x] Disk-pressure signal path wired end-to-end (12b.2 T3 follow-up): the resignation
    mechanism above shipped complete, but production `start()` fed the reconciler a permanently
    empty pressured set — a voter only knows its own disk locally, and nothing carried that to
    the leader. Now the node advertises a `disk_pressured: bool` on the authenticated gossip
    directory extension (same trailing, position-versioned, HMAC-covered wire model as
    `labels`); the bun disk-pressure loop drives it off the hysteresis state machine; and a
    consumer task on the reconciler node folds the directory's pressured set with the live voter
    set into the Raft-id set the planner acts on. Tests: gossip wire round-trip + both-direction
    legacy tolerance + flipped-bit HMAC rejection, directory fold/prune, `pressured_voter_ids`
    filtering, and a gated end-to-end test driving pressure through real gossip into the set.
  - [x] Verified openraft 0.9 `trigger().elect()` deposes the current leader (gated step-0
    test) — no version bump needed.
  - [x] Gated black-box acceptance (`tests/council_disaster_recovery.rs`,
    `RELIABURGER_CLUSTER_TESTS=1`): three voters + backup, kill all three, recover on a
    survivor, council re-forms with restored state and a fresh epoch and regrows; plus a
    reconciler-driven leader-deposition-under-pressure test.
- [x] **Scheduler truth, labels, quotas and autoscaling** — advertise authenticated node
  labels/resources; use one mutable reservation cache per planning pass; revalidate
  generation, resources, labels, readiness and cordon; converge daemon workloads against
  eligible nodes; wire namespace quotas. Validate autoscale bounds/durations, use structured
  namespace/app metrics and configured windows, commit before cooldown and clear stale
  overrides. Reject numeric overflow (CP7-CP8, DEP8-DEP9, D13).
  - [x] Labels travel (CP7): the gossip `DirectoryExtension` now carries the sending node's
    placement labels (`BTreeMap`, bounded to 16 keys / 64-byte fields / 512 total bytes so
    datagrams stay well under the MTU), HMAC-authenticated like #89's endpoints and appended
    as trailing bytes so old and pre-labels peers keep gossiping in both directions (pinned by
    a legacy-extension decoder test). Received labels land on the stamping node's membership
    record and the `NodeDirectory`, so `filter_nodes`' label filtering and zone-aware council
    selection have live input instead of the empty map Mustard used to insert. `[node] labels`
    flows from node.toml → `ClusterParams` → `set_advertised_endpoints`.
  - [x] One reservation cache per planning pass (CP8): the leader builds the cluster cache
    ONCE per tick and plans every app against a single mutable reservation view — each
    committed placement subtracts before the next app plans, so two apps that together exceed
    one node's headroom no longer both land on it (regression test: exactly one is placed, the
    other refused). `apply_upgrade_cordon` is wired into the pass (Phase 14's helper finally
    has a caller); a cordoned node receives nothing. Each decision is revalidated against the
    LATEST membership immediately before the async Raft write, so a node that dies mid-pass is
    dropped rather than assigned. Pure `plan_scheduling_pass` is unit-tested for all four cases.
  - [x] Daemon convergence: a daemon app re-plans over the currently eligible nodes each tick
    (a placement targeting a departed/cordoned node counts as stale), so it gains an instance
    when a node joins/becomes eligible and loses one when a node leaves. Tests cover node-join
    growth and departed-node re-plan.
  - [x] Namespace quotas wired (enforcement seam): `QuotaLedger` accumulates per-namespace
    usage cumulatively across the pass and refuses an app that would bust its quota, with a
    clear deploy-time error. **Handoff closed in T6:** namespace resources are now desired
    state, and the scheduling pass builds its ledger from `desired.namespaces` via
    `ledger_from_namespaces` (`orchestrate.rs:150`), so a declared quota actually rejects an
    over-budget app on the apply path.
  - [x] Autoscale lifecycle (DEP8): `AutoscaleConfig::from_spec` returns a validated `Result`
    (rejects `min > max`, zero max, zero/unparseable evaluation window — cooldown may be zero,
    out-of-range threshold) and config validation surfaces it on `relish apply`; the cooldown
    starts only *after* a successful Raft write (a failed write no longer suppresses the next
    attempt); stale overrides are cleared in the state machine when an app's replica baseline
    changes or the app is deleted (an image-only redeploy keeps the override); the rollup query
    uses the configured `evaluation_window` instead of a hardcoded five minutes.
  - [x] Numeric overflow rejected (DEP9): `parse_num` uses checked multiplication (a huge
    memory string is a validation error, not a wrapped small value) and the quota arithmetic
    saturates throughout.
- [x] **Transactional desired state and deployment** — make instance identity include
  namespace/generation/ordinal; apply AppDelete through Raft on cluster stop; consume
  count/generation-aware reconstruction corrections; wait for terminal deploy/stop outcome,
  retry/reschedule failures and rebuild applied state after restart. Move image/init/runtime/
  health waits outside Bun's command loop and honour surge, unavailable, drain and rollback
  semantics (H7/D10, DEP1-DEP6, codex-M3).
  - [x] PR 1 — Namespaced instance identity + AppDelete-on-stop + durable applied-state
    (DEP1/DEP2/DEP3).
    - [x] `InstanceIdentity { namespace, app, generation: Option<u64>, ordinal }`
      (`grill::mod`) with one canonical string form, `{namespace}__{app}[-g{gen}]-{ordinal}`
      (`__` is illegal in a DNS-1123 label, so the namespace parses back out unambiguously
      even for a hyphenated app name). Replaces both old formats (`{app}-{i}` and the canary
      `{app}-g{gen}-{i}`) at every construction site (supervisor app/job, agent canary,
      reporting ordinal, record replica-index, upgrade inventory). Round-trip + hyphenated-app
      unit tests; DEP1 collision regression (two same-name apps in different namespaces
      coexist in the supervisor).
    - [x] Adoption compat: a pre-theme record (bare `web-0`) re-adopts under the canonical
      key rebuilt from the record's structured `namespace`/`app_name`/`replica_index` fields,
      while the runtime, identity dir and log stem stay keyed on the legacy runtime id the
      container ran as — so an in-place upgrade across the change never orphans a workload.
      The upgrade marker gained a `full_id` (serde-default) so a marker written pre-theme
      still loads; fixture test pins it.
    - [x] Cluster stop proposes `AppDelete` through Raft (`stop_handler` → `cluster_stop`,
      leader-forwarded like `apply`), so desired state clears and no reconciler resurrects the
      app; a leader with no local replica still clears cluster state (no spurious 404). The
      local container stop is best-effort after the delete commits. Standalone mode keeps the
      local-only stop. Gated 3-node test: deploy, stop through any node, desired state clears
      and stays clear across reconcile ticks.
    - [x] Durable, terminal-outcome applied-state (`cluster::applied`, DEP3/H7/D10): the
      placement reconciler marks a placement applied only on the deploy's terminal `Complete`
      event (a failed deploy is retried next tick), and persists the applied map to a
      self-describing JSON checkpoint (atomic temp+rename, schema-versioned) reloaded on boot —
      so a restart resumes converged work without double-deploying or forgetting in-flight
      work. Unit tests: round-trip, corrupt-loads-empty, restart-skip-vs-redeploy semantics.
    - [x] Snapshot compat: the identity/applied-state changes touch no council/Raft state, so a
      pre-theme snapshot loads unchanged (the #83 envelope loader / existing fixture holds).
    - [x] `make ci` green (fmt, clippy `-D warnings`, 2387-test default suite); gated
      `RELIABURGER_CLUSTER_TESTS=1` `placement`/`cluster_failover` suites green. Book: chapter 2
      (instance identity), chapter 7 (applied-means-done, AppDelete-on-stop).
  - [x] PR 2 — Deploy work off the command loop (DEP4/codex-M3).
    - [x] Each `Deploy` command now spawns a `DeployWorker` task instead of awaiting `deploy`
      inline in the command arm, so image preparation, init-container polling, runtime
      create/start and the rolling health wait no longer block the Bun command loop. The loop
      keeps servicing health checks, crash restarts, status and further deploys while a pull is
      in flight; concurrent deploys interleave rather than serialise.
    - [x] The supervisor state machine stays authoritative: the worker owns only the blocking
      grill I/O (grill+port allocator are `Arc`-backed clones), and drives every state
      transition and every supervisor / service-map / networking mutation back through a
      `DeployOp` message channel (`deploy_ops_rx`) drained in the loop's `select!` and
      dispatched to the same `&mut self` helpers the old serial path used. No deploy logic
      moved — only where it runs.
    - [x] Preserved behaviours: create → program-egress → start ordering (#86), the #87
      per-instance identity lifecycle, health-gating, crash-restart, rolling rollback on an
      unhealthy new replica, and PR 1's namespaced identity + durable applied-state. The fresh,
      rolling and job paths were split into loop-side (`prepare_fresh_instance`,
      `finish_fresh_instance`, `prepare_rolling_instance`, `finalise_rolling_deploy`,
      `rollback_rolling_deploy`, `finish_job_instance`) and task-side (`drive_fresh_instance`,
      `drive_job`, `rolling_redeploy`) halves.
    - [x] Failing-first tests: `slow_deploy_does_not_block_the_command_loop` (a 3s image-pull
      deploy still answers a `Status` in <500ms) and `concurrent_deploys_interleave` (two
      deploys' creates are both in flight after one delay). `MockGrill::set_create_delay`
      simulates a slow pull. Both fail against the old inline deploy.
    - [x] `make ci` green (fmt, clippy `-D warnings`, default suite); gated
      `RELIABURGER_CLUSTER_TESTS=1` `placement`/`cluster_failover` suites green. Book: chapter 7
      (a slow deploy shouldn't freeze the node).
  - [x] PR 3 — Deploy semantics: drain / surge / rollback and exit-aware stop (DEP5/DEP6).
    - [x] The library-only `DrainTracker` now governs the live path. A `SharedDrains`
      (`Arc<tokio::sync::Mutex<DrainTracker>>`) is owned by the agent and handed to the Wrapper
      proxy (`bind_proxy_with_drains`, `agent.drains_handle()`). The proxy bumps a draining
      backend's in-flight count around each forwarded request/WebSocket via a `DrainGuard` RAII
      type whose `Drop` releases the count on every exit path; `select_backend` now carries the
      `instance_id` so the count keys correctly.
    - [x] DEP5 rolling retire drains through the live path: `finalise_rolling_deploy` registers
      the new backends and rebuilds the routing table *first* (so the proxy routes new traffic to
      the fresh instances), then `retire_with_drain` starts a per-instance drain, waits for
      in-flight requests to finish (up to `drain_timeout`) via `wait_drained`, and only then does
      an exit-aware stop. Surge-first ordering keeps the serving count from dipping below
      `replicas - max_unavailable`; the existing unhealthy-replica auto-rollback path is
      preserved.
    - [x] DEP6 exit-aware stop: `stop_and_wait_for_exit` (SIGTERM → poll for exit up to
      `STOP_GRACE_SECS` → SIGKILL) is shared by `stop_app`, the rolling retire and `shutdown_all`.
      The supervisor records `Stopped` only after the runtime confirms the exit, so container and
      supervisor state cannot diverge.
    - [x] Failing-first tests: `retire_waits_for_in_flight_request_before_kill` and the live
      `live_proxy_holds_drain_open_while_a_request_is_in_flight` (a real bound proxy holds an
      in-flight request open and the drain does not complete until it returns);
      `rolling_redeploy_never_drops_below_target_availability` (new `start` precedes old
      `stop`/`kill`); `stop_escalates_to_kill_when_process_ignores_sigterm` and
      `stop_reports_stopped_after_exit_without_kill` (`MockGrill::set_ignore_stop` simulates a
      process that refuses SIGTERM). All fail against the pre-PR kill-immediately path.
    - [x] `make ci` green (fmt, clippy `-D warnings`, 2405-test default suite); gated
      `RELIABURGER_CLUSTER_TESTS=1` `placement`/`cluster_failover` suites green. Book: chapter 7
      (wiring drain/surge/rollback into the live path, why stop must wait for exit). Theme
      complete.
- [x] **Complete declarative resources** — validate and apply apps, jobs, builds, namespaces
  and permissions through the same local/Raft/GitOps path; enforce namespace and permission
  resources plus build scope; reject or remove every parsed field that cannot affect the
  binary. Acceptance: one configuration containing every resource kind converges identically
  through manual apply and GitOps (DEP7/D12). **This theme closes the 12b.2 "Make the cluster
  converge" tier.**
  - [x] Namespaces and permissions as desired state (DEP7): append-only serde-default
    `NamespaceSpec`/`NamespaceDelete`/`PermissionSpec`/`PermissionDelete` Raft variants and
    `namespaces`/`permissions` `BTreeMap`s in `DesiredState`; a pre-theme snapshot without
    those keys loads cleanly (fixture test through the strict loader). `Config::validate`
    rejects negative/overflowing namespace budgets, zero caps, unknown permission actions,
    and permissions/builds referencing a namespace that exists in neither the config nor
    committed desired state (`validate_against`).
  - [x] Shared apply path (the linchpin): one `council::config_to_desired_writes` turns a
    parsed `Config` into the ordered write set (namespaces, then permissions, then apps).
    BOTH manual `cluster_apply` (`bun/api.rs`) and Lettuce call it, so the two paths cannot
    diverge by construction. Deletion semantics: manual `relish apply` is additive (writes
    what's in the file, prunes nothing — matching how app apply already behaved); GitOps
    reconciles the whole repo and prunes namespaces/permissions consistently with apps.
  - [x] Namespace quota handoff closed (T4 seam): the scheduling pass builds its
    `QuotaLedger` from `desired.namespaces` via `ledger_from_namespaces`
    (`orchestrate.rs:150`), so an app exceeding its namespace CPU/replica budget is refused
    at deploy time with a logged reason; a namespace with headroom admits it. `quota_from_spec`
    parses the config's `"8000m"`/`"16Gi"` strings into the scheduler's integer quota.
  - [x] Build scope wired (DEP7): `Config::validate` runs `validate_build_namespace`, and
    `/v1/build` submission runs `check_namespace_scope` against desired-state namespaces so a
    build pushing to a `pickle://ns/name` destination must target an existing namespace.
  - [x] Lettuce through the unified path, atomically (D12): `resource_change_to_request` was
    replaced by `change_to_request`/`payload_to_request`/`remove_to_request` handling apps,
    namespaces and permissions (no more silent `None` skip); the app write now keys on the
    spec's own namespace, not a hardcoded `default` (the divergence the acceptance test
    caught). `last_applied_commit` advances ONLY when every write in the sync succeeds
    (`apply_changes` returns `Err(id)` and stops on first failure); a partial/failed sync
    leaves the commit unadvanced and re-applies (idempotently) next tick.
  - [x] Parsed-but-dead audit: `NamespaceSpec` (all five fields feed the quota),
    `PermissionSpec` (actions/namespaces validated, stored as desired state; principal→
    permission *binding* is AUTH1/12b.3, noted in the book), `BuildSpec`
    (destination/namespace/context validated). Nothing removed — every field now affects the
    binary.
  - [x] Acceptance: `manual_apply_and_gitops_converge_identically` asserts one every-kind
    config produces byte-identical declarative `DesiredState` via manual apply and GitOps.
    `make ci` green (fmt, clippy `-D warnings`, 2431-test default suite); gated
    `RELIABURGER_CLUSTER_TESTS=1` `placement`/`cluster_failover` suites green. Book: chapter 7
    (why apply and GitOps must share one path; why `last_applied` advancing on partial failure
    silently drops resources). Theme complete; 12b.2 tier complete.
- [x] **Durable batch and build execution** — use one authoritative namespace; persist
  monotonic IDs, trackers and terminal state; include unschedulable jobs; bound and retry
  dispatch/callbacks; make duplicate reports idempotent and run GC. Transfer build context
  to the chosen builder, derive registry endpoints from node config, isolate/clean tempdirs,
  bound CLI polling, retry another builder and make required signing part of terminal
  success. Test leader restart, lost callback and delegation from a non-builder entry node
  (D18, JOB3-JOB7, old batch-order Low).
  - [x] One authoritative job namespace (JOB3): resolved once at submit (conflicting
    submission/spec values are a 400), written into both the record and the spec so
    dispatch, deploy and the watcher agree; non-default-namespace batch completes e2e.
  - [x] Raft-durable trackers and IDs (JOB4): `batch_state`/`build_state` in `DesiredState`
    (serde defaults; pre-theme snapshot fixture-tested through the envelope loader),
    append-only `BatchRegister`/`BatchJobUpdate`/`BuildRegister`/`BuildUpdate` variants,
    monotonic Raft-allocated ids that survive restart, deterministic register-time GC
    (retention window + cap-at-50, keyed on the request's clock), `/v1/build/track`
    leader-forwarding for follower builders.
  - [x] Bounded, retried dispatch and callbacks; validated idempotent reports (JOB3):
    dispatch retries then fails jobs honestly, callbacks retry with backoff, reports are
    transition-validated (forged status 400, unknown batch/job 404, conflicting terminal
    409, duplicate 200 no-double-count), unschedulable jobs are first-class terminal
    records in the batch summary.
  - [x] Pull backstop + restart resumption (JOB3/JOB4): leader-side per-batch watcher polls
    assigned nodes; lost-callback batch still terminates; a restarted leader respawns the
    watcher from the durable record on status reads; a Running build with no live runner is
    rewritten to an honest durable failure.
  - [x] Build context transfer + builder retry (D18/JOB5): confirmed the residual (bare
    `_buildcontext` blobs are uncatalogued and never replicate), closed it by copying the
    blob to the chosen builder's registry before dispatch (addresses from membership +
    config only); delegation walks all capable peers before an honest 502.
  - [x] Signing as part of terminal success (JOB7): builds sign keylessly with a
    Workload-CA-issued certificate (verified against the root CA before attaching);
    `AttachSignature` for an unknown digest is Refused, not a no-op; under
    `require_signatures` a signing failure fails the build with the reason.
  - [x] Deterministic allocation (old Low): `BTreeMap` profile groups + name-sorted input,
    pinned by a same-input-same-plan test.
  - [x] Bounded CLI polling (JOB6 residue): `relish build --timeout` and
    `relish batch-status --wait --timeout` with Ctrl-C handling; timeout exits non-zero
    with the last known state.
  - [x] Registry chunked-upload PATCH/PUT responses now carry the spec-required
    `Location` header — found by the Lima buildah gate: containers/image 5.29
    (buildah 1.33) reads it strictly, so every real `buildah push` against Pickle
    failed (pre-existing on main; the gated e2e build now passes in the VM).

### 12b.3 — Secure every boundary

- [x] **API authorisation and Brioche** — enforce app/namespace scopes in reusable
  extractors; cover every mutation endpoint; refuse unsafe non-loopback bootstrap; replace
  the shared cluster-Admin service token with route-limited per-node capabilities; index and
  verify one Argon2 token in spawn_blocking under a concurrency bound; use real dashboard/node
  state and escape each HTML/attribute context including apostrophes (H4, D8, AUTH1-AUTH5, AUTH7-AUTH8).
  - [x] Authenticated read-only browser sessions + `/`+`/ui/*` route lockdown; env values
    masked (Stage 5, PR #77 — closes H6/AUTH6).
  - [x] Scope enforcement (AUTH1/H4) via `authorize_scoped` on apply/stop/exec/rollback/snapshot;
    fail-closed non-loopback bootstrap refusal at bind time (AUTH3); snapshot-mutate + rollback
    now Deployer-gated (AUTH2); the `__system` service principal refused on token/secret/sign/
    chaos/fault via `authorize_user` while node fan-out keeps its System routes (AUTH4);
    off-lock bounded Argon2 with a format short-circuit + `spawn_blocking` semaphore (AUTH5);
    dashboard/node fragments read the live gossip membership (AUTH7); `escape_html` now escapes
    `'` for single-quoted chart attributes (AUTH8).
- [x] **Node PKI, join and mTLS** — the listener/identity half landed with Stage 5; the join
  hardening and peer binding below landed with 12b.3 (registry auth/TLS moves to the Pickle
  storage theme).
  - [x] Persist the bootstrap node leaf/key/Node CA/root CA + config paths; deliver a bundle
    to joiners with TOFU fingerprint (Stage 5 — PKI1).
  - [x] Handshake-true, CRL-aware mTLS builders with live-update tests, Node-CA-pinned client
    verifier, and **mTLS wired onto the Raft, reporting and agent-API listeners** behind the
    `require_mtls` mode matrix (Stage 5 — PKI1/PKI2, C5(b) listeners, L17 peer-CRL refresh).
  - [x] CSR-based join keeping the key on the joiner: the joiner generates its keypair + CSR,
    the issuer signs it and returns a key-free bundle, the joiner assembles its identity from
    its own key (12b.3 — PKI4).
  - [x] Atomic consume-and-serial join: one Raft `ConsumeJoinTokenForIssue` entry marks the
    token consumed and allocates the serial together, so racing joiners with one token issue
    exactly one cert; the issuer DN is derived from the stored Node CA (12b.3 — PKI5).
  - [x] Expected-peer node-id binding: node certs carry a `spiffe://reliaburger/node/<id>` URI
    SAN, and the Raft connector builds a per-target verifier that asserts the peer leaf's
    node-id SAN matches the node it is dialling (12b.3 — PKI3).
  - [x] Transactional bundle install/validation: the identity bundle writes a commit marker
    last and `load` refuses a marker-less (partial) or non-chaining bundle (12b.3 — PKI9).
  - [x] Per-connection handshake/read deadlines on the Raft and reporting accept paths, on top
    of the existing frame bounds, so a stalled peer is dropped (12b.3 — CP11).
  - [x] Constant-time join-token comparison (12b.3 — PKI10 slice; IMG1-IMG3 remain in the
    image-trust theme).
- [x] **Image trust policy** — distribute authoritative trust state to workers and
  standalone mode and fail closed; validate every intermediate, time, revocation, EKU,
  issuer and SPIFFE/OIDC identity; enforce issuer/audience/algorithm/kid; use a persistent
  configured or workload key for build signing and require the signature write to apply.
  Use constant-time join-token comparison (IMG1-IMG3, PKI10, old keyless/OIDC Lows).
  - [x] Fail-closed image verification: a node with `require_signatures` set that has no
    council handle (a standalone node) can't reach the catalogue or root CA, so an image
    deploy is *refused* rather than skipped; a process workload (no image) still passes. No
    cluster-state distribution needed — clustered nodes already replicate the trust anchors
    via the CouncilNode, so only genuine standalone mode hits the refusal (12b.3 — IMG2).
  - [x] Full keyless chain validation: `verify_keyless` now walks every adjacent link
    (signature + issuer-DN binding) to the trust anchor, checks each cert's validity at an
    injected clock, requires the leaf's code-signing EKU, and binds the leaf's SPIFFE URI SAN
    to the identity the signature declares — reusing `cert::validate_chain_at`,
    `check_code_signing_eku`, `subject_uri_sans` and the existing CRL check (12b.3 — IMG3).
  - [x] OIDC constraint hardening: `verify_jwt_with_constraints` checks the JOSE `alg`/`kid`
    header *before* trusting the signature, then enforces `iss`, `aud`, and `iat` skew/age
    bounds on top of the signature + `exp` (defence-in-depth: no production caller yet)
    (12b.3 — PKI10 OIDC half).
  - [x] Persistent build signer: one code-signing identity per namespace
    (`spiffe://…/job/build-signer`) provisioned via the council and cached, reused across
    builds instead of a fresh ephemeral CSR each time; its leaf carries the code-signing EKU
    so a signature it produces passes the tightened deploy-time check (build-sign → deploy-
    verify round-trip). The `require_signatures` build-failure gate is preserved: no
    policy-trusted signature, no `Completed` (12b.3 — JOB7 follow-up).
  - [x] Constant-time join-token comparison + CSR join landed in Theme 2 (#100).

> **12b.3 "Secure every boundary" is complete** — all three themes done: API
> authorisation and Brioche (#99); Node PKI, join and mTLS (#100); Image trust policy.

### 12b.4 — Finish the data plane

- [x] **Global namespaced service catalogue and DNS** — introduce ServiceId { namespace,
  name } through Onion, DNS, eBPF, Wrapper, firewall and APIs; replicate healthy endpoints;
  allocate VIP/container IPs with collision, exhaustion and release handling; bind DNS on
  a container-reachable address and fail startup/deploy if unavailable. Add DNS TCP and
  source ACLs, and either provide a portable non-eBPF VIP path or reject that configuration
  up front (H3/codex-M1, D3/D5/D6/D7-routes, old M5/M6).
  - [x] **PR 1 — namespaced local service identity.** `ServiceId { namespace, name }`
    (canonical `{namespace}__{name}`, aligned with `InstanceIdentity`) re-keys the
    `ServiceMap` and all ~64 agent call sites; VIP derives from the qualified identity with
    collision-probe + release-on-stop; DNS resolves `<app>.<namespace>.internal` (bare name
    → default namespace), adds a TCP listener and a source ACL (public sources REFUSED), and
    fails closed on bind; Wrapper ingress keyed by `(namespace, app)`; the eBPF `(vip,port)`
    key is unchanged (namespaced VIP keeps it collision-free, no recompile); `[dns]` without
    `[ebpf]` rejected at config validation; whitepaper §10 D6 doc-drift fixed. Tests-first:
    distinct VIPs + independent resolve, VIP release, namespaced routes, DNS ACL refusal,
    fail-closed bind, config reject. `make ci` green.
  - [x] **PR 2 — replicated global endpoint catalogue.** New `EndpointCatalog`
    (`onion/catalog.rs`) — namespaced services → VIP + cluster-wide backends, cluster-wide
    deterministic collision-free VIP allocation. Added a distinct, additive, serde-default
    `endpoint_catalog` field to `DesiredState` (separate from Pickle's `manifest_catalog`)
    plus a `RaftRequest::PublishEndpoints` variant + apply (wholesale replace); pre-theme
    snapshots load cleanly (fixture test). The leader builds the catalogue from aggregated
    health reports (namespace/app/port/health) + gossip node IPs and publishes it via Raft
    only when it changes. Every node overlays it onto its local service map
    (`ServiceMap::with_cluster_catalog`, non-mutating merge) so DNS + ingress resolve
    services on other nodes with no DNS/routing changes; workers get it piggybacked on the
    `/v1/placements` poll. Tests-first: catalogue rebuild/collision/JSON, non-mutating merge
    (remote-only + merge-into-local + dedupe), leader build across nodes, apply + snapshot
    fixture; gated (`RELIABURGER_CLUSTER_TESTS=1`) cross-node resolve that survives a leader
    change. `make ci` green.
- [x] **Ingress transport and draining** — carry per-route TLS mode into routing; implement
  ACME/cluster-CA or the documented explicit certificate contract; redirect HTTP except
  challenges; stream request/response bodies with limits/backpressure; hold connection
  permits through TLS/WebSocket lifetime; add handshake/idle timeouts. Replace untrusted
  forwarded headers, use boundary-correct deterministic route/rate keys, parse IPv6 Host
  correctly and wire deployment draining (D7/D10, ING1-ING5).
  - [x] **ING1/D7 — per-route TLS mode + HTTP→HTTPS redirect.** New `TlsMode` enum
    (`Disabled`/`Cluster`/`Explicit`) parsed from `IngressSpec.tls`, carried into
    `PathRoute`. Unsupported modes (`auto`/`acme`) are rejected at routing rebuild rather
    than silently served plaintext; a TLS-marked route reached over plain HTTP gets a 308
    to HTTPS (ACME challenge paths excepted). Cluster-CA path: `tls::issue_ingress_cert`
    issues a server-auth ingress cert from the Sesame Ingress CA (reuses the existing CA
    hierarchy, no parallel scheme). Explicit certs keep the existing disk-file contract.
  - [x] **ING2 — connection permits through the full lifetime.** TLS handshakes are bounded
    by a semaphore and a per-handshake deadline (`tokio::time::timeout`), so a slow-handshake
    flood can't exhaust tasks. The proxy holds a connection permit for the whole request; for
    a WebSocket the permit (and drain guard) move into the splice task and release when the
    splice closes, not at the 101.
  - [x] **ING3 — streamed bodies with limits.** Response bodies stream via
    `Body::from_stream(resp.bytes_stream())` instead of buffering whole (SSE/gRPC/large
    downloads flow with backpressure). Request bodies keep a configurable
    `max_request_body_bytes` cap; over-cap requests get 413.
  - [x] **ING4/D10 — WebSocket drain.** `increment_websocket`/`decrement_websocket` wired at
    the 101 and splice end; `check_completions` now waits for both the HTTP and WebSocket
    counts to reach zero (or the deadline), so a rolling retire honours `drain_timeout` for a
    live WebSocket, extending #97's HTTP drain.
  - [x] **ING5 — trusted forwarded headers + correct route/rate keys.** Client
    `X-Forwarded-For`/`-Proto`/`Forwarded` are stripped and replaced with the proxy's real
    view; route matching is segment-boundary correct (`/api` no longer matches `/apievil`)
    with deterministic tie-break ordering; rate buckets key on `(host, path, client IP)` not
    the IP alone; zero/overflow rates are rejected at rebuild; IPv6 Host (`[::1]:port`) is
    bracket-aware. `make ci` green.
  - **12b.4 tier complete** — all three themes (namespaced catalogue, Pickle durability,
    ingress transport) done. The data plane is finished.
- [x] **Pickle storage and replication durability** — stream/hash off the async runtime;
  add authenticated principal/repository quotas and upload expiry; write unique temp files,
  fsync and rename; reverify cache and isolate immutable rootfs generations; constrain
  redirects to same-origin relative paths and bound peer reads. Recheck references at
  deletion, report degraded redundancy honestly, define acknowledged push semantics and
  support multi-segment repositories (D11, REG2/REG4-REG8, old cache/upload Lows).
  - [x] REG5 — durable persistence: `write_blob`/catalogue `persist_to`/`complete_upload`
    write a unique temp, fsync the file, rename, fsync the parent dir; cache hits and the
    deploy path revalidate the digest (`revalidate_blob`); rootfs unpacks into an immutable
    content-addressed `gen-{hash}` directory so a tag move can't clobber a live container.
  - [x] REG2 — one authoritative catalogue: `manifest_get`/`tags_list` read
    `catalog_snapshot()` (the council's Raft catalogue when clustered), so a peer's commit is
    visible everywhere without waiting for a heal tick.
  - [x] REG4 — auth, TLS, quotas, limits: registry writes reuse `sesame::auth` (bearer +
    service token; Deployer role); the listener serves over TLS via `build_api_server_config`
    when the node has an mTLS identity, peers address `https://`; per-repository/registry byte
    quotas; whole-blob hashing moved to `spawn_blocking`; upload-session TTL + sweep (REG8).
  - [x] REG6 — same-origin redirects + bounded reads: `resolve_same_origin_location` refuses
    an absolute/cross-host/protocol-relative upload `location`; peer body reads are capped and
    wrapped in the timeout.
  - [x] REG7/D11 — honest push semantics: `record_commit` returns a `CommitOutcome`; the push
    returns `201`+`OCI-Replication: pending` (authoritative, replication owed) or
    `202`+`raft-uncommitted`; GC rechecks the full catalogue reference set before deletion.
  - [x] REG8 — multi-segment repositories: the router captures the whole `/v2/*rest` path and
    splits the OCI operation suffix, recovering `team/app` names end to end; upload sessions
    expire.

### 12b.5 — Make automation and observability truthful

- [x] **Metrics, logs and object storage** — parameterise every DataFusion query; add
  typed/bounded raw-log access; push predicates into Parquet and move flush/compaction off
  async locks. Persist/prune epoch-aligned idempotent rollups; collect per-app/scraped
  metrics; distinguish stale telemetry in alerts; URL-encode fan-out and return partial
  failures; deduplicate by stable node/instance/event identity. Implement real S3/GCS log
  export, one Bun-owned checkpoint, durable object IDs, provider webhook payloads and final
  shutdown flush; remove/consolidate legacy Ketchup calendar/config (codex-M4/D13,
  old M3/M4/M19/M20/M24/X8, OBS1-OBS8).
  - [x] Cluster rollup metric-name queries escaped via `escape_sql_literal`, closing the
    `/v1/metrics/rollup` SQL injection where `params.name` was interpolated raw (OBS1).
  - [x] **PR O1 — Metrics.** OBS1-remainder: `metrics_app_handler` escapes both `?name=` and
    the `namespace/app` path before interpolation (injection can't bypass the tenant/time
    predicate). OBS2 rollups: buffer bounded, windows epoch-aligned, ingest idempotent per
    `(node, window)` so a re-sent window doesn't double-count, and the flush counter recovers
    past existing files on restart (no clobber) with history read back from Parquet. OBS3:
    the collection loop now collects per-app process metrics (labelled `namespace/app`) from
    the agent's running instances, so autoscaling/dashboards have a signal. OBS4 alerts:
    stale telemetry no longer resolves a firing alert (value-not-boolean state machine),
    Slack (attachments) and PagerDuty (Events API v2) get provider-shaped payloads, and a
    zero `evaluation_interval_secs` is rejected at startup. OBS5/M3: flush drains under a
    brief lock then writes via `spawn_blocking` off the lock (queries proceed during a
    flush), and a corrupt Parquet file is skipped per-file rather than failing every query.
  - [x] **PR O2 — Logs + object storage.** OBS5: `/v1/logs/sql` goes through a bounded path
    (`query_sql_json_bounded`) — read-only `SELECT`/`WITH` only, `logs`-table-only session,
    outer-`LIMIT` row cap and a working-memory limit; a rejected query is a `400`. OBS6
    fan-out: `app`/`namespace` percent-encoded as path segments and query values through
    reqwest's encoder (a `grep` with `&`/`?` travels intact); node HTTP/JSON/task failures
    return a partial `FanOutResult` (which nodes failed) instead of silent empty; dedup keyed
    on stable `(node, timestamp, stream, line)` identity, so two replicas' identical lines
    survive while one node's retransmit collapses. OBS7/M20/X8: `export_logs` ships via
    `object_store::parse_url` (`s3://`/`gs://`/`file://`/bare path), one Bun-owned checkpoint
    shared with `relish logs-export` (`CHECKPOINT_FILENAME`), durable content-hash object ids
    so a filename reused after retention isn't skipped, and a final shutdown flush of both
    metrics and log buffers. OBS8/M24: the dead `KetchupStore` (+ its `index`/`json` modules
    and `logs.max_file_size_mb`) removed; `LogStore` is the sole live path. Real S3/GCS lives
    behind a named `#[ignore]` manual test, not a silent gate.
- [x] **GitOps convergence and webhook security** — expose a signature-validated HMAC
  webhook route with replay/rate controls; make diff namespace-aware and deterministic;
  apply every resource through the unified desired-state path; never advance last_applied
  after skip, partial result or failed write; keep jobs stable. Verify reused clone
  remote/branch, terminate Git options safely and wire coordinator/backoff semantics
  (D12, GIT1-GIT4, old Git/Jobs Lows).
  - [x] GIT2 namespaced convergence: the diff keys apps by full `AppId` (namespace + name),
    and the `resource_id` encodes `app.<namespace>/<name>`, so a `prod/web` removed from git
    deletes `prod/web` and never `default/web`. Two same-named apps in different namespaces
    converge independently (`diff.rs`, `runner.rs`).
  - [x] GIT2b jobs: dropped the phantom job `Add` the diff emitted every sync against an
    always-empty set. Jobs run to completion (not reconciled desired state), so the diff
    emits nothing for them — no re-add, no inflated `summary.added`.
  - [x] GIT3 public HMAC webhook: `WebhookValidator` wired into the live handler; the route
    is public (exempt from the 12b.3 bearer middleware — GitHub/GitLab can't send a bearer)
    but HMAC-gated in the handler. Validates `X-Hub-Signature-256` (GitHub) or `X-Gitlab-Token`
    (GitLab) over the raw body, rejects replayed delivery ids (401), and rate-limits (429).
    Fails closed (503) with no `[gitops] webhook_secret`. Constant-time compares throughout
    (`ring::hmac::verify`; explicit CT for the GitLab token).
  - [x] GIT4 git safety: `--`/`--end-of-options` terminators before every user-controlled
    ref/path/SHA (no option injection); reused clone verified against remote URL + branch
    and re-cloned on drift; deterministic sorted-path file merge with a duplicate-resource
    error instead of hash-order overwrite.
  - [x] GIT4 durable errors + backoff + coordinator: hard sync failures recorded in
    `SyncState` (`last_error`, `consecutive_failures`, `phase = Error`) so relish/UI see a
    broken sync, not just stderr; `backoff_delay` wired into the retry; `select_coordinator`
    wired to stamp the elected coordinator (complements leader-only sync — the leader still
    drives writes).
  - GIT1 (last_applied advances only on full success) already landed in #98; not redone.

### 12b.6 — Platform, upgrades and documentation

- [x] **Process workloads and platform capabilities** — pass [process_workloads] into the
  supervisor; default-deny host exec/script; enforce executable allowlists and mount
  isolation before runtime creation. Implement or explicitly reject rootless networking/
  resource limits, GPU detection/device isolation and unsupported Apple/runtime adoption;
  make gpu_enabled effective (H8/D15/D17, old M22/M23).
  - [x] D17/H8: `[process_workloads]` now reaches the supervisor. `bun.rs` calls
    `agent.set_process_config(...)` → `WorkloadSupervisor::set_process_config`, so the
    operator's allowlist governs host execution instead of the ignored constructor default.
  - [x] Default-DENY host exec/script: `ProcessWorkloadsConfig::is_binary_allowed` no longer
    treats an empty allowlist as all-allowed — an empty/absent list refuses every host
    binary. A `script` is host execution of `/bin/sh`, so the interpreter must be allowlisted
    too. Enforced in `WorkloadSupervisor::admit_process_workload` for both `deploy_app` and
    `deploy_job`, before anything is allocated; container (runc/apple) workloads are
    unaffected. Mount isolation is checked at admission: config asking for it on a non-Linux
    node is refused rather than run unprotected.
  - [x] D15 GPU: real `NvidiaGpuDetector` (probes `/dev/nvidia0`, parses `nvidia-smi
    --query-gpu`, falls back cleanly to none, OS-guarded). `gpu_enabled` made effective —
    the supervisor **rejects** a GPU-requesting workload when `gpu_enabled = false` or the
    detector found no device, instead of silently scheduling onto a node reporting zero GPUs.
    (OCI `/dev/nvidia*` device passthrough deferred; placement is now honest.)
  - [x] M22 rootless: **reject** path chosen (runc.rs is out of this seam). A rootless node
    refuses a workload declaring cpu/memory limits (rather than silently dropping them via
    `make_rootless`'s `resources = None`), with a clear error naming the fix. slirp4netns
    wiring remains deferred to the runc-create seam.
  - [x] Reserved resources: `reserved_cpu`/`reserved_memory` were already wired
    (`bun.rs::node_capacity` → `set_node_capacity`); confirmed, nothing dead to remove. GPU
    capacity is not yet threaded through the reporting protocol (orchestrate seam) — the
    supervisor gate makes `gpu_enabled` effective in the meantime.
  - Deferred (not this theme): unsupported Apple/runtime adoption (Self-upgrade seam).
- [x] **Smoker effects and cleanup** — stop returning success for no-op memory, disk,
  drain and node-kill faults; apply CPU stress to the target workload cgroup; make every
  persistent effect reversible on clear/expiry, including pause/resume. Acceptance measures
  each advertised effect and its removal (CHAOS1).
  - [x] Thread instance ns/app/ordinal into `apply_fault` (`target_instance_cgroups`) so
    resource faults target the workload's own cgroup, not a bare service name.
  - [x] CPU stress caps the target cgroup's `cpu.max` (was: burning Bun's own CPU); the
    saved quota is restored on clear/expiry.
  - [x] Memory pressure lowers the target cgroup's `memory.high` toward `memory.max` (was:
    a genuine no-op returning Ok); the saved soft limit is restored on clear/expiry. An
    `oom` request is rejected as irreversible (use a Kill fault instead).
  - [x] Disk-I/O throttle writes `io.max` on the target cgroup keyed by the volumes-dir
    device; the throttle is lifted on clear/expiry.
  - [x] Pause auto-resumes: the frozen PIDs are recorded and SIGCONT'd on clear/expiry
    (`expire_faults` and both clear paths), so a paused workload never stays frozen.
  - [x] Node drain / node kill return an honest rejection (a cluster-level operation, not a
    node-local fault) instead of a fake Ok; the real effect lands with the self-healing/
    upgrade themes that own the cluster machinery.
  - [x] `DnsNxdomain` acts in the userspace `.internal` resolver (was: wrote a `fault_dns_map`
    entry into a never-loaded eBPF object, so it did nothing on any config — caught by the
    12b.6 gate). The agent publishes the faulted-service set on a `watch` channel
    (`onion::dns::DnsFaultState`); the responder returns NXDOMAIN for a targeted service before
    the service-map lookup, and clear/expiry republish the smaller set so it reverses like every
    other fault. `requires_ebpf()` is now `false` for `DnsNxdomain`; the dead `fault_dns_map`
    (Rust + eBPF C) is removed. Portable tests drive the resolver and the full UDP path.
  - [x] Reversal state (`FaultReversal`) rides on the registry's `FaultRule` (runtime-only,
    `#[serde(skip)]`), captured at apply time via `record_reversal`/`get_mut`.
  - [x] Portable tests cover cpu-quota computation, drain/kill/oom rejection, pause
    auto-resume (real child + `waitpid`), off-Linux resource rejection and reversal
    round-trip; Linux cgroup-effect + reversal tests are `#[ignore]` under
    `make test-linux` (`cgroup_*`, `RELIABURGER_CGROUP_TESTS=1`).
- [x] **Self-upgrade convergence and adoption** — wire scheduler cordon; calculate quorum
  headroom from live voters; derive roles/addresses server-side; prove gossip rejoin and
  finish Apple/rootless adoption. Make progress/book describe the implemented in-place or
  leadership-transfer sequence consistently (D20, UPG1-UPG2).
  - [x] Scheduler cordon (UPG1, verified): `meat::filter::apply_upgrade_cordon` IS called
    from the leader scheduling loop (`cluster/orchestrate.rs`) against a `ClusterStateCache`
    fed from Raft — a node in `Directed`/`Verifying` takes no new placements. (The audit's
    "no caller" was stale.)
  - [x] Live-voter quorum headroom (UPG1): `upgrade::orchestrator::quorum` now counts LIVE
    reachable voters — configured voters cross-referenced against gossip `Alive` via the
    stable `raft_id_from_name` hash — not just configured ones. With one voter already dead,
    upgrading another that would drop live voters below quorum is refused
    (`live_quorum_headroom_ok`, unit-tested for the 3-voter one-dead and 5-voter two-dead
    boundaries).
  - [x] Server-derived roles/addresses (UPG2): `upgrade::plan::derive_upgrade_nodes` rebuilds
    each node's role (from the Raft voter set + current leader) and address (from gossip
    membership) server-side and validates the client's start request against it. A spoofed
    address or a claim that crosses the leader boundary is rejected; a worker↔council relabel
    among non-leaders is corrected to the authoritative role. The plan the orchestrator walks
    never depends on the client's claim.
  - [x] Gossip-rejoin verification (UPG2): a node reaches `Healthy` only when it is on the
    target version, HTTP-healthy AND back in the gossip mesh (`Alive`). One that comes up
    HTTP-healthy but never rejoins gossip is held (and times out → pauses the run), not
    counted done. Unit-tested both ways.
  - [x] Apple Container adoption (UPG2): `grill::apple::AppleContainerGrill::adopt` re-tracks
    a running Apple workload via `container inspect` (the VM survives bun's exec; the workload
    is not a bun child pid, so pid-liveness doesn't apply). An adopted Apple instance survives
    an exec-in-place swap instead of restarting; a removed container declines adoption. Parse
    logic unit-tested; end-to-end adoption gated behind `make test-apple`. (Rootless is a
    spec-modifier over runc, not a separate adoption path — n/a.) The 12b acceptance gate ran
    `make test-apple` on real Apple silicon and caught that the inspect parser had guessed the
    wrong JSON shape: the CLI returns a single-element array with a top-level lowercase `status`
    and the IPv4 address as CIDR under `networks[0].ipv4Address`. Parser corrected against the
    real output (fixtures replaced with the captured schema), so adoption now works on hardware.
  - [x] D20 prose reconciled: `docs/design/agent-bun.md` §5.5 and the Phase 14 lines below now
    describe the shipped authenticated-HTTP / `GET /v1/version` poll / leader-last-in-place
    implementation (no leadership transfer, same order on rollback), and the book chapter 14
    matches.
- [x] **Documentation and book truth pass** — corrected to the shipped reality:
  userspace DNS in whitepaper §10 and `discovery-onion.md` (the in-kernel `onion_dns`
  program is now clearly a decision-log note, not live behaviour); Grill drives
  runc/Apple/process directly, not containerd (`agent-bun.md` overview, dependency
  table, decision-log note); the Raft log/RPC/snapshot use self-describing JSON, so
  book ch.2a/10/14 no longer teach the stale bincode-index rule; GPU qualified to the
  real `/dev/nvidia0`+`nvidia-smi` detector with effective `gpu_enabled` and refusal
  (OCI device passthrough still deferred) in whitepaper §2, `agent-bun.md` §5.4 and
  book ch.1; batch dispatch/completion marked a Phase 12 deliverable in `scheduler-meat.md`
  §5.2 and whitepaper §5.2; recovery §§8.2–8.3 split into shipped (learning period,
  backup/restore, disk-pressure step-down) vs the pre-seeded-candidate architecture
  proposal; process-workload deny-by-default made explicit in whitepaper Q6. Stale
  progress annotations reconciled (DNS responder spawned, WebSocket proxy wired, Grill
  runtime). The headline test count already reads as a suite taxonomy (#106). X6 stays
  in Phase 13 (D6/D14-D15/D18/D20-D22).
- [x] **Phase 12b acceptance gate** — ran the full matrix on the merged programme
  (main at #118). Results:
  - [x] Sandbox suites (macOS host): `cargo fmt --check` clean; `cargo clippy
    --all-targets -- -D warnings` clean (full `--all-features` clippy is Linux-only —
    `aya`; it runs in CI); portable default suite **2628 passed / 39 skipped**;
    `--no-default-features` **2609 passed**; doctests (no rustdoc examples); in-process
    cluster (`RELIABURGER_CLUSTER_TESTS=1`: `cluster_failover`, `cluster_gossip`,
    `council_self_healing`, `council_disaster_recovery`, `placement`, `chaos`) **20 passed**
    — 8+ node leader-failover and council recovery; in-process upgrade
    (`RELIABURGER_UPGRADE_TESTS=1`: `self_upgrade`, `self_upgrade_cluster`) **8 passed**
    — failed-voter/gossip-rejoin/adoption; wall-clock acceptance (`make test-slow`)
    **4 passed / 29 skipped**. The mTLS/auth, registry push-GC-peer-pull, GitOps
    partial-failure and Smoker effect/clear checks run inside the portable + cluster
    suites above.
  - [x] Full CI on the merge commit: **all 11 test jobs green** — coverage floor
    (`--fail-under-lines 78.65`) held, 10k-member scale, fast/large benchmarks,
    multi-node cluster, single-node + cluster upgrade, wall-clock, portable Linux/macOS,
    privileged Linux.
  - [x] Linux platform gate: CI's **privileged Linux** job *is* `sudo -E … make test-linux`
    (eBPF/runc/netns/Btrfs/Buildah/cgroups) — **green** on the merge commit. (A local Lima
    run was redundant and only tripped a guest `~/.cargo` permission quirk, not code.)
  - [x] Apple platform gate: `make test-apple` **2/2 passed on real Apple silicon**
    (`apple_container_grill_creates_instance`, `adopt_re_tracks_a_running_apple_container`),
    run twice back-to-back (idempotent). The gate caught the inspect-schema bug fixed in
    #118 — a green unit test had hidden it.
  - Gate findings fixed before close: DnsNxdomain userspace no-op (#117) and Apple
    inspect-schema adoption failure (#118). This closes the entire Phase 12b review
    programme (12b.1–12b.6).

## Phase 13: Relish TUI

Implementation plan: [docs/plans/2026-07-06-plan-tui.md](plans/2026-07-06-plan-tui.md)

- [x] Full interactive terminal UI (ratatui + crossterm) — `src/relish/tui/`
  (app loop, input/keys, navigation, live event/log streams, terminal, theme/palette)
- [x] Dashboard, apps, nodes, jobs, events, logs, routes, search, help views
  (plus app-detail and snapshots) under `src/relish/tui/views/`
- [x] Book chapter 13: "A Room with a View"
- [x] All Phase 13 tests green (TUI unit tests in the portable suite)

## Phase 14: Self-Upgrade

> Detailed implementation plan: [2026-07-06-plan-self-upgrade.md](plans/2026-07-06-plan-self-upgrade.md)
> (12 commit-sized steps, decision log, type definitions, test inventory, gotchas checklist).

- [x] Rolling binary replacement (exec-in-place; workers → council → leader-last; state in Raft; `relish upgrade` command set). There is no leadership transfer: the leader upgrades itself last, in place (openraft 0.9 can't gracefully hand off against a live leader), and the returning process finishes the run via poll-first idempotency. Cordon, live-voter quorum headroom, server-derived identity and gossip-rejoin verification landed in 12b.6 (see above).
- [x] Dual-signature verification (embedded Ed25519 release key set + external operator key from node.toml; air-gapped `--binary` needs embedded only)
- [x] Automatic rollback on failure (crash-loop boot budget reverts the symlink; nodes refuse previously-reverted upgrade ids; leader pauses the run; `upgrade resume` retries with a fresh id)
- [x] Version retention and GC (keep newest `retain_versions`, rollback targets protected)
- [x] Workload adoption across the swap (ProcessGrill pidfile records + runc `state` adoption + Apple `container inspect` adoption; pid+start-time fingerprinting for pid-based runtimes, container liveness for Apple VMs; file-backed process logs). `[runc adoption unverified on Linux; Apple adoption gated behind make test-apple]`
- [x] Book chapter 14: "Changing the Tyres at Full Speed"
- [x] All Phase 14 tests green (unit tests in the portable suite; 5 single-node + 3 cluster real-binary integration tests are ignored by default and owned by the required `upgrade-node` and `upgrade-cluster` CI jobs). The jobs use nextest resource groups and no retries, so contention or convergence flakes remain visible.

The Phase 14 deferred seams are closed in 12b.6: scheduler cordoning is wired
against a live `ClusterStateCache`, quorum headroom counts live voters, node
roles/addresses are derived server-side from gossip + Raft, and post-upgrade
verification now requires gossip rejoin explicitly (see the "Self-upgrade
convergence and adoption" theme under 12b.6).

## Phase 15: Testing, Benchmarking & Diagnostics

> Detailed implementation plan: [2026-07-06-plan-chaos.md](plans/2026-07-06-plan-chaos.md)
> (15 commit-sized steps, test catalogue, data structures, acceptance runbook).

- [x] Test harness audit and suite taxonomy: nextest local/CI profiles, JUnit, timeouts,
  zero retries, serial resources and no-tests-selected failures; portable, wall-clock,
  privileged Linux, cluster, upgrade and manual Apple suites have explicit ownership
- [x] Truthful gating and cleanup: platform code uses `#[cfg]`; provisioned tests use
  reasoned `#[ignore]` plus failing preflight; duplicate/demo/field-access tests removed;
  Relish has black-box exit/stdout/stderr coverage
- [x] Deterministic async harness: fixed portable waits replaced by channels, watches,
  notifications, barriers or bounded predicates; ProcessGrill and TCP reporting own and
  cancel their child tasks; listeners use ephemeral ports and isolated temporary paths
- [x] Benchmark and coverage split: deterministic seeded gossip simulation shared by
  Criterion and scale acceptance; P2P correctness no longer has a host-speed threshold;
  combined default/no-default LLVM coverage publishes LCOV and HTML artefacts
- [x] Required CI and release validation: portable Linux/macOS, privileged Linux, cluster,
  node/cluster upgrade, coverage and three benchmark/scale jobs; release publication waits
  for the same reusable workflow. Apple Container remains a documented manual exception
- [ ] `relish test` command (built-in test runner, parallel, filtering, JSON output)
- [ ] `relish test --chaos` (integration tests + Smoker fault injection)
- [ ] `relish bench` (scheduler, eBPF, network, deploy, state reconstruction benchmarks)
- [ ] `relish wtf` (automated cluster health diagnosis)
- [ ] `relish trace` (end-to-end connectivity debugging)
- [x] Book chapter 15 test-harness and benchmarking foundations: Rust attributes, ignored
  versus compiled-out tests, deterministic async tests, nextest, benchmarks and coverage
- [ ] Complete chapter 15 with the built-in diagnostics commands (`relish test`, `wtf`,
  `trace`) when those commands land
- [ ] All Phase 15 tests green

## Phase 15a: Current-State Hardening

> Prioritised follow-up plan:
> [2026-07-18-plan-codebase-review-follow-up.md](plans/2026-07-18-plan-codebase-review-follow-up.md)
>
> This is a hardening gate before the unfinished Phase 15 diagnostic commands,
> not a declaration that Phase 15 is complete. The source review is
> [2026-07-17-review-codebase-current-state.md](plans/2026-07-17-review-codebase-current-state.md).

### High-value / must fix

- [ ] Enforce authentication on every non-loopback control-plane listener (H1 / SEC-1)
- [ ] Fail closed when a declared egress policy can't be enforced (H2 / SEC-3)
- [ ] Make `.internal` DNS reachable, supervised and schedulable (H3 / NET-1)
- [ ] Make generated clusters use mTLS by default (H4 / SEC-2)
- [ ] Replace the broken published first-run sequence with an executable one (H5 / DOC-1)
- [ ] Rerun the complete review matrix and close the high-value gate

### Medium-value

- [ ] Live subsystem readiness/capability evidence and supervised failure (M1)
- [ ] Executable examples, coherent cross-platform lint and advisory scanning (M2)
- [ ] Peer-reachable clustered registry defaults (M3)
- [ ] Configured workload trust domain (M4)
- [ ] Rootless proxy adoption across Bun replacement (M5)
- [ ] Real deployment operation state (M6)
- [ ] Explicit v1 ingress/TLS contract (M7)
- [ ] Corrected Phase 15 prerequisites and catalogue (M8)

### Optional

- [ ] Ownership-based module splits (O1)
- [ ] Library-backed DNS/duration parsing where compatibility tests justify it (O2)
- [ ] Public API doctests (O3)
- [ ] Production TC DNS fast-path evaluation if profiling justifies it (O4)
- [ ] Mechanical shipped/planned/experimental/historical documentation markers (O5)

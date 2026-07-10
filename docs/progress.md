# Implementation Progress

Single source of truth for what's done and what's next. Check off an item only when it compiles, passes tests, and is committed. See [roadmap.md](roadmap.md) for full details on each phase.

> **Review note (July 2026):** a full verification pass ([review-2026-07.md](plans/review-2026-07.md))
> found that many checked items are **library-only** — implemented and unit-tested, but never
> wired into the `bun`/`relish` binaries. Those are tagged **`[lib-only]`** below with their
> finding ID (e.g. `L7`, `C5`). `[x]` still means "code exists + tests pass"; `[lib-only]` means
> "not reachable from the running binary". Critical bugs in *wired* paths are tagged with their
> ID too (e.g. `C4`). See the review doc for `file:line` and the staged fix plan.

---

## Phase 1: Foundation

- [x] Cargo workspace setup (binary `bun`, library `reliaburger`, test fixtures)
- [x] TOML config parsing (App, Job, Secret, ConfigFile, Volume, Permission, Namespace)
- [x] Grill container runtime interface (containerd/runc, OCI extraction, ports, cgroups)
- [x] Bun agent core (process supervisor, health checks, restart logic, GPU detection) — **restart re-drive broken for apps on all runtimes (`H1`); GPU detector is a stub (StubGpuDetector)**
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
- [x] Rootless runc (user namespaces, UID/GID mapping, rootless cgroups v2, no-sudo containers) — **`M22` resource limits silently dropped (no systemd-run); slirp4netns unwired → empty netns, no connectivity**
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
  - [x] Userspace DNS responder for `.internal` queries (replaces infeasible in-kernel DNS synthesis) — **`[lib-only]` `L9` `run_dns_responder` never spawned; see `M8` fragility**
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

- [x] Smoker fault injection (safety rails, fault registry, process/resource/node faults, eBPF network fault types + maps, scripted scenarios, chaos test suite) — wired in Stage 4 W11: `InjectFault` builds a live `SafetyContext` and calls `evaluate_safety`; approved faults are applied for real (Kill/Pause/Resume via PIDs, CpuStress via burn loops, memory/disk-IO via Linux cgroups); network faults rejected honestly without eBPF; partitions drive real transport blocklists. eBPF network-fault *enforcement* (delay/drop/dns/bandwidth) still lands with W12
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
- [x] WebSocket upgrade proxying in Wrapper ingress (detection, dispatch, close frame, draining) — **`[lib-only]` `L7` proxy never runs; handshake stub drops the backend stream and omits `Sec-WebSocket-Accept`**
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
- [x] Log export to S3/GCS (scheduled Parquet, `relish logs-search` for remote SQL) — **`M20` filesystem-only; `object_store` has only the `fs` feature, an `s3://` dest is treated as a local dir; `X8` `logs-export` races the agent's checkpoint**
- [x] Cross-node log queries via Raft (leader fan-out, merge-sort) — **`[lib-only]` production always takes the single-node path (`ApiState.council`/`membership` are `None`); direct HTTP, not "via Raft"; `M4` dedup flaw**
- [x] Book chapter 11: "Eyes Everywhere"
- [x] All Phase 11 tests green (1595 tests)

## Phase 11b: Review & Tying the Loose Ends

The July 2026 verification pass ([review-2026-07.md](plans/review-2026-07.md)) found that the build
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
- [ ] `C5(b)` mTLS on the Raft RPC and agent API listeners (needs node-identity on-disk persistence — certs are never persisted and joiners never receive theirs)
- [ ] `L17` (CRL) Enforce CRL / cert-revocation checks in `verify_keyless` (naturally rides with mTLS peer certs)
- [ ] Brioche UI / dashboard auth (`/`, `/ui/*` left public in Stage 3b auth)

### Stage 4 — Wire the remaining library-only subsystems (one at a time, binary-driven test each)

Implementation plan: [docs/plans/wiring-plan-2026-07.md](plans/wiring-plan-2026-07.md)

- [x] `L1` Scheduler → placement → remote dispatch: `relish apply` under `--cluster` commits `AppSpec`s to Raft (followers forward to the leader); a leader-only scheduler places replicas and commits `SchedulingDecision`s; every node polls `/v1/placements/{node}` and reconciles its instances (idempotent). `H8` fixed (spread weight 60 > bin-pack 50; test now asserts distinct nodes). Flushed out a latent bug: durable Raft log + council TCP RPC used bincode, which can't drive the config types' `deserialize_any` — both switched to self-describing JSON (matching the snapshot). Binary-driven test in `tests/placement.rs`
- [x] `L2` / `M16` / `X3` — `M16`: orchestrator no longer leaks a failed step's own half-started instance (regression test asserts it's stopped). `X3`: `relish rollback` actually rolls back — deploy history now carries the full `AppSpec` (every path records it, including the first deploy), `POST /v1/rollback/{app}/{ns}` redeploys the previous successful spec via the apply path (Raft in cluster mode). Note: cluster-wide *staged* rollout (max_unavailable gating across nodes) rides on the W6 desired-state reconciler and the per-node rolling redeploy; the imperative `DeployOrchestrator` stays library-side (correct + unit-tested) rather than duplicated as a parallel cluster driver
- [x] `L3` Autoscale loop wired: leader-only task drives the tested pure functions (`evaluate`/`AutoscaleTracker`/`AutoscaleConfig::from_spec` — the library's sync `app_provider` closure can't read async Raft/rollup state), reads each `[autoscale]` app's metric from the rollup store, and commits `AutoscaleOverride` to Raft. The scheduler now targets *effective* replicas (override ∨ spec), so a scale flows through the same placement→reconcile path as apply. End-to-end test: high metric → override → grows to `max`
- [x] `L4` State reconstruction wired into the leader scheduler loop: on the leadership edge it calls `on_leader_elected`, runs a learning period (feeding reports through `on_report_received`/`check_timeout`), and **gates scheduling** until phase == Active — so a fresh leader never re-places apps that are running but haven't reported yet. `MissingApp`/`ExtraApp` corrections are realised by the loop's ordinary placement reconciliation; `UnknownNode` exclusion was deliberately dropped (it blacklisted slow-reporting nodes and caused churn). `[reconstruction]` config now read (dead-config cleared)
- [x] `L6` / `L11` Reporting + rollups wired: `RollupWorker` spawned per node, aggregator gets a real rollup store, `/v1/metrics/cluster` serves from it; StateReports carry real capacity (`[resources]` now read) and requested-resource usage. Flat-star kept by design (tree deferred, see ch. 11); fixed a latent DataFusion overflow (`unwrap_or(u64::MAX)` time ranges, 4 handlers)
- [x] `L7` Bind the Wrapper ingress listener — `[ingress]` node-config section (off by default), HTTP + HTTPS listeners (self-signed or disk certs), per-client rate limiting wired into the proxy path, WebSocket pass-through; drain-on-deploy integration lands with `L2` (W7)
- [x] `L8` / `L9` Load the Onion eBPF programs in production; start the DNS responder (fix `M8` fragility) — **`L9`+`M8` done**: `[dns]` config section (off by default), responder spawned from bun, full hardening (recv errors non-fatal, per-query spawned forwards behind a semaphore, connected sockets + transaction-ID checks, NXDOMAIN for unmatched `.internal` with no upstream leak, QTYPE honoured, SERVFAIL on dead upstream), runc containers get `resolv.conf` pointed at the responder. **`L8` done**: `[ebpf]` config section (off by default; `program_dir` defaults to the build-time `OUT_DIR` baked in via `build.rs` `RELIABURGER_BPF_DIR`, so dev/Lima builds self-locate their `.bpf.o`), `bun` loads + attaches `OnionEbpf` at startup (load failure logs and continues without enforcement; non-`ebpf` builds warn that enforcement is off). Verified in the `reliaburger-test` Lima VM: `cargo build --features ebpf` compiles the objects and all 9 `tests/ebpf.rs` integration tests pass (load/attach, backend-map read/write/remove, connect→VIP rewrite, no-backend deny `EPERM`, non-VIP passthrough, `.internal` DNS). Not covered by `make ci` (needs root + kernel 5.7+ + cgroup v2)
  - **eBPF production enforcement now complete** (Phase 11b follow-up, P0–P3): the agent syncs its live service map into the kernel `backend_map` at every mutation (`agent_deploy_populates_backend_map`), the fault-map writers write real `fault_connect_map`/`fault_dns_map`/`fault_bw_map` entries with CLOCK_MONOTONIC expiry that matches the kernel (`agent_drop_fault_refuses_vip_with_eperm`), and egress allowlists re-resolve periodically. All verified in the Lima VM.
- [x] `L10` / `M2` Pickle wired: catalog persists to disk + loads at boot; pushes record real raft-id holders and propose to Raft on council nodes (worker proposal forwarding lands with W6); leader replication loop keeps layers at `[images] redundancy`; scheduled two-phase GC — nominate → Raft-arbitrated approval (`CouncilResponse::GcApproved`) → delete, with an orphan grace window for in-flight pushes. `X1` fixed: `relish build` targets the registry port, `/v1/build` executes buildah for real (honest 501 without it)
- [x] `L13` / `H12` GitOps wired: new `src/lettuce/runner.rs` spawns a leader-only sync loop (clone → poll/webhook → `execute_sync` in `spawn_blocking` → apply changes as `AppSpec`/`AppDelete` Raft writes). Webhook endpoint gets a real channel (was unconditional 503); `[gitops]` config now read. `H12`: `is_key_trusted` no longer falls through to `true` — a valid signature from an unlisted key is rejected. Fixed a latent first-sync bug (a fresh clone has nothing to fetch but nothing applied either → now syncs when HEAD ≠ last-applied). Integration tests in `tests/gitops.rs` (real git repo → Raft; webhook triggers sync)
- [x] `L14` / `L15` Real Smoker fault injection + chaos transport blocklists: `InjectFault` now builds a live `SafetyContext` (council size + alive nodes from Raft/membership, replica counts from the supervisor, active node-faults from the registry — no more hardcoded zeros) and runs `evaluate_safety`; approved faults are actually applied (Kill/Pause/Resume via real PIDs, CpuStress via `spawn_blocking` burn loops, memory/disk-IO via cgroups on Linux). Network faults (delay/drop/dns/bandwidth) are **rejected honestly** without eBPF, and **injected for real** when the eBPF data path is loaded (P2: `write_fault_bpf_entry` writes `fault_connect_map`/`fault_dns_map`/`fault_bw_map`, expiry on CLOCK_MONOTONIC to match the kernel; `agent_drop_fault_refuses_vip_with_eperm`). `L15`: partitions populate the real gossip + Raft transport blocklists (both directions), so an isolated node is marked Dead by SWIM and rejoins on heal. Integration tests drive the binary path: `kill_fault_actually_kills_the_instance`, `network_fault_without_ebpf_is_rejected_not_faked` (`tests/integration.rs`); `partition_isolates_a_node_for_real`, `fault_injection_rejected_when_quorum_at_risk` (`tests/placement.rs`)
- [x] `L16` Program egress allowlists — an app's `[egress] allow` list is now enforced in the kernel: on instance start the agent resolves the instance's cgroup id (from `/proc/<pid>/cgroup` → cgroup v2 dir inode), resolves the allowlist (DNS off the event loop via `spawn_blocking`), writes the `egress_map` entries and flips `egress_enabled_map` for that cgroup; on stop it lifts enforcement. A rate-limited event-loop task re-resolves each allowlist and reprograms the delta as DNS changes (P3: `egress_diff` + `re_resolve_egress_async`). Non-eBPF/non-Linux builds warn that egress is unenforced (default-deny is eBPF-only, per D5). Verified in the Lima VM: `egress_denied_by_default_allowed_when_listed` (`tests/ebpf.rs`) loads the real program, allows one destination for the test's own cgroup, and asserts the listed destination connects while an unlisted one is denied with `EPERM`
- [x] `M17` K8s import fidelity (`command`/`args` concatenated, `env.valueFrom` warned not dropped, namespace preserved, same-name-two-namespaces no longer overwrites)
- [x] `H11` Fix `relish fmt` for nested-table configs — recursive section emission + a round-trip guard that refuses to write output that re-parses differently
- [x] `X1`/`X3`/`X4`/`X5`/`X6` CLI mismatches: `X1` (build → registry port + real buildah execution), `X4` (logs `--grep`/`--since`/`--json-field` wired, server + client side) and `X5` (unreachable agent exits non-zero; explicit `--dry-run` flag added) done; `X3` rollback done (W7); `X6` no-args TUI is out of Stage 4 scope by design → [tui-plan-2026-07.md](plans/tui-plan-2026-07.md)

### Throughout

- [x] Fix the misleading tests — `L15` "worker isolation" (was a no-op) replaced with `chaos_isolated_member_misses_writes_until_healed`, which really partitions a council member and asserts the isolated node misses writes until healed. `H1` restart tests now assert real post-restart behaviour, not just a counter bump: `health_check_triggers_restart` checks the instance reached a live re-created state (`running`/`health-wait`/`unhealthy`, never stuck in `Preparing`), and `job_failed_retries_then_fails` asserts the terminal `failed` state after retries exhaust
- [x] Remove dead config or wire it — wired during Stage 4: `[resources]` (W4), `[reconstruction]` (W9), `[gitops]` (W10), `[images]` (W5), `[metrics]` (W4), node `labels` (W6), new `[ingress]`/`[dns]`/`[ebpf]` (W2/W3/W12). Still genuinely dead and **documented, not silently dropped** (out of Stage 4 scope): `[process_workloads]` (M23), `[logs] max_file_size_mb`. `[storage] volumes` (M21) was wired in Phase 12 E0.
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

### Beyond Phase 11b — deferred & un-staged review items

The [July 2026 review](plans/review-2026-07.md) staged a specific remediation plan
(Stages 0–4). **Every item in that plan is now done** — including the eBPF
production-enforcement follow-ups (P0–P3) and three security Mediums pulled
forward (P4: `M25`, `M1`, `M18`). The findings below were **never part of the
review's Stages 0–4** (or were explicitly deferred). They are recorded here as
the authoritative post-Phase-11b backlog — nothing silently dropped.

**Deferred by design (were carved out of their stage):**
- `C5(b)` — mTLS on the Raft RPC and agent API listeners (Stage 3b deferral).
- `L17` — CRL enforcement at connect time (image-signature verification is done; Stage 3b deferral).
- `X6` — the no-args `relish` TUI → [tui-plan-2026-07.md](plans/tui-plan-2026-07.md).

**Never-staged Mediums (still open, confirmed in code):**
- `M3` — whole-blob `std::fs` + SHA-256 run on the async runtime (no `spawn_blocking`); large pushes stall the event loop.
- `M4` — log/metric dedup only removes *adjacent* duplicates, so cross-node dupes survive.
- `M5` — VIP collisions unchecked (SipHash into 65,534 slots; ~50% odds near ~300 apps).
- `M6` — service map keyed by app name only; the same name in two namespaces collides.
- `M7` — a crashed app with no health check stays `Running` forever (only jobs are exit-monitored).
- `M19` — alert webhooks build only a generic payload; Slack/PagerDuty formats are never applied.
- `M20` — log export is filesystem-only (`object_store` has only the `fs` feature); an `s3://`/`gs://` target is treated as a local dir.
- `M21` — managed volumes: mount entries generated but the host dir is never created (`[storage] volumes` dead config).
- `M22` — rootless: resource limits silently dropped; slirp4netns has no callers (empty netns).
- `M23` — process-workload allowlist + `mount_isolation` never enforced (`[process_workloads]` dead config).
- `M24` — `KetchupStore::today()` calendar math can emit month 13 (dead path today).
- `X8` — `relish logs-export` copies files directly, racing the agent's export checkpoint (never contacts the agent).

**All ~24 Low findings** remain open (review §Low): container blob-cache re-verify, `parse_num` overflow, weak `raft_id_from_name` djb2 hash, non-constant-time join-token compare, `verify_jwt` skips `aud`/`iss`, keyless verify ignores cert validity/SPIFFE, `manifest_put` doesn't verify referenced blobs, upload sessions never expire, fan-out swallows node failures / doesn't URL-encode params, IPv6 Host-header mangling, git arg-injection (`--` separator), diff engine re-adds every job per sync, and the rest. None were staged for Phase 11b.

## Phase 12: Optimisations

> Detailed implementation plan: [optimisations-plan-2026-07.md](plans/optimisations-plan-2026-07.md)
> (revised 2026-07-09 after the Stage 4 wiring merge: slice B — Pickle catalog via Raft,
> replication and two-phase GC — landed as-built in #71; 14 remaining implementation steps,
> refreshed ground truth, config/endpoint/test inventories, Lima acceptance runbook).

- [ ] Wire `SubmitBatch` into the agent (resolve job specs → `schedule_batch` over cluster capacity → dispatch → track completion via `BatchTracker`, `/v1/batch/{id}` status)
- [ ] Wire `SubmitBuild` into the agent — async build tracking, builder selection, signing (the synchronous local-only handler landed with Stage 4)
- [ ] Switch port mapping from nftables rules to nftables maps (O(1) lookup at scale)
- [x] Managed-volume wiring (E0, fixes review `M21`): the agent creates managed volume host dirs (loop-mounted when sized, Linux root) in `spawn_blocking` before spec generation, failing the deploy closed; `[storage] volumes` config wired via `set_volumes_dir`; **no deletion on Stop** (rebalances/upgrades send Stop; deleting would destroy data — explicit cleanup is future work)
- [x] Heal-loop hardening (B5): `pickle::replication::heal_tick` extracted from the bun binary (testable; `cluster::identity::pickle_peers` shared helper), rarest-first ordering + 10-manifest per-tick cap, leader-pull-first (non-leader pushes now gain redundancy), roadmap auto-heal integration test + 2 more, loopback `registry_bind` startup warning (registry has no auth/TLS — keep firewalled)
- [x] P2P multi-source image downloads — pure `pickle::p2p::plan_downloads` planner (rarest-first, least-loaded balancing, dedup, skip-local; proptested over arbitrary topologies) + bounded-`JoinSet` executor with alternate-holder retry, wired into `ImageStore::pull_and_unpack` via a late-injected `ClusterImageSource` (which also fixes cluster-pushed HTTP-only images being undeployable on other nodes — the external client is HTTPS-only); catalog-known images never fall back to external registries; 100MB/5-layer peer pull verified < 5s
- [x] Pull-through cache full wiring (upstream → Pickle → Raft) — `pickle::upstream` (pure `decide()` on `cache_recheck_secs`, HEAD-compare refresh, `UpstreamRegistry` trait over oci-distribution + counting mock, env-resolved `external_registries` credentials) + `ClusterSource::ensure_external_image` fill path (serialised fills with post-lock recheck; stale-serving when upstream is down; commits under `cache/<host>/<repo>` with holders={self}, heal loop replicates); `cache/` repos exempt from `require_signatures` by construction (pinned by test); peer blob-transfer URLs flatten multi-segment names (single-segment registry routes; blobs are content-addressed)
- [x] Volume snapshots (CoW, scheduled jobs, S3/GCS upload) — E3 adds `bun::snapshot_worker`: `[storage.snapshots] { interval_secs, retain, upload_url }` interval loop (cron-expression parser deliberately rejected), pure `prune_plan`, tar.gz in `spawn_blocking`, upload via `object_store` (`file://`/`s3://`/`gs://` — aws+gcp features enabled); the per-snapshot `uploaded` flag is checkpoint, retry policy, and audit column at once; sweeps report per-app failures without aborting. E2: `grill::snapshot` (Btrfs-only, read-only `-r` snapshots under `.snapshots/`, meta.json sidecars, injected-clock naming); restore = delete live + writable snapshot back, refused by the agent while the app has non-terminal instances (409); no-`--volume` snapshots every provisioned volume (sidecar discovery — works for stopped apps); 4 `/v1/snapshots` routes + `relish snapshot create|list|restore|delete`; roadmap create/corrupt/restore test on the Lima loopback-btrfs rig. Scheduled jobs + object-store upload land in E3.
- [x] Btrfs subvolume quotas (alternative to loop mount) — `grill::btrfs` (statfs detection, pure argv generators + decision table); volumes on Btrfs become subvolumes (qgroup limit when sized — subvolumes even unsized, so E2 can snapshot them); backend recorded in a `*.volume.json` sidecar (delete/snapshot dispatch) which also made creation idempotent (restarts were stacking loop mounts); Lima-gated quota test provisions its own loopback btrfs; `btrfs-progs` added to VM provisioning, `RELIABURGER_BTRFS_TESTS`/`RELIABURGER_BUILDAH_TESTS` gates added to `relish dev test`
- [x] Parquet bloom filters for archive equality pruning — on `app`/`namespace` (1% FPP), not `line` (bloom filters answer equality, not substring LIKE; `bloom_filter_on_read` enabled in remote_query)
- [x] Zstd compression for archived logs — via Parquet's native per-row-group ZSTD codec (random access preserved; >5x vs flat text), not a separate seekable-frame container
- [~] Book chapter 12: "Squeezing Every Drop" — logs section written; remaining sections land with later slices
- [ ] All Phase 12 tests green

## Phase 13: Relish TUI

Implementation plan: [docs/plans/tui-plan-2026-07.md](plans/tui-plan-2026-07.md)

- [ ] Full interactive terminal UI (ratatui + crossterm)
- [ ] Dashboard, apps, nodes, jobs, events, logs, routes, search, help views
- [ ] Book chapter 13: "A Room with a View"
- [ ] All Phase 13 tests green

## Phase 14: Self-Upgrade

> Detailed implementation plan: [self-upgrade-plan-2026-07.md](plans/self-upgrade-plan-2026-07.md)
> (12 commit-sized steps, decision log, type definitions, test inventory, gotchas checklist).

- [x] Rolling binary replacement (exec-in-place; workers → council → leadership transfer → former leader; state in Raft; `relish upgrade` command set)
- [x] Dual-signature verification (embedded Ed25519 release key set + external operator key from node.toml; air-gapped `--binary` needs embedded only)
- [x] Automatic rollback on failure (crash-loop boot budget reverts the symlink; nodes refuse previously-reverted upgrade ids; leader pauses the run; `upgrade resume` retries with a fresh id)
- [x] Version retention and GC (keep newest `retain_versions`, rollback targets protected)
- [x] Workload adoption across the swap (ProcessGrill pidfile records + runc `state` adoption; pid+start-time fingerprinting; file-backed process logs). `[runc adoption unverified on Linux]`; AppleGrill adoption deferred (TODO in grill/apple.rs)
- [x] Book chapter 14: "Changing the Tyres at Full Speed"
- [x] All Phase 14 tests green (unit tests in the default suite; 5 single-node + 3 cluster real-binary integration tests gated behind `RELIABURGER_UPGRADE_TESTS=1`, too slow for the default test job). Single-node runs in the dedicated `upgrade-tests` CI job; the 4-node cluster suite is `make test-upgrade-cluster` on a real multi-core machine — it doesn't converge reliably on a contended 2-core CI runner (Raft membership-change RPC times out under load)

Deferred wiring (seams marked with TODOs): scheduler cordoning awaits an
in-binary `ClusterStateCache` (`meat::filter::apply_upgrade_cordon` is ready);
node API addresses are derived `gossip-ip:9117` until gossip advertises API
ports; cluster-mode post-upgrade verification checks adopted workloads but
not yet gossip-rejoin explicitly.

## Phase 15: Testing, Benchmarking & Diagnostics

> Detailed implementation plan: [chaos-plan-2026-07.md](plans/chaos-plan-2026-07.md)
> (15 commit-sized steps, test catalogue, data structures, acceptance runbook).

- [ ] `relish test` command (built-in test runner, parallel, filtering, JSON output)
- [ ] `relish test --chaos` (integration tests + Smoker fault injection)
- [ ] `relish bench` (scheduler, eBPF, network, deploy, state reconstruction benchmarks)
- [ ] `relish wtf` (automated cluster health diagnosis)
- [ ] `relish trace` (end-to-end connectivity debugging)
- [ ] Book chapter 15: "Ready for Production"
- [ ] All Phase 15 tests green

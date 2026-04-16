# Implementation Progress

Single source of truth for what's done and what's next. Check off an item only when it compiles, passes tests, and is committed. See [roadmap.md](roadmap.md) for full details on each phase.

---

## Phase 1: Foundation

- [x] Cargo workspace setup (binary `bun`, library `reliaburger`, test fixtures)
- [x] TOML config parsing (App, Job, Secret, ConfigFile, Volume, Permission, Namespace)
- [x] Grill container runtime interface (containerd/runc, OCI extraction, ports, cgroups)
- [x] Bun agent core (process supervisor, health checks, restart logic, GPU detection)
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
- [x] Init container execution (sequential run, failure prevents main start)
- [x] Restart re-drive (health check and job restarts re-start instances)
- [x] Exit code tracking on Grill trait (ProcessGrill, MockGrill)
- [x] Example configs (minimal-app, restarts, job-success, job-failure, init-container, volumes, multi-app, full-featured)
- [x] OCI image pulling from Docker Hub (oci-distribution, content-addressed cache, layer unpacking with whiteouts)
- [x] Rootless runc (user namespaces, UID/GID mapping, rootless cgroups v2, no-sudo containers)
- [x] Streaming apply progress via SSE (real-time deploy feedback instead of blocking response)
- [x] HostPath-style volumes (dual-mode: explicit source for hostPath, managed for auto-provisioned storage)
- [x] Relish init command (scaffold reliaburger.toml and app.toml from defaults)
- [x] Log tailing (`--tail N`) and streaming (`--follow`/`-f`)
- [x] Relish exec command (run commands in running instances)
- [x] All Phase 1 tests green (321 tests)

## Phase 2: Cluster Formation

- [x] Shared types: `NodeId`, `AppId`, `Resources`, `NodeCapacity`, `SchedulingDecision` (`src/meat/types.rs`)
- [x] Mustard state machine: NodeState enum, incarnation conflicts, membership table, piggyback dissemination
- [x] Mustard transport and protocol: `MustardTransport` trait, SWIM probe cycle, gossip convergence tests
- [x] Indirect probe (PING-REQ) ACK routing, proptest for conflict resolution, broadcast count lambda=3
- [x] Dead node reap timer (cleanup_timeout=60s), graceful leave protocol (Left state broadcast on shutdown)
- [x] Raft integration (openraft): storage, network, and state machine adapters; leader election and log replication
- [x] Council selection: stability/zone diversity scoring, deterministic tiebreak, size bounds 3–7
- [x] Reporting tree: `StateReport` to council member every 5s, consistent hash assignment, `watch` channel
- [x] State reconstruction: learning period after leader election, 95% threshold or 15s timeout, diff/correction
- [x] Meat scheduler: Filter → Score → Select → Commit pipeline, bin-packing, labels, daemon mode, quotas
- [x] Scheduler image locality scoring — prefers nodes with cached images
- [x] Scheduler stability scoring — prefers nodes with longer uptime
- [x] Agent integration: wire cluster subsystems into `BunAgent`, extend config, cluster API endpoints
- [x] CLI extensions: `relish nodes`, `relish council` (stub responses, full pipeline)
- [x] CLI extensions: `relish join`
- [x] Chaos tests: council partition, worker isolation (full council loss deferred to Phase 4/8)
- [x] Book chapter + docs: `02-finding-friends.md`, update README and progress (588 tests)

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
  - [x] eBPF C programs and Rust loader scaffolding (Linux only)
  - [x] Wire aya loader for connect rewrite (cgroup/connect4)
  - [x] Userspace DNS responder for `.internal` queries (replaces infeasible in-kernel DNS synthesis)
  - [x] `relish dev test` runs Linux + eBPF tests from macOS via Lima
  - [x] eBPF integration tests (load/attach, map read/write, connect rewrite, DNS responder)
- [x] Wrapper ingress proxy (host/path routing, load balancing, draining, rate limiting)
  - [x] Routing table (host/path → backend pool, longest prefix match, round-robin LB)
  - [x] HTTP reverse proxy on dedicated tokio runtime (DDoS isolation, connection limit)
  - [x] Per-client-IP token bucket rate limiting (429 + Retry-After)
  - [x] Connection draining protocol (zero-downtime deploys)
  - [x] Agent wiring (routing table rebuilds on deploy/stop/health, `relish routes` command)
  - [x] TLS termination with self-signed certs (rcgen + rustls, Phase 4 adds ACME + Sesame)
  - WebSocket upgrade proxying — Phase 9
- [x] nftables perimeter firewall (cluster boundary rules, management access)
  - [x] Ruleset generation (targeted blocking of Reliaburger ports, policy accept)
  - [x] `apply_ruleset()` via `nft -f` (Linux), no-op on macOS
  - [x] Wire into agent (reconcile on gossip membership changes, auto-disabled in rootless mode)
- [x] All Phase 3 tests green (702 tests)

## Phase 4: Security

- [x] Sesame CA hierarchy (Root, Node, Workload, Ingress CAs — ECDSA P-256, HKDF key wrapping)
- [x] Node mTLS (join tokens, certificate issuance, mTLS config builders, gossip HMAC)
- Workload identity — deferred to Phase 10
- [x] API authentication (tokens, Argon2id hashing, roles: Admin/Deployer/ReadOnly, axum middleware)
- [x] Secret encryption (age keypairs, `ENC[AGE:...]` decryption at container startup, namespace-scoped keys)
- [x] eBPF firewall rules (`allow_from` resolution, cgroup-to-namespace mapping, BPF map wiring)
- [x] Raft log encryption at rest (AES-256-GCM, HKDF from node cert private key)
- [x] `relish init` generates full PKI + join token; `relish token create`
- `relish token list/revoke` — moved to Phase 10 (requires SecurityState in Raft)
- Join token validation in agent — moved to Phase 10 (requires SecurityState in Raft)
- [x] `relish secret pubkey` and `relish secret encrypt` CLI commands
- `relish secret rotate` — moved to Phase 10 (requires SecurityState in Raft)
- [x] Book chapter 4: "Trust No One"
- [x] All Phase 4 tests green (795 tests)

## Phase 5: Storage & Registry

- [x] Pickle types and Raft state extensions (Digest, ImageManifest, ManifestCatalog, Raft commands)
- [x] Pickle blob store (content-addressed, upload sessions, digest verification, atomic rename)
- [x] OCI Distribution API (push/pull: blob upload POST/PATCH/PUT, manifest PUT/GET, tag list)
- [x] Synchronous replication on push (peer selection, layer transfer via OCI API, mTLS)
- [x] Peer pull (fetch missing layers from peers via Raft layer_locations)
- [x] Garbage collection (sole-copy protection, active reference safety, retention window, GcReport)
- [x] Volume size enforcement (loop mount on Linux, soft warning on macOS)
- [x] `relish images` CLI command, `[images]` config section
- Pull-through cache (Phase 12), P2P downloads (Phase 12), image signing (Phase 10), volume snapshots (Phase 12)
- [x] Book chapter 5: "Where the Images Live"
- [x] All Phase 5 tests green (867 tests)

## Phase 6: Observability

- [x] Mayo TSDB (Arrow RecordBatches + DataFusion SQL + Parquet persistence via object_store)
- [x] System metrics collector (CPU, memory, disk, network via sysinfo)
- [x] Prometheus scraping (prometheus-parse crate, auto /metrics endpoint)
- [x] Metrics API (`/v1/metrics`, `/v1/metrics/summary`, `/v1/metrics/keys`)
- [x] Alert evaluation (5 default rules, Inactive→Pending→Firing state machine)
- [x] Ketchup log collection (append-only files, sparse timestamp index, JSON detection)
- [x] Ketchup queries (grep, tail, time range, JSON field filter)
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
- [x] Rolling deploy orchestrator (DeployDriver trait, per-step health-gated replacement)
- [x] Automatic rollback (revert upgraded instances on health failure)
- [x] Dependency ordering (`run_before` jobs complete before rolling)
- [x] Deploy Raft persistence (active deploys + history capped at 50 per app)
- [x] CLI: `relish deploy`, `relish history`, `relish rollback`, `relish lint`
- [x] API: `/v1/deploys/active`, `/v1/deploys/history/{app}`
- [x] `make deploy-demo` for local testing
- [x] Book chapter 7: "Ship It"
- Autoscaling, Lettuce GitOps, blue-green, K8s migration — see Phase 9
- [x] All Phase 7 tests green (1039 tests)

## Phase 8: Advanced

- [x] Smoker fault injection (safety rails, fault registry, process/resource/node faults, eBPF network fault types + maps, scripted scenarios, chaos test suite)
- [x] Network security (egress allowlists, eBPF enforcement in connect hook, namespace isolation)
- [x] Process workloads (exec/script apps and jobs, binary allowlist, ProcessManager, OCI spec wiring, validation)
- [x] High-throughput batch scheduling (BatchScheduler greedy bin-packing, BatchTracker, 100K jobs in <1s)
- [x] Build jobs (BuildSpec config, pickle:// destination parsing, namespace-scoped push, buildah integration)
- [x] All Phase 8 tests green (1263 tests)

## Phase 9: User Experience

- [x] Blue-green deploy strategy (parallel start, atomic routing swap, orchestrator dispatch)
- [x] Autoscaling (evaluation logic, hysteresis, cooldown, Mayo query_avg, async task runner, Raft persistence)
- [x] Lettuce GitOps engine (sync loop, git ops, signature verification, diff engine, webhook endpoint, coordinator election, Raft integration, node config)
- [x] Kubernetes migration (`relish import`, `relish export` via k8s-openapi, resource correlation, migration reports, optional `kubernetes` feature)
- [x] `relish compile`, `relish diff`, `relish fmt`
- [x] WebSocket upgrade proxying in Wrapper ingress (detection, dispatch, close frame, draining)
- [x] Book chapter 9: "The Full Package"
- [x] All Phase 9 tests green (1271 tests)

## Phase 10: Advanced Security

- [x] Workload identity (SPIFFE certs, CSR, automatic rotation, OIDC JWTs)
- [x] Image signing (keyless via workload identity, cosign-compatible)
- [ ] TPM sealing, CRL distribution, egress DNS resolution
- [ ] `relish token list/revoke` (SecurityState in Raft)
- [ ] Join token validation in agent (SecurityState in Raft)
- [ ] `relish secret rotate` (SecurityState in Raft)
- [ ] Book chapter 10: "Locking It Down"
- [ ] All Phase 10 tests green

## Phase 11: Advanced Observability

- [ ] PromQL-to-SQL compatibility layer (rate, sum by, avg by, histogram_quantile)
- [ ] Hierarchical metrics aggregation via council (cluster-wide queries)
- [ ] Full Brioche UI (app/node detail pages, HTMX auto-refresh, uPlot charts)
- [ ] Alert webhooks (Slack, PagerDuty, generic HTTP)
- [ ] Log export to S3/GCS (scheduled, jsonl.gz)
- [ ] Cross-node log queries via Raft (leader fan-out, merge-sort)
- [ ] Book chapter 11: "Eyes Everywhere"
- [ ] All Phase 11 tests green

## Phase 12: Optimisations

- [ ] Switch port mapping from nftables rules to nftables maps (O(1) lookup at scale)
- [ ] P2P multi-source image downloads (parallel fan-out)
- [ ] Pull-through cache full wiring (upstream → Pickle → Raft)
- [ ] Volume snapshots (CoW, scheduled jobs, S3/GCS upload)
- [ ] Btrfs subvolume quotas (alternative to loop mount)
- [ ] Parquet bloom filters on log `line` column (skip row groups in LIKE queries)
- [ ] Zstd seekable frame compression for archived logs
- [ ] Book chapter 12: "Squeezing Every Drop"
- [ ] All Phase 12 tests green

## Phase 13: Relish TUI

- [ ] Full interactive terminal UI (ratatui + crossterm)
- [ ] Dashboard, apps, nodes, jobs, events, logs, routes, search, help views
- [ ] Book chapter 13: "A Room with a View"
- [ ] All Phase 13 tests green

## Phase 14: Self-Upgrade

- [ ] Rolling binary replacement
- [ ] Dual-signature verification
- [ ] Automatic rollback on failure
- [ ] Version retention and GC
- [ ] Book chapter 14: "Changing the Tyres at Full Speed"
- [ ] All Phase 14 tests green

## Phase 15: Testing, Benchmarking & Diagnostics

- [ ] `relish test` command (built-in test runner, parallel, filtering, JSON output)
- [ ] `relish test --chaos` (integration tests + Smoker fault injection)
- [ ] `relish bench` (scheduler, eBPF, network, deploy, state reconstruction benchmarks)
- [ ] `relish wtf` (automated cluster health diagnosis)
- [ ] `relish trace` (end-to-end connectivity debugging)
- [ ] Book chapter 15: "Ready for Production"
- [ ] All Phase 15 tests green

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
  - [ ] Switch port mapping from individual nftables rules to nftables maps for O(1) lookup at scale
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
  - [ ] TLS termination with self-signed certs (Phase 4 adds ACME + Sesame)
  - [ ] WebSocket upgrade proxying
- [ ] nftables perimeter firewall (cluster boundary rules, management access)
- [ ] All Phase 3 tests green

## Phase 4: Security

- [ ] Sesame CA hierarchy (Root, Node, Workload, Ingress CAs)
- [ ] Node mTLS (join tokens, certificate issuance, inter-node encryption)
- [ ] Workload identity (SPIFFE certs, CSR, automatic rotation, OIDC JWTs)
- [ ] API authentication (tokens, roles, scoping, rate limiting, audit logging)
- [ ] Secret encryption (age keypairs, `ENC[AGE:...]`, namespace-scoped keys, rotation)
- [ ] eBPF firewall rules (`allow_from` ingress, egress allowlists, namespace isolation)
- [ ] Raft log encryption at rest (AES-256-GCM, HKDF)
- [ ] All Phase 4 tests green

## Phase 5: Storage & Registry

- [ ] Pickle registry (OCI Distribution API, content-addressed store, synchronous replication)
- [ ] Peer-to-peer layer distribution (parallel multi-source downloads)
- [ ] Pull-through cache (Docker Hub, GHCR, ECR)
- [ ] Image signing (keyless via workload identity, cosign-compatible)
- [ ] Distributed garbage collection (Raft GcReport)
- [ ] Local volumes (Btrfs subvolume quotas / loop mount, size limits)
- [ ] Volume snapshots (CoW, scheduled jobs, S3/GCS upload)
- [ ] All Phase 5 tests green

## Phase 6: Observability

- [ ] Mayo TSDB (per-node time-series, 3-tier retention, downsampling)
- [ ] Prometheus scraping (auto-detect `/metrics`, configurable intervals)
- [ ] Hierarchical metrics aggregation (council rollups for cluster queries)
- [ ] Built-in alerts (5 defaults + custom PromQL)
- [ ] Ketchup log collection (structured capture, timestamp-indexed storage, querying, retention)
- [ ] Brioche web UI (cluster overview, app detail, node detail, ingress, GitOps status)
- [ ] All Phase 6 tests green

## Phase 7: GitOps & Deployments

- [ ] Deploy orchestration (state machine, rolling/blue-green, draining, health gating)
- [ ] Automatic rollback (revert on health check failure)
- [ ] Dependency ordering (`run_before` job-to-app dependencies)
- [ ] Autoscaling (CPU/memory-based, runtime replica overrides)
- [ ] Lettuce GitOps engine (poll/webhook sync, signed commits, coordinator election)
- [ ] Relish config tooling (`plan`, `diff`, `compile`, `lint`, `fmt`)
- [ ] Kubernetes migration (`relish import`, `relish export`, migration reports)
- [ ] All Phase 7 tests green

## Phase 8: Advanced

- [ ] Smoker fault injection (eBPF network faults, resource faults, safety rails, expiry)
- [ ] Process workloads (exec/script apps and jobs, binary allowlist, isolation)
- [ ] High-throughput batch scheduling (100M jobs/day target)
- [ ] Build jobs (in-cluster image building, `pickle://`, scoped registry access)
- [ ] Network security (eBPF inter-app firewall, egress allowlists, namespace isolation)
- [ ] All Phase 8 tests green

## Phase 9: Production Hardening

- [ ] `relish test` command (built-in test runner, parallel, filtering, JSON output)
- [ ] `relish test --chaos` (integration tests + Smoker fault injection)
- [ ] `relish bench` (scheduler, eBPF, network, deploy, state reconstruction benchmarks)
- [ ] `relish wtf` (automated cluster health diagnosis)
- [ ] `relish trace` (end-to-end connectivity debugging)
- [ ] Relish TUI (apps, nodes, jobs, events, logs, routes, search views)
- [ ] Self-upgrade mechanism (rolling binary replacement, dual-signature, auto-rollback)
- [ ] All Phase 9 tests green

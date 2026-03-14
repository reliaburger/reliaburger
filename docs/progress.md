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

- [x] Shared types: `NodeId`, `AppId`, `Resources`, `NodeCapacity`, `SchedulingDecision` (`src/patty/types.rs`)
- [x] Mustard state machine: NodeState enum, incarnation conflicts, membership table, piggyback dissemination
- [x] Mustard transport and protocol: `MustardTransport` trait, SWIM probe cycle, gossip convergence tests
- [x] Indirect probe (PING-REQ) ACK routing, proptest for conflict resolution, broadcast count lambda=3
- [x] Dead node reap timer (cleanup_timeout=60s), graceful leave protocol (Left state broadcast on shutdown)
- [ ] Raft integration (openraft): storage, network, and state machine adapters; leader election and log replication
- [ ] Council selection: stability/zone diversity scoring, deterministic tiebreak, size bounds 3–7
- [ ] Reporting tree: `StateReport` to council member every 5s, consistent hash assignment, `watch` channel
- [ ] State reconstruction: learning period after leader election, 95% threshold or 15s timeout, diff/correction
- [ ] Patty scheduler: Filter → Score → Select → Commit pipeline, bin-packing, labels, daemon mode, quotas
- [ ] Agent integration: wire cluster subsystems into `BunAgent`, extend config, cluster API endpoints
- [ ] CLI extensions: `relish members`, `relish council`, `relish join`
- [ ] Chaos tests: council partition, worker isolation, full council loss recovery
- [ ] Book chapter + docs: `02-finding-friends.md`, update README and progress

## Phase 3: Networking

- [ ] Per-container network namespaces (veth pairs, port mapping)
- [ ] Onion eBPF service discovery (DNS interception, connect() rewrite, service map)
- [ ] Wrapper ingress proxy (host/path routing, TLS, WebSocket, load balancing, draining, rate limiting)
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
- [ ] Self-upgrade mechanism (rolling binary replacement, dual-signature, auto-rollback)
- [ ] Relish TUI (apps, nodes, jobs, events, logs, routes, search views)
- [ ] `relish wtf` (automated cluster health diagnosis)
- [ ] `relish trace` (end-to-end connectivity debugging)
- [ ] All Phase 9 tests green

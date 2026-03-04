# Reliaburger Roadmap

This roadmap defines the implementation phases for Reliaburger, ordered by dependency. Each phase builds on the previous and ends with a concrete, demonstrable milestone.

**Methodology:** Every phase starts by writing tests, then implementing until they pass. Each phase produces a chapter of the Reliaburger book, combining design narrative with Rust implementation walkthrough. See [CLAUDE.md](CLAUDE.md) for the full methodology.

For the full architectural vision, see [whitepaper.md](whitepaper.md). For implementation details on each component, see the [design/](design/) directory.

---

## Phase 1: Foundation, Single-Node Container Lifecycle

Build the core single-node primitives: run a container, health-check it, restart it on failure.

Book: **Chapter 1, "Hello, Container"** (project setup, Rust basics, running a container)

### Tests (write first)

Unit tests:

- Grill: OCI spec generation, port allocation/release, cgroup path construction, container state machine transitions
- HealthChecker: priority queue scheduling, consecutive counter logic, state transition decisions, timeout handling
- NodeConfig: TOML parsing with all defaults and all fields, validation errors, auto-detection fallbacks

Integration tests:

- Container lifecycle: deploy app with 1 replica, verify running
- Health check: app returns 200, verify marked healthy
- Health check: app returns 500, verify marked unhealthy and restarted
- Health check: app hangs (no response), verify timeout detection
- Health check: recovery re-adds to service map
- Init container: failure prevents main container start
- Volume: write survives container restart
- Job: runs to completion and reports success
- Job: failed job retries up to limit
- Relish CLI: `status`, `logs`, `exec`, `inspect` return expected output

### Implementation

1. **Cargo workspace setup.** Binary crate (`bun`), library crate (`reliaburger`), test fixtures.
2. **TOML config parsing.** Define all 7 resource types (App, Job, Secret, ConfigFile, Volume, Permission, Namespace) with serde, including `[[app.*.init]]` blocks.
3. **Grill container runtime interface.** containerd/runc integration, OCI image extraction, port mapping, cgroup management.
4. **Bun agent core.** Process supervisor loop, health checks, restart logic, GPU detection via NVML.
5. **Relish CLI skeleton.** `apply`, `status`, `logs`, `exec`, `inspect` (clap derive API, single-node mode).
6. **Run all tests green.**

Design docs: [agent-bun.md](design/agent-bun.md), [cli-relish.md](design/cli-relish.md)

**Milestone:** `relish init && relish apply -f app.toml` runs a container on one node with health checks, logs, and resource limits. All Phase 1 tests pass.

---

## Phase 2: Cluster Formation, Multi-Node Coordination

Add node discovery, leader election, and multi-node scheduling.

Book: **Chapter 2, "Finding Friends"** (networking, gossip protocols, distributed consensus)

### Tests (write first)

Unit tests:

- Mustard SWIM state machine: ALIVE → SUSPECT → DEAD transitions, incarnation number conflicts, piggyback dissemination
- Council selection: zone diversity scoring, stability filtering, tiebreaking determinism
- Patty scheduler simulation: bin-packing correctness, required/preferred label enforcement, daemon mode, quota enforcement, autoscaler stability

Integration tests:

- Gossip convergence: membership event propagates to all nodes within O(log N) protocol periods
- Raft leader election: kill leader on 5-member council. New leader elected within 5 seconds, log consistency verified.
- State reconstruction: populate cluster, kill leader. New leader's world view matches ground truth within learning period.
- Reporting tree failover: remove council member. Workers re-hash to surviving members.
- Scheduling: deploy 3 replicas, verify distinct nodes
- Scheduling: deploy daemon app (`replicas = "*"`), verify on all nodes
- Scheduling: deploy with required labels, verify correct placement
- Scheduling: deploy with preferred labels, verify fallback behaviour
- Scheduling: namespace quota rejection
- Scheduling: rolling deploy with zero downtime (continuous health probe)

Chaos tests (network namespace / iptables simulation):

- Council partition 3/2: majority elects leader, minority enters read-only. Heal and reconcile.
- Worker isolation: apps continue running, leader marks state-unknown.
- Full council loss: recovery candidate assumes leadership within `catastrophic_timeout`.

### Implementation

1. **Mustard gossip protocol.** SWIM-based membership, failure detection, leader identity broadcast.
2. **Raft consensus.** Leader election, desired-state replication across council.
3. **Council formation and selection.** Automatic selection based on stability and zone diversity.
4. **Hierarchical reporting tree.** Worker→council→leader aggregation for runtime state.
5. **State reconstruction protocol.** Learning period, StateReport collection, diff and correction.
6. **Patty scheduler.** Multi-node placement, required/preferred label constraints, GPU-aware scheduling.
7. **Run all tests green.**

Design docs: [gossip-mustard.md](design/gossip-mustard.md), [scheduler-patty.md](design/scheduler-patty.md)

**Milestone:** 3-node cluster with leader election, gossip-based failure detection, and app scheduling across nodes. All Phase 2 tests pass.

---

## Phase 3: Networking, Service Discovery and Ingress

Enable service-to-service communication and external traffic ingress.

Book: **Chapter 3, "Talking to Each Other"** (eBPF, Linux networking, reverse proxies)

### Tests (write first)

Unit tests (eBPF, using `BPF_PROG_TEST_RUN`):

- DNS interception: `.internal` name lookup, pass-through for non-`.internal`, malformed queries, case-insensitive matching
- Connect rewrite: VIP rewrite, ECONNREFUSED on no healthy backends, round-robin distribution

Integration tests:

- Service discovery: `dig redis.internal` returns the correct VIP from inside a container
- Service discovery: `curl http://web.internal:8080/` reaches a healthy backend
- Service discovery: namespace isolation enforced
- Service map consistency: deploy/scale/health-fail/recover/delete an app, verify BPF map state
- Ingress: TLS 1.3/1.2 handshake success, TLS 1.0/1.1 rejection
- Ingress: host-based routing exact match, mismatch returns 404
- Ingress: path-based routing (longest prefix match)
- Ingress: round-robin and least-connections load balancing
- Ingress: empty backend pool returns 502
- Ingress: WebSocket upgrade handshake, bidirectional data flow
- Ingress: drain completes slow request, drain timeout forces RST
- Ingress: rate limiting (under-limit pass, over-limit reject, Retry-After header)
- Perimeter firewall: external connections to non-ingress ports rejected
- `relish resolve redis` shows correct VIP, backends, health status

### Implementation

1. **Per-container network namespaces.** Network namespace creation, veth pairs, port mapping.
2. **Onion eBPF service discovery.** DNS interception, connect() rewrite, in-kernel service map.
3. **Wrapper ingress proxy.** Host/path-based routing, TLS termination, WebSocket support, health-aware load balancing, connection draining, rate limiting.
4. **nftables perimeter firewall.** Cluster boundary rules, management access control.
5. **Run all tests green.**

Design docs: [discovery-onion.md](design/discovery-onion.md), [ingress-wrapper.md](design/ingress-wrapper.md), [security-sesame.md](design/security-sesame.md)

**Milestone:** Apps discover each other via `name.internal`, external traffic enters via Wrapper with automatic routing. All Phase 3 tests pass.

---

## Phase 4: Security, PKI and Identity

Establish the trust hierarchy and encrypt all cluster communication.

Book: **Chapter 4, "Trust No One (Until They Prove It)"** (PKI, mTLS, cryptography in Rust)

### Tests (write first)

Unit tests:

- Certificate chain validation, dual-signing period during rotation
- age encryption/decryption round-trip, namespace key isolation
- Token generation, validation, expiry, scope checking
- HKDF key derivation, AES-256-GCM encrypt/decrypt round-trip

Integration tests:

- PKI rotation: workloads continue serving during CA rotation
- Certificate expiry: grace period extends when council is unreachable, hard expiry at boundary
- mTLS: all inter-node traffic encrypted. Plain HTTP connections rejected.
- Workload identity: SPIFFE certificate issued, automatic rotation, OIDC JWT validation
- Join token: expiry enforced, single-use enforced
- Secret encryption: encrypt a value, deploy an app, verify it's decrypted in the container env
- Secret rotation: rotate key. Old and new ciphertexts accepted during transition; finalize drops old.
- Firewall: `allow_from` restrictions enforced. Unauthorized connections get ECONNREFUSED.
- Firewall: egress allowlist blocks unauthorized destinations.
- Firewall: `relish firewall test` diagnostic accuracy
- API auth: valid token succeeds, expired token rejected, insufficient scope rejected.
- Raft log encryption: encrypted at rest, readable after node restart.
- Audit logging: token `last_used` updated, secret decryption events recorded.

### Implementation

1. **Sesame CA hierarchy.** Root CA, Node CA, Workload CA, Ingress CA generation and storage.
2. **Node mTLS.** Join tokens, certificate issuance, inter-node encryption.
3. **Workload identity.** SPIFFE-compatible certs, CSR model, automatic rotation, OIDC JWTs.
4. **API authentication.** Token creation with roles (admin/deployer/read-only), scoping, rate limiting, audit logging.
5. **Secret encryption.** age asymmetric keypairs, `ENC[AGE:...]` decryption, namespace-scoped keys, key rotation.
6. **eBPF firewall rules.** `allow_from` ingress, egress allowlists, namespace isolation.
7. **Raft log encryption at rest.** AES-256-GCM, HKDF key derivation.
8. **Run all tests green.**

Design docs: [security-sesame.md](design/security-sesame.md)

**Milestone:** All inter-node traffic uses mTLS. Workloads have SPIFFE identities, secrets are encrypted in git, and the Raft log is encrypted at rest. All Phase 4 tests pass.

---

## Phase 5: Storage & Registry, Image Distribution and Persistence

Build the image distribution layer and local volume management.

Book: **Chapter 5, "Where the Images Live"** (OCI registries, content-addressed storage, filesystem quotas)

### Tests (write first)

Unit tests:

- OCI manifest parsing, layer content-address verification
- Btrfs subvolume quota creation, loop mount enforcement
- Snapshot creation, compression, restoration

Integration tests:

- Push/pull round-trip: push multi-layer image, pull from same node (< 2s), pull from different node via P2P (< 5s)
- Replication: push succeeds only after N peer replications. Verify the image survives single node loss.
- Replication: under-replicated image auto-heals when a new node joins
- Pull-through cache: first pull from Docker Hub is cached, second pull served locally
- Image signing: signed image verified, unsigned image rejected when signing is required
- GC safety: active images aren't collected, unreferenced images are collected after retention, sole copy never deleted
- Local volumes: size limit enforced (write beyond quota fails)
- Volume snapshots: create snapshot, corrupt data, restore from snapshot, verify data intact
- Volume: write survives container restart but is lost on node reassignment (by design)

### Implementation

1. **Pickle registry.** OCI Distribution API, local content-addressed store, push with synchronous replication.
2. **Peer-to-peer layer distribution.** Parallel multi-source downloads.
3. **Pull-through cache.** Transparent caching for Docker Hub, GHCR, ECR.
4. **Image signing.** Keyless signing via workload identity, cosign-compatible verification.
5. **GC.** Distributed garbage collection with Raft GcReport serialisation.
6. **Local volumes.** Btrfs subvolume quotas / loop mount enforcement, size limits.
7. **Volume snapshots.** CoW snapshots, scheduled snapshot jobs, upload to S3/GCS.
8. **Run all tests green.**

Design docs: [registry-pickle.md](design/registry-pickle.md), [agent-bun.md](design/agent-bun.md)

**Milestone:** `docker push cluster:5000/app:v1` distributes the image across peers. Volumes persist across restarts with snapshot backup. All Phase 5 tests pass.

---

## Phase 6: Observability, Metrics, Logs, Dashboards

Add built-in monitoring with zero configuration.

Book: **Chapter 6, "Watching Everything"** (time-series databases, log storage, web UIs in Rust)

### Tests (write first)

Unit tests:

- Mayo: retention tier rollup aggregation, counter reset handling, downsampler precision.
- Mayo: sum/avg/max/min aggregation correctness, commutativity.
- Mayo: alert state machine (Inactive → Pending → Firing), threshold reset, per-app suppression.
- Ketchup: log file round-trip, index binary search, JSON detection, grep filter, JSON field filter
- Brioche: Askama template compile-time type checks, XSS escaping, HTML well-formedness

Integration tests:

- Metrics: 2-hour synthetic data. Verify tier transitions (10s → 1m → 1h) and pruning.
- Metrics: 5-node cluster with deterministic metrics. Verify hierarchical aggregation correctness.
- Metrics: partial results returned when one aggregator is down.
- Metrics: Prometheus remote-read API round-trip, PromQL function correctness.
- Metrics: scrape auto-detection within one collection interval, no errors for apps without `/metrics`.
- Alerts: memory pressure triggers alert within evaluation window, webhook payload correct.
- Logs: container stdout/stderr captured by Ketchup, day rotation, compression.
- Logs: cross-node query (3 nodes, 3 replicas, all lines in timestamp order).
- Logs: Bun restart reconnects log stream, no lines lost.
- Logs: retention eviction under storage pressure.
- Brioche: cluster overview renders with correct data. App detail shows instance count.
- Brioche: streaming logs appear within 2 seconds.
- Brioche: encrypted env vars aren't exposed in API responses.
- `relish top` shows live resource usage

Property-based tests:

- Ketchup: index lookup monotonicity, no line loss for arbitrary sequences, compression round-trip

### Implementation

1. **Mayo TSDB.** Per-node time-series storage, 3-tier retention (10s/1m/1h), downsampling.
2. **Prometheus scraping.** Auto-detect `/metrics` endpoints, configurable intervals.
3. **Hierarchical metrics aggregation.** Council member rollups for cluster-wide queries.
4. **Built-in alerts.** 5 default alerts (CPU throttle, OOM, memory, disk, CPU idle) + custom PromQL.
5. **Ketchup log collection.** Structured capture from stdout/stderr, timestamp-indexed storage, querying, retention.
6. **Brioche web UI.** Cluster overview, app detail, node detail, ingress overview, GitOps status (axum + Askama + htmx).
7. **Run all tests green.**

Design docs: [metrics-mayo.md](design/metrics-mayo.md), [logs-ketchup.md](design/logs-ketchup.md), [ui-brioche.md](design/ui-brioche.md)

**Milestone:** `relish top` shows live resource usage. Brioche dashboards work, and alerts fire on default conditions. All Phase 6 tests pass.

---

## Phase 7: GitOps & Deployments, Production Workflow

Enable git-driven deployments with safety guarantees.

Book: **Chapter 7, "Ship It"** (deployment state machines, GitOps, config tooling)

### Tests (write first)

Unit tests:

- Deploy state machine: Pending → RunningPreDeps → Rolling → Completed, and failure paths (Halted, Reverting → RolledBack)
- Lettuce sync loop: noop, new commit, partial parse error, git fetch failure backoff, Raft write retry
- Signature verification: GPG valid/untrusted, SSH valid, unsigned with `require_signed_commits`
- Autoscaler interaction: replicas preserved when other fields change, overridden when changed in git
- Config tooling: `compile` resolves `_defaults.toml` merging, `lint` catches errors, `fmt` is idempotent
- K8s import: resource correlation (Deployment + Service + Ingress → single App), field mapping, migration report

Integration tests:

- Rolling deploy: 3-replica app, continuous health probe, zero 5xx during rollout. All instances end up on new version.
- Blue-green deploy: version header shows atomic cutover, zero dropped requests.
- Auto-rollback: deploy a broken image. Verify it reverts to the previous version, and history records the reason.
- Dependency ordering: `run_before` job completes before app starts.
- Autoscaling: CPU load triggers scale-up within evaluation window. Scale-down on relief, stays within bounds.
- GitOps end-to-end: commit to local bare repo. Lettuce syncs within poll interval, app deployed.
- GitOps webhook: push triggers deploy within 5 seconds.
- GitOps rollback: git revert triggers rollback.
- GitOps coordinator failover: new coordinator resumes from `last_applied_commit`.
- Config tooling: `relish plan` shows correct diff. `relish lint` catches invalid config.
- K8s import: `relish import -f deployment.yaml -f service.yaml` produces correct TOML.
- K8s export: `relish export --format kubernetes` produces valid K8s YAML.

### Implementation

1. **Deploy orchestration.** State machine on leader, rolling and blue-green strategies, connection draining, health-check gating.
2. **Automatic rollback.** Revert on health check failure (opt-in via `auto_rollback = true`).
3. **Dependency ordering.** `run_before` job-to-app dependencies, migration-before-deploy.
4. **Autoscaling.** CPU/memory-based, runtime replica overrides preserved across GitOps syncs.
5. **Lettuce GitOps engine.** Poll/webhook sync, signed commit verification, coordinator election, autoscaler interaction.
6. **Relish config tooling.** `plan`, `diff`, `compile`, `lint`, `fmt`.
7. **Kubernetes migration.** `relish import` (K8s YAML → TOML) and `relish export` (TOML → K8s manifests) with migration reports.
8. **Run all tests green.**

Design docs: [gitops-lettuce.md](design/gitops-lettuce.md), [deployments.md](design/deployments.md), [cli-relish.md](design/cli-relish.md)

**Milestone:** `git push` triggers a validated rolling deploy that automatically rolls back on failure. `relish import -f k8s-manifests/` converts an existing Kubernetes project. All Phase 7 tests pass.

---

## Phase 8: Advanced Capabilities, Chaos, Process Workloads, Batch

Add fault injection, non-container workloads, and high-throughput batch scheduling.

Book: **Chapter 8, "Breaking Things on Purpose"** (eBPF fault injection, process isolation, batch systems)

### Tests (write first)

Unit tests:

- ProcessManager: binary allowlist validation, mount namespace construction, script temp file creation, isolation config
- Smoker: fault injection/removal in BPF maps, expiry enforcement, safety rail checks

Integration tests:

- Process workloads: exec app runs, gets health checked, appears in service map
- Process workloads: inline script job runs and completes
- Process workloads: correct namespace/cgroup isolation
- Process workloads: binary not in allowlist is rejected
- Process workloads: can't see `/var/lib/reliaburger` or other workloads' volumes
- Fault injection: delay, drop, DNS NXDOMAIN, partition, bandwidth (each verified for injection and recovery)
- Fault injection: CPU stress, memory fill, disk-io throttle (verified with cgroup metrics)
- Fault injection: process SIGKILL (reschedule), SIGSTOP/SIGCONT (health check cycle)
- Fault injection: 5s duration auto-clears, eBPF-level expiry verified
- Safety rails: partitioning a majority of council returns QuorumRisk error; killing all replicas returns ReplicaMinimum error
- Safety rails: faulting leader without `--include-leader` is rejected
- Batch scheduling: 100,000 job instances across 100 nodes, allocation under 1 second
- Build jobs: in-cluster image build, push to `pickle://`. Image available for deploy.
- Network security: eBPF inter-app firewall, egress allowlists, namespace isolation
- `relish fault clear` works via Unix socket when cluster API unavailable

Chaos tests (via Smoker):

- Kill leader mid-deploy: new leader completes the deploy from Raft state.
- Kill node: replicas rescheduled, zero downtime for multi-replica apps.
- Drain node: zero-downtime migration.
- Kill 2 of 3 replicas simultaneously: recovered within health timeout.
- Rapid leader elections (3 in 30s): cluster stabilizes.
- Node failure with volume app: alert fires, volume isn't lost.
- Resource exhaustion: OOM kill triggers restart + recovery, CPU stress triggers degraded state, disk full triggers alert + GC.
- Bun restart: containers keep running. Reconnects on restart, deploy resumes if interrupted.

### Implementation

1. **Smoker fault injection.** eBPF network faults (delay, drop, partition, DNS, bandwidth), resource faults (CPU/memory/disk), safety rails, expiry.
2. **Process workloads.** Exec/script apps and jobs, binary allowlist, mount namespace isolation, cgroup limits.
3. **High-throughput batch scheduling.** Batch allocation to nodes, async reporting, 100M jobs/day target.
4. **Build jobs.** In-cluster image building via Pickle, `pickle://` destination, scoped registry access.
5. **Network security.** eBPF inter-app firewall (`allow_from`), egress allowlists, namespace isolation.
6. **Run all tests green.**

Design docs: [chaos-smoker.md](design/chaos-smoker.md), [agent-bun.md](design/agent-bun.md), [security-sesame.md](design/security-sesame.md), [discovery-onion.md](design/discovery-onion.md)

**Milestone:** `relish fault delay redis 200ms` works, process jobs run with full isolation, batch scheduling meets throughput targets. All Phase 8 tests pass, including chaos suite.

---

## Phase 9: Production Hardening, Tooling, Performance, Polish

Build the self-validation tooling and polish the operator experience. By this phase, all subsystem tests already exist and pass. This phase wraps them into the built-in test runner, adds the benchmark harness, and builds the remaining operator tools.

Book: **Chapter 9, "Ready for Production"** (TUI development, performance tuning, self-upgrading binaries)

### Tests (write first)

Unit tests:

- TUI snapshot tests: ratatui TestBackend + insta for all views (dashboard, apps, nodes, jobs, events, logs, routes, search, help)
- TUI navigation tests: key sequences produce correct view stack transitions
- UpgradeManager: signature verification, symlink management, version retention/GC
- `wtf` correlation engine: known patterns produce correct diagnoses

Integration tests:

- `relish test` runs all 39 integration tests and reports results
- `relish test --chaos` runs chaos suite and reports results
- `relish test --filter scheduling` runs only scheduling tests
- `relish bench` produces valid benchmark report with all metrics
- `relish bench --compare` detects regression (> 10% metric degradation)
- Self-upgrade: upgrade a single node. Containers survive.
- Self-upgrade: roll back a single node. Revert succeeds.
- Self-upgrade: full rolling upgrade across the cluster (workers first, council, leader last).
- Self-upgrade: upgrade failure triggers automatic rollback.
- `relish wtf` detects and diagnoses known failure patterns
- `relish trace <app> --to <app>` traces connectivity through eBPF, firewall, and network layers
- TUI launches, renders dashboard, keyboard navigation works

### Implementation

1. **`relish test` command.** Test runner that executes all subsystem integration tests (compiled into binary), parallel execution, filtering, JSON output for CI.
2. **`relish test --chaos`.** Combines integration tests with Smoker fault injection, confirmation prompt, production safety check.
3. **`relish bench`.** Benchmark harness (scheduler throughput, eBPF latency, network throughput, deploy speed, state reconstruction), regression detection via `--compare`.
4. **Self-upgrade mechanism.** Rolling binary replacement, dual-signature verification, automatic rollback.
5. **Relish TUI.** Full interactive terminal UI (ratatui + crossterm): apps, nodes, jobs, events, logs, routes, search views.
6. **`relish wtf`.** Automated cluster health diagnosis with root cause correlation.
7. **`relish trace`.** End-to-end connectivity debugging through eBPF, firewall, and network layers.
8. **Run all tests green.**

Design docs: [cli-relish.md](design/cli-relish.md), [agent-bun.md](design/agent-bun.md)

**Milestone:** All design goal targets met, full test suite passes, self-upgrade works end-to-end. `relish test` reports all green. All Phase 9 tests pass.

---

## Future (v2)

Features explicitly deferred to v2 (see whitepaper §22 for rationale):

- **External secret manager integration.** Vault, AWS Secrets Manager, GCP Secret Manager.
- **Multi-cluster federation (Franchise).** The design is in whitepaper §21; implementation deferred to v2.
  - WAN gossip ring (Mustard extension) for cluster-level metadata exchange
  - Cross-cluster service discovery via `name.cluster.franchise` DNS (Onion extension)
  - Cross-cluster traffic via Wrapper ingress (no VPNs or tunnels)
  - Unified Brioche dashboard and `relish franchise status` CLI
  - Cross-cluster image pull via Pickle OCI API
  - Multi-cluster GitOps via Lettuce per-cluster directories
  - `relish franchise join` one-command peering with OIDC trust bundle exchange
- **IPv6 support.** Dual-stack networking, IPv6 virtual IPs.
- **Sidecars.** Co-located containers sharing a parent's network namespace and lifecycle.
- **Fractional GPU scheduling.** MIG partitions on NVIDIA A100/H100, time-slicing.

# Phase 15 Implementation Plan — Testing, Benchmarking, Diagnostics & Chaos

**Originally written:** 6 July 2026
**Revised:** 26 July 2026 — re-verified against `main` after the 12b programme, both codebase reviews (2026-07-17 posture, 2026-07-19 code-logic) and PRs #133–#142. **Scope extended** to complete the `relish fault`/`relish chaos` surface, which the original plan assumed was finished.

**Audience:** the implementing model. Deliberately prescriptive: exact paths, type names, signatures, test names, commit boundaries. Where this plan and the source disagree, the source wins — then fix this file and say so in the commit.

---

## 0. What changed since 6 July (read this first)

The original plan was written before Phase 12b, both reviews and their follow-up PRs. Fifteen of its factual claims are now wrong. Verified 26 July 2026:

| The old plan said | Reality on `main` today |
|---|---|
| CLI has 22 commands | **41** (`src/bin/relish.rs:43`), including `Chaos` and `Fault` |
| `relish fault ...` exists and works | Exists — 15 subcommands — but with **real gaps** (§3) |
| `router_with_upgrade` takes ~14 params | **23** (`src/bun/api.rs:196`) |
| Chapter 15 is a stub to be written | **329 lines already written** — the harness/philosophy half from the earlier Phase 15 tranche. New sections *append*, they don't replace |
| `TestAppMode` has 4 variants | **5** (`Healthy, UnhealthyAfter, Hang, ExitAfter, Slow`) — `src/bun/testapp.rs:16` |
| testapp is an HTTP server we can add routes to | It's a **raw TCP server that answers every path identically** (`testapp.rs:74-111`). `/payload` and `/env/` mean teaching it to parse a request line |
| `TestHarness` in `tests/integration.rs` is reusable | It's **private** (`tests/integration.rs:29`) and there's now a `tests/support/` directory. Needs promoting or duplicating |
| Reuse `Capabilities` as a fresh name | **Name collision**: `NodeCapabilities` (`src/meat/cluster_state.rs:15`) and `PlatformCapabilities` (`src/bun/supervisor.rs:74`) exist. Use **`ClusterCapabilities`** |
| `[cluster] environment` needs adding | Still true — `ClusterSection` (`src/config/node.rs:308`) has no `environment` |
| Smoker is library-only (review L14/L15) | **Wired.** `apply_fault` (`src/bun/agent.rs:3102`) has real paths for Kill, Pause, Resume, CpuStress, DnsNxdomain, MemoryPressure, DiskIoThrottle, and eBPF Delay/Drop/Bandwidth |
| Graceful-skip is needed for most subsystems | Still the right stance, but **far fewer skips**: ingress, rollups, GitOps, egress, eBPF sync, identity are all wired now |
| `relish test --chaos` scenarios use `node-kill` | **`node-kill`/`node-drain` always return an error** (`agent.rs:3299`). Three of the five scenarios cannot work until this is implemented — see §2 |
| Faults apply cluster-wide | **Node-local only.** The CLI POSTs to one agent; `target_node` is never used to route (`src/relish/fault.rs:512`) |
| `deployer` is the right role for faults | Design says **admin or an explicit `fault-injection` grant** (`chaos-smoker.md:88,1429`). Today plain `Deployer` (`src/bun/authz.rs:152`) |

**Still true and load-bearing:** `main() -> ExitCode` (`relish.rs:833`) but every dispatched command returns `Result<(), RelishError>`, so `wtf`'s 0/1/2 contract still needs a richer return; `insta` is a dev-dependency with existing snapshot dirs; `src/relish/fault.rs` has the duration/percentage/bandwidth parsers to reuse (`:16,:61,:81`); there is no `src/testkit/`, no `/v1/capabilities`, no `/v1/trace`, no `src/firewall/evaluate.rs`.

---

## 1. What Phase 15 delivers

| Deliverable | Status today | Work |
|---|---|---|
| `relish test` | absent | build (§6 stream B) |
| `relish test --chaos` | absent | build (stream D) — **blocked on stream C** |
| `relish bench` | absent | build (stream E) |
| `relish wtf` | absent | build (stream F) |
| `relish trace` | absent | build (stream G) |
| `relish fault ...` | 15 subcommands, gaps | **complete (stream C — new)** |
| `relish chaos ...` | stringly-typed, undesigned | **fold into `fault` (stream C — new)** |
| Book chapter 15 | harness half written | append command sections |

**Milestone:** `relish test` green against a 3-node dev cluster; `relish test --chaos` runs all five scenarios *with real node kills*; `relish bench` produces a comparable report; `relish wtf` diagnoses seeded failures; `relish trace` walks four steps; the documented fault surface matches the implemented one.

---

## 2. The finding that reorders the plan

The old plan's five chaos scenarios (§6 there, stream D here) are:

| Scenario | Primary fault | Works today? |
|---|---|---|
| C1 leader failure | `node-kill` | ❌ hard error |
| C2 dead worker rescheduled | `node-kill` | ❌ hard error |
| C3 minority partition | `/v1/chaos/partition` | ✅ real (blocks gossip + Raft transports) |
| C4 resource exhaustion | `cpu` + `memory` | ✅ on Linux |
| C5 node death during deploy | `node-kill` | ❌ hard error |

`apply_fault`'s `NodeDrain | NodeKill` arm returns *"is a cluster-level operation and cannot be applied as a node-local fault"* (`agent.rs:3299-3312`). That refusal is correct and honest — CHAOS1 made it so rather than let it lie — but it means **60 % of the chaos suite is unimplementable until node-level faults exist**.

So chaos completion (stream C) must land **before** the chaos suite (stream D). The old plan had them as one step near the end.

---

## 3. The chaos gap, itemised

From `docs/design/chaos-smoker.md`, `docs/design/cli-relish.md:760-782`, `docs/whitepaper.md:768-773`.

### 3.1 Commands that don't match the docs

| Documented | Today | Fix |
|---|---|---|
| `relish fault run <file>` | `relish fault scenario <file>` | alias `run`, keep `scenario` |
| `relish fault clear <app>` | `clear [id: u64]` only | accept a name; `FaultRegistry::clear_by_service` (`registry.rs:88`) is **dead code** waiting for it |
| `relish fault pause --resume` | absent | `FaultType::Resume` exists and the agent implements it (`agent.rs:3138`) — **no CLI path constructs it** |
| `relish chaos council-partition` | bare `String` action, no flags, 30s hardcoded | promote to typed subcommands under `fault` |

### 3.2 Flags whose plumbing exists but aren't exposed

`--instance`, `--node`, `--reason` are on `FaultRequest`/`FaultRule` (`types.rs:397-405`), honoured by `target_pids` (`agent.rs:3348`) and `target_instance_cgroups` (`:3384`) — **and hardcoded to `None` in all 15 CLI handlers** (e.g. `fault.rs:136-142`). Dead code from the CLI down.

`--override-safety` is hardcoded `false` everywhere, so the one *designed-as-overridable* rail (`check_node_percentage`, `safety.rs:164`) can't be overridden.

### 3.3 Faults that can't take effect

- **`node-drain` / `node-kill`** — always error. Needed by C1/C2/C5.
- **`memory <app> oom`** — always rejected (`agent.rs:3219`), though the design's own example scenario uses it (`chaos-smoker.md:1170`).
- **`partition`** — the last *silent* no-op: succeeds and does nothing without eBPF (`agent.rs:3313-3335`), with an explicit `TODO(Phase 15)` to fix here.
- `delay`/`drop`/`bandwidth` honestly reject without eBPF; `cpu`/`memory`/`disk-io` reject off Linux. Both fine — capability-gate and skip.

### 3.4 Design features absent

- **`[smoker]` config section** (`default_duration`, `max_duration`) — doesn't exist. Default 10m is hardcoded CLI-side (`fault.rs:13`); the only ceiling is a hardcoded 24h clamp (`types.rs:302`), not the designed 1h with a rejection message.
- **`fault-injection` permission** — absent. `Deployer` can inject; the design explicitly forbids it.
- **Leader-mediated distribution** — the design routes faults leader → target nodes via the reporting tree (`chaos-smoker.md:84-91`). Implementation is node-local.
- **Audit event `fault.injected`** — absent. `injected_by` comes from client-side `$USER` (`fault.rs:139`), i.e. spoofable; `source_ip` isn't even a field.
- **Unix-socket `relish fault clear` fallback** for a degraded API — absent.
- Startup sweep of stale kernel fault maps — absent (relies on the in-kernel `expires_ns`).

### 3.5 Cosmetic bugs worth fixing in passing

`fault dns <app> <type>` ignores its positional (`fault.rs:175`), so `dns redis banana` injects NXDOMAIN. `Bandwidth`/`DiskIoThrottle` `Display` divides by 1024² and calls it "mbps" while the parser correctly reads megabits (`types.rs:152`), so `bandwidth api 1mbps` echoes `0mbps`. `fault list` column widths are off by one label (`fault.rs:419`).

---

## 4. Decisions — all four settled (26 July 2026)

**Resolved: option (a) on every one.** The recommendations below are now the
plan of record; step 13b is in scope, and `--node` routing, the
`fault-injection` permission, the `relish chaos` deprecation and
transport-quiesce node kills all get built.

**D1. Cluster-wide fault routing.** Faults are node-local. Options: (a) **implement `--node` routing via the leader's reporting tree, as designed** — makes `node-kill` meaningful and is what "inject into the live cluster" implies; (b) keep node-local and document the deviation. (a) is more work (~1 extra step) but (b) leaves `target_node` dead and the chaos suite weak.

**D2. Fault authorisation.** Design says admin-or-`fault-injection`-grant; today any Deployer. Options: (a) **add the `fault-injection` permission and require it**, a small security fix in the same family as C3; (b) leave as Deployer and amend the design doc. (a).

**D3. `relish chaos` fate.** It's undesigned, stringly-typed, and overlaps `fault`. Options: (a) **promote `council-partition`/`worker-isolation` to typed subcommands under `relish fault` and keep `relish chaos` as a deprecated alias for one release**; (b) delete it; (c) leave it. (a) — it's used by existing chaos tests.

**D4. Scope of node-kill.** "Kill a node" on a real cluster means stopping that node's `bun`. Options: (a) **stop the agent's cluster transports (gossip + Raft + reporting) for the duration and restore on expiry** — reversible, testable, no supervision needed; (b) actually SIGKILL the process, requiring an external supervisor to restart it. (a) — it's what C1/C2/C5 actually assert (leader re-election, rescheduling, membership transitions) and it stays inside Smoker's reversibility guarantee.

---

## 5. Architecture (revised)

```
src/
  testkit/                       # NEW — compiled-in cluster test/bench harness
    mod.rs report.rs registry.rs context.rs runner.rs
    cases/ …                     # the integration catalogue
    chaos/ …                     # the 5 scenarios + preflight
    bench/ mod.rs report.rs compare.rs suites.rs
  smoker/
    node_fault.rs                # NEW — node-kill/drain: transport quiesce + restore
    types.rs                     # MODIFIED — SmokerConfig limits
  relish/
    test_cmd.rs bench_cmd.rs wtf.rs trace.rs   # NEW command bodies
    fault.rs                     # MODIFIED — new flags, run alias, clear-by-app, resume
  firewall/evaluate.rs           # NEW — pure policy evaluation for trace step 3
  onion/trace.rs                 # NEW — run_trace
  bun/
    capabilities.rs              # NEW — ClusterCapabilities (NOT `Capabilities`)
    api.rs                       # MODIFIED — /v1/capabilities, /v1/trace
    agent.rs                     # MODIFIED — node fault arms, audit event
    testapp.rs                   # MODIFIED — request-line parsing, /payload, /env/
  config/node.rs                 # MODIFIED — [cluster] environment, [smoker] section
  bin/{bun,relish}.rs            # MODIFIED — subcommands
tests/support/                   # MODIFIED — promote TestHarness for reuse
```

Division of labour unchanged from the original: `testkit` and `wtf` and `bench` run **client-side** through `BunClient`, exactly as an operator would; `trace` is **server-side** (`/v1/trace`) because only the node has the service map, firewall state and network locality.

---

## 6. Work streams and steps

Twenty steps. Each: **(a)** failing tests, **(b)** implementation, **(c)** book section, **(d)** `make ci` + any gated suite the change touches → commit.

Commit convention: `<summary> (Phase 15, N/20)`.

> **Gating reminder** (learned the hard way on PR #141): `make ci` runs the portable suite only. Match the gated target to what you touched — gossip/dissemination → `make bench-10k`; council/Raft/placement → `make test-cluster`; agent/deploy timing → `make test-slow`; runc/netns/eBPF → `sudo make test-linux`. Chaos work touches the agent and the cluster plane: run `make test-cluster` on every stream-C and stream-D commit.

### Stream A — Foundations (steps 1–3)

**1/20 — `ClusterCapabilities` endpoint + `[cluster] environment`**
Tests: `capabilities_reports_wired_subsystems` (mayo=Some/council=None → `"metrics":true,"council":false`), `capabilities_reports_environment_tag`, `capabilities_defaults_to_no_environment`, client round-trip.
Implement: `src/bun/capabilities.rs` — `ClusterCapabilities` derived field-by-field from `ApiState` (§7.1); `environment: Option<String>` on `ClusterSection`; `GET /v1/capabilities` (protected, add to `authz::ROUTE_MATRIX` — the source-scan test fails otherwise); `BunClient::capabilities()`.
Book: §"Asking the cluster what it can do".

**2/20 — testapp: request-line parsing, `/payload`, `/env/`, `alloc` mode**
Tests: `payload_endpoint_serves_requested_bytes`, `env_endpoint_returns_variable_or_404`, `unknown_path_still_answers_healthy` (don't regress the answer-everything behaviour the existing tests rely on), `alloc_mode_holds_memory`.
Implement: parse the request line in `testapp.rs`'s TCP loop (it currently only logs it); add `TestAppMode::Alloc(usize)`; add a `Testapp` subcommand to `src/bin/bun.rs` delegating to the same library as `src/bin/testapp.rs`; expose `exit-after` in the standalone binary's `--mode` parser (currently missing).
Book: §"A workload in your pocket".

**3/20 — Promote `TestHarness` into `tests/support/`**
Tests: existing suites keep passing.
Implement: move `TestHarness` from `tests/integration.rs` into `tests/support/harness.rs`, make it `pub(crate)`-visible to all integration tests, update `tests/integration.rs` to use it. No behaviour change.
Book: none (mechanical).

### Stream B — `relish test` (steps 4–8)

**4/20 — testkit core: report types, registry, filters** (as original step 3; §7.2)
**5/20 — runner: parallelism, timeouts, teardown, skips** (as original step 4; `Semaphore` + `JoinSet`; **no `start_paused`** — standing pitfall)
**6/20 — `relish test` command + `CommandOutcome`** (as original step 5, plus: give the four new commands `Result<CommandOutcome, RelishError>` so exit codes 0/1/2 are expressible)
**7/20 — catalogue part A** — scheduling, deployments, health-checks, process-workloads, jobs (**15 cases**, three per group — the "18" was a miscount; §7.7 enumerates 3×5)

> **Deviations recorded during step 7** (source wins; these correct the plan):
> - **Workload = `testapp` as a *process* workload, not an OCI image.** `relish dev`
>   nodes run runc, where a `proc-grill` command fails, and packaging `testapp` as an
>   image is infeasible on dev nodes (no builder, loopback-only registry, glibc-dynamic
>   binary). Since `testapp` is a `bun` subcommand and `bun` is at `/usr/local/bin/bun`
>   on every node, `testapp_spec` emits `command = ["/usr/local/bin/bun", "testapp", …]`
>   and `relish dev create` gained a `--runtime process` flag (default stays `runc`).
>   Cases gate on the new **`Capability::ProcessRuntime`** and skip on a runc cluster.
> - **Runtime skip mechanism.** A case can `skip(reason)` from its body for conditions
>   `requires` can't express (no labelled node, etc.); the runner maps the marker to
>   `Skipped`, not `Failed`.
> - **Cluster-wide fan-out.** `/v1/status` is node-local (no aggregated endpoint), so
>   `TestContext` grew `node_clients`/`cluster_instances`/`wait_running_cluster`/
>   `wait_for_cluster` that fan out to each node (API address = gossip IP + entry node's
>   API port).
> - **Config facts:** `JobSpec` has no `retries` field; `deploy_history` returns untyped
>   `Vec<serde_json::Value>`; `DeployResult` serialises PascalCase (`"Completed"`).
> - Cases run only against a live cluster (acceptance milestone); `make ci` covers the
>   catalogue-integrity + helper + `testapp_spec` unit tests.
**8/20 — catalogue part B** — service-discovery, secrets-config, firewall, workload-identity, ingress, volumes, image-registry, cluster-coordination (21 cases). Most of these are now *actually testable* — the wiring landed in 12b.

### Stream C — Complete the chaos surface (steps 9–13) — **NEW**

**9/20 — Fault targeting flags and CLI fidelity**
Tests: `fault_request_carries_instance_node_and_reason` (each flag reaches the POSTed body); `clear_by_app_removes_only_that_apps_faults` (gives `clear_by_service` its first caller); `run_is_an_alias_for_scenario`; `dns_rejects_an_unknown_fault_type` (today it silently injects NXDOMAIN); `bandwidth_display_round_trips_megabits` (today `1mbps` echoes `0mbps`).
Implement: `--instance`, `--node`, `--reason` on every fault subcommand; `--override-safety` on those where the rail is designed as overridable; `fault run` alias; `clear <app|id>`; `fault resume <app>` constructing `FaultType::Resume`; fix the `dns` positional, the `Display` unit bug and the `list` column widths.
Book: §"The flags that were already wired".

**10/20 — `[smoker]` config: default and maximum duration**
Tests: `smoker_config_defaults_to_ten_minutes_and_one_hour`; `fault_exceeding_max_duration_is_rejected_with_both_values`; `fault_without_duration_uses_the_configured_default`; `max_duration_is_clamped_to_the_hard_ceiling` (the 24h `types.rs` clamp stays as a backstop).
Implement: `[smoker] default_duration`, `max_duration` in `src/config/node.rs`; enforce server-side in the inject handler (not just CLI-side, which a direct API call bypasses); keep the 24h clamp beneath it.
Book: §"Every fault must expire".

**11/20 — Node-level faults (D4: transport quiesce)**
Tests: unit — `node_kill_quiesces_all_cluster_transports`, `node_kill_restores_on_expiry`, `node_drain_stops_scheduling_but_keeps_gossip`, `node_fault_refuses_without_a_duration`; gated cluster (`tests/chaos_node.rs`, `RELIABURGER_CLUSTER_TESTS=1`) — `killed_leader_is_replaced_and_rejoins`.
Implement: `src/smoker/node_fault.rs` — a reversible quiesce that blocks gossip, Raft and reporting transports (the mechanism `/v1/chaos/partition` already uses at `agent.rs:2505`, generalised), with `FaultReversal::NodeQuiesce`; replace the `NodeDrain | NodeKill` refusal arm; `node-drain` additionally cordons the node (the scheduler's `apply_upgrade_cordon` seam from Phase 14 already exists).
Book: §"Killing a node without killing the process".

**12/20 — Partition stops lying; memory-oom decision**
Tests: `partition_without_ebpf_is_refused_not_silently_dropped`; move the quorum-rail acceptance test onto an eBPF node or a mock so the `TODO(Phase 15)` no-op can go; `memory_oom_is_implemented_or_refused_consistently`.
Implement: remove the silent-success `Partition` arm — honest error without eBPF, like `delay`/`drop`. Decide `memory oom`: implement via `memory.max` squeeze **or** remove it from the docs; don't leave documented-but-rejected.
Book: §"The last fault that lied".

**13/20 — Fault authorisation and audit (D2)**
Tests: `deployer_cannot_inject_without_the_grant`; `admin_can_inject`; `fault_injection_permission_grants_a_deployer`; `injected_faults_emit_an_audit_event_with_the_authenticated_principal`.
Implement: `fault-injection` permission; `require_fault_injection` in the inject/clear handlers; `ROUTE_MATRIX` updated; `injected_by` taken from the **authenticated principal server-side**, not the client's `$USER`; emit `fault.injected` into the event store with principal, target, type, duration.
Book: §"Who may break production".

*(If D1 = implement routing, insert **13b/20 — leader-mediated fault distribution**: `--node` routes through the leader's reporting tree; `target_node` stops being dead.)*

### Stream D — `relish test --chaos` (step 14)

**14/20 — Chaos suite with production guards**
Tests: pure `chaos_preflight(caps, flags, is_tty) -> Result<(), RefusalReason>` — `refuses_fewer_than_three_nodes`, `refuses_production_without_override`, `allows_production_with_override`, `requires_yes_when_not_a_tty`; integration — `--chaos` against the 1-node harness exits 1 naming the 3-node requirement.
Implement: the five scenarios (now genuinely runnable), `ChaosGuard` that always clears the faults **it injected** (track ids — never a blanket clear), `--chaos`/`--override`/`--yes`.
Book: §"Chaos with a safety catch".

### Stream E — `relish bench` (steps 15–16)

**15/20 — bench schema + regression comparison** (direction-aware, 10 % threshold, `schema_version` guard)
**16/20 — the seven suites** (`--quick` scaling, per-suite timeout, skip plumbing, `--capacity` opt-in)

### Stream F — `relish wtf` (steps 17–18)

**17/20 — pure `diagnose()` + pattern catalogue** (13 patterns, one `check_*` fn each, crashloop→deploy correlation)
**18/20 — `collect()` fan-out + CLI** (parallel, 10s per-request timeouts, `--app`, `--watch`, exit 0/1/2)

### Stream G — `relish trace` (steps 19–20)

**19/20 — `/v1/trace` endpoint + pure firewall evaluator**
**20/20 — `relish trace` CLI + phase close-out** (progress.md, both READMEs, chapter 15 final pass, this plan's checklist)

---

## 7. Data structures

Unchanged from the 6 July plan except where noted. Reproduced here only where the revision changes them.

### 7.1 `ClusterCapabilities` — `src/bun/capabilities.rs`

> **Deviation recorded during step 1.** The field list below dropped
> `fault_injection` as a standalone flag and gained `cgroup_faults`, because
> the Smoker API is always mounted — a bare `fault_injection: true` would
> have told a caller nothing. It is now *derived*
> (`cgroup_faults || ebpf || cluster`), i.e. "can any fault take real effect
> here", with `cgroup_faults` exposed separately for finer gating. Also added
> `events` and `upgrade` (both `ApiState` `Option`s worth reporting), and
> `StaticCapabilities` / `WiredSubsystems` split the inputs so the derivation
> is a pure function that unit-tests without an `ApiState`.

**Renamed** from `Capabilities` to avoid colliding with `NodeCapabilities` (`meat/cluster_state.rs:15`) and `PlatformCapabilities` (`bun/supervisor.rs:74`).

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClusterCapabilities {
    pub version: String,
    pub environment: Option<String>,
    pub container_runtime: String,
    pub cluster: bool,
    pub node_count: u32,
    pub metrics: bool,        // ApiState.mayo.is_some()
    pub logs: bool,           // ApiState.log_store.is_some()
    pub rollups: bool,        // ApiState.rollup_store.is_some()
    pub council: bool,        // ApiState.council.is_some()
    pub registry: bool,       // ApiState.pickle_catalog.is_some()
    pub events: bool,         // ApiState.events.is_some()
    pub upgrade: bool,        // ApiState.upgrade.is_some()
    pub fault_injection: bool,
    pub ebpf: bool,
    pub ingress: bool,
    pub firewall: bool,
    pub identity: bool,
    pub process_workloads: bool,
}
```

Every field derives from `ApiState` or config — no hardcoded `true`. `Capability` (what a *test* requires) stays as specified, in `src/testkit/mod.rs`, with `MultiNode => node_count >= 3`.

### Carried forward verbatim (7.2 – 7.12)

Everything below this line is carried verbatim from the 6 July plan and is still
correct. Two notes before it:

- `TestFn = fn(TestContext) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>`
  is still the shape (async fn pointers don't exist in Rust). Good book moment for
  `Pin` and boxed trait objects.
- The `CorrelatedEvent` deviation note still applies, **but** `ApiState.events` now
  exists (`EventStore`), so correlation can draw on real events rather than only
  deploy history. Prefer the event store; keep deploy history as a fallback.

### 7.2 Test runner types — `src/testkit/report.rs`, `registry.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TestGroup {
    Scheduling, ServiceDiscovery, Deployments, HealthChecks, SecretsConfig,
    Firewall, WorkloadIdentity, Ingress, Volumes, ProcessWorkloads, Jobs,
    ImageRegistry, ClusterCoordination,
}
// impl FromStr (kebab-case; error lists all valid names) and Display.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TestOutcome {
    Passed,
    Failed { message: String },
    Skipped { reason: String },
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCaseResult {
    pub name: String,
    pub group: TestGroup,
    pub outcome: TestOutcome,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReport {
    pub schema_version: u32,             // 1
    pub started_at: String,              // RFC 3339
    pub duration_ms: u64,
    pub cluster_nodes: u32,
    pub chaos: bool,
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub results: Vec<TestCaseResult>,
}
```

A test case. `async fn` pointers don't exist in Rust, so store a plain `fn` returning a boxed future (book moment — explain `Pin<Box<dyn Future>>`):

```rust
pub type TestFn = fn(TestContext) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

pub struct TestCase {
    pub name: &'static str,              // e.g. "schedule_fixed_replicas_across_nodes"
    pub group: TestGroup,
    pub requires: &'static [Capability],
    pub run: TestFn,
}

pub fn all_cases() -> Vec<TestCase>;     // concatenates every cases/*.rs module
pub fn chaos_cases() -> Vec<TestCase>;   // the 5 chaos scenarios

/// Parse "--filter scheduling,firewall" and select cases.
pub fn parse_filter(s: &str) -> Result<Vec<TestGroup>, String>;
pub fn select(cases: Vec<TestCase>, groups: &[TestGroup]) -> Vec<TestCase>;
```

Each case body is written as an ordinary `async fn` plus a tiny wrapper:

```rust
fn boxed<F>(f: fn(TestContext) -> F) -> TestFn where F: Future<Output = Result<(), String>> + Send + 'static
```

(If the generic wrapper fights the type system, fall back to a `case!` macro_rules that wraps `Box::pin(body(ctx))`. Keep it simple; no external crates.)

### 7.3 TestContext — `src/testkit/context.rs`

```rust
pub struct TestContext {
    pub client: BunClient,
    pub namespace: String,               // "rbtest-{run_id}-{seq:02}" — unique per test
    pub capabilities: Capabilities,
    pub timeout: Duration,               // per-test budget (from --timeout)
}

impl TestContext {
    /// Apply a TOML app config into this test's namespace.
    pub async fn apply(&self, toml: &str) -> Result<(), String>;
    /// Poll /v1/status until `app` has `replicas` Running instances, or fail at deadline.
    pub async fn wait_running(&self, app: &str, replicas: u32) -> Result<(), String>;
    /// Poll until a predicate on the app's status is true.
    pub async fn wait_for<F: Fn(&AppStatus) -> bool>(&self, app: &str, what: &str, pred: F) -> Result<(), String>;
    /// TOML for a testapp process workload in this namespace.
    pub fn testapp_spec(&self, app: &str, mode: &str, replicas: u32) -> String;
    /// Best-effort teardown: stop every app in this namespace. Never errors.
    pub async fn teardown(&self);
}
```

`run_id`: lowercase hex of seconds-since-epoch (6–8 chars) generated once per `relish test` invocation. Namespaces are how tests stay production-safe: everything a test creates carries its namespace, and teardown stops that namespace only. Teardown **always runs**, pass or fail or timeout (the runner owns this, not the case body).

Polling helpers use a 500 ms interval and the context deadline; every HTTP call goes through `tokio::time::timeout`.

### 7.4 Bench types — `src/testkit/bench/report.rs`, `compare.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchReport {
    pub schema_version: u32,             // 1
    pub started_at: String,
    pub duration_ms: u64,
    pub cluster_nodes: u32,
    pub quick: bool,
    pub metrics: Vec<BenchMetric>,
    pub skipped: Vec<SkippedSuite>,      // { name, reason }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchMetric {
    pub name: String,                    // "deploy_speed", see §7
    pub value: f64,
    pub unit: String,                    // "s", "ms", "MiB/s", "apps/s", "apps"
    pub higher_is_better: bool,
    pub samples: u32,
}

pub struct Regression { pub name: String, pub baseline: f64, pub current: f64, pub change_percent: f64 }

pub struct Comparison {
    pub regressions: Vec<Regression>,
    pub improvements: Vec<Regression>,   // same shape, opposite sign
    pub missing_in_current: Vec<String>,
    pub missing_in_baseline: Vec<String>,
}

/// Pure. `threshold` is 0.10. Direction-aware: for higher_is_better metrics a
/// regression is a drop; otherwise a rise. Metrics are matched by name; a
/// metric absent on either side is reported, not treated as a regression.
pub fn compare(baseline: &BenchReport, current: &BenchReport, threshold: f64) -> Comparison;
```

Baseline file = a previous `relish bench --output json` capture, verbatim. Reject `schema_version != 1` with a clear error.

### 7.5 Wtf types — `src/relish/wtf.rs` (structs match `docs/design/cli-relish.md` §4, with one deviation)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WtfReport {
    pub cluster_name: String,
    pub node_count: u32,
    pub critical: Vec<WtfFinding>,
    pub warnings: Vec<WtfFinding>,
    pub ok: Vec<WtfOk>,
    pub summary: WtfSummary,             // { critical_count, warning_count, ok_count }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WtfFinding {
    pub title: String,
    pub details: Vec<String>,
    pub suggestion: String,
    pub correlated_events: Vec<CorrelatedEvent>,
    pub affected_resource: String,       // "app.payments/default", "node.reliaburger-2"
}

/// DEVIATION from cli-relish.md: the design references a persistent `Event`
/// type which doesn't exist yet (events are streamed, not stored). Until an
/// event store lands, correlation draws on deploy history and alerts:
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelatedEvent {
    pub timestamp: String,
    pub kind: String,                    // "deploy", "alert", "restart", "log"
    pub message: String,
}
```

Record this deviation in `docs/design/cli-relish.md` (one short paragraph) as part of step 12.

The engine is split for testability:

```rust
/// Everything wtf collected, one field per source; None = source unavailable.
pub struct WtfInputs {
    pub capabilities: Option<Capabilities>,
    pub nodes: Option<Vec<NodeMembershipInfo>>,     // /v1/cluster/nodes
    pub council: Option<CouncilInfo>,               // /v1/cluster/council
    pub status: Option<ClusterStatus>,              // /v1/status
    pub alerts: Option<Vec<Alert>>,                 // /v1/alerts
    pub faults: Option<Vec<FaultSummary>>,          // /v1/fault
    pub deploy_history: Vec<DeployHistoryEntry>,    // /v1/deploys/history/{app}
    pub services: Option<Vec<ResolveEntry>>,        // /v1/resolve
    pub metrics_summary: Option<MetricsSummary>,    // /v1/metrics/summary
    pub recent_logs: HashMap<String, Vec<LogLine>>, // last 50 lines per unhealthy app
    pub unreachable_nodes: Vec<String>,             // fan-out failures
}

/// Pure: no I/O, fully unit-testable with fixtures.
pub fn diagnose(inputs: &WtfInputs) -> WtfReport;

/// I/O: parallel fan-out (tokio::join!), 10 s per-request timeout, fills
/// WtfInputs with None/unreachable on failure. Never returns Err for a
/// down subsystem — only for "cannot reach any node at all".
pub async fn collect(client: &BunClient, app: Option<&str>) -> Result<WtfInputs, RelishError>;
```

Reuse existing response structs from `src/relish/client.rs` / `src/bun/api.rs` rather than redefining them; add `pub` or move types if needed.

### 7.6 Trace types — shared, so put them in `src/onion/trace.rs` or `src/bun/api.rs`; match the design doc exactly

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceResult {
    pub source: String,
    pub destination: String,
    pub destination_port: u16,
    pub steps: Vec<TraceStep>,
    pub overall_result: TraceVerdict,
    pub latency_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceStep {
    pub step_number: u32,
    pub name: String,                    // "Service resolution", "Backend health", "Firewall", "TCP probe"
    pub details: Vec<String>,
    pub verdict: TraceVerdict,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "verdict")]
pub enum TraceVerdict {
    Pass,
    Fail { reason: String },
}
```

---

### 7.7 The test catalogue (39 cases)

Names are behaviour sentences (project convention). Each case: apply config → wait → assert via API → runner tears down the namespace. `requires` lists capabilities beyond the implicit ones. If an assertion depends on wiring that hasn't landed (per `docs/plans/2026-07-02-review-codebase.md`), gate it on the capability and skip — never write a test that asserts broken behaviour just to pass.

**scheduling** (requires: `Cluster`, `MultiNode` for #1)
1. `schedule_fixed_replicas_across_nodes` — 3 replicas on ≥3 nodes land on ≥2 distinct nodes (guards review item H8), all Running.
2. `schedule_respects_required_placement_label` — app with `[placement] required` runs only on the matching node (`/v1/status` shows the node).
3. `schedule_rejects_app_exceeding_namespace_quota` — namespace with `max_apps = 1`, second app's apply fails with a quota error; first app untouched.

**service-discovery**
4. `resolve_returns_vip_and_healthy_backends` — deploy testapp ×2, `/v1/resolve/{name}` has a VIP and 2 healthy backends.
5. `resolve_reflects_scale_up` — scale 1→3, backends reach 3 within the deadline.
6. `stopped_instance_leaves_the_backend_list` — stop the app, resolve returns no healthy backends (or NotFound).

**deployments**
7. `rolling_deploy_replaces_image_without_losing_backends` — deploy v1 (2 replicas), redeploy v2; during the roll, resolve never reports 0 healthy backends (poll every 250 ms; guards H2); afterwards all instances new version.
8. `failed_deploy_rolls_back_automatically` — redeploy to `--mode unhealthy-after --count 0`; auto-rollback restores the previous spec; app healthy at the end.
9. `deploy_history_records_each_version` — after 7–8, `/v1/deploys/history/{app}` lists both deploys with outcomes.

**health-checks**
10. `unhealthy_app_is_restarted` — `unhealthy-after 3` with an HTTP health check: restart observed (restart count rises AND instance id changes — guards H1's masking).
11. `hanging_health_check_marks_instance_unhealthy` — `hang` mode → instance reaches Unhealthy within `interval × failures + margin`, agent stays responsive (`/v1/health` still answers; guards H3).
12. `slow_but_within_timeout_app_stays_running` — `slow 500` with a 2 s probe timeout: no restarts over 30 s (no flapping).

**secrets-config** (requires: `Identity`)
13. `encrypted_env_value_is_decrypted_in_workload` — apply app with `ENC[AGE:...]` env; testapp gains an `--echo-env VAR` mode or the case uses `exec` to read it; plaintext inside, ciphertext in `/v1/status` (guards C5d).
14. `config_file_is_mounted_with_contents` — `[[config_file]]` content readable in the workload via exec.
15. `cluster_pubkey_encrypt_roundtrip` — fetch pubkey via API, encrypt locally, deploy, decrypted value visible in workload.

**firewall** (requires: `Firewall`, `MultiNode`)
16. `allow_from_permits_listed_app` — B allows A; exec-curl from A to B's VIP succeeds.
17. `unlisted_app_is_denied` — C not in B's allow_from; connection from C fails.
18. `firewall_reflects_allow_from_change` — add C to B's allow_from, redeploy, connection now succeeds.

**workload-identity** (requires: `Identity`)
19. `workload_receives_spiffe_certificate` — identity endpoint/status shows a cert with the SPIFFE URI SAN for the app.
20. `jwks_endpoint_serves_signing_keys` — `/v1/identity/jwks` returns ≥1 key, well-formed JWK.
21. `namespace_scoped_token_is_rejected_elsewhere` — create token scoped to ns X; a write to ns Y gets 403.

**ingress** (requires: `Ingress`)
22. `ingress_routes_host_header_to_app` — app with `[ingress] host`; HTTP GET to the proxy with that Host reaches testapp.
23. `ingress_returns_502_when_no_backends` — stop the app; same request → 502.
24. `ingress_route_appears_in_routing_table` — `/v1/routes` lists the route with backend count.

**volumes**
25. `volume_data_survives_instance_restart` — write a file via exec, kill instance (fault kill or stop/start), file still there.
26. `volume_is_isolated_per_app` — app B does not see app A's volume content.
27. `volume_size_limit_is_enforced` — write beyond the declared size fails (requires loop-mount enforcement; capability-gate on a `volumes_enforced` detail or skip with reason "M21 pending").

**process-workloads** (requires: `ProcessWorkloads`)
28. `process_workload_reports_running` — plain `testapp_spec` app reaches Running.
29. `process_job_with_exit_zero_succeeds` — job `sh -c 'exit 0'` → Succeeded.
30. `failing_job_retries_then_fails` — `sh -c 'exit 1'`, `retries = 2` → 3 attempts then Failed (guards H13 semantics on runc later).

**jobs**
31. `job_runs_to_completion_and_reports_exit` — batch job completes, status shows Succeeded + duration.
32. `scheduled_job_fires_on_its_schedule` — `schedule = "* * * * *"` (or the minimum supported) fires within ~70 s.
33. `job_logs_are_retrievable_after_completion` — `/v1/logs/...` returns the job's stdout after it finished (requires `Logs`; guards H10).

**image-registry** (requires: `Registry`)
34. `push_and_pull_image_roundtrip` — push a tiny synthetic OCI image (fixture built in-test: config blob + one layer) via Pickle API, pull it back, digests match.
35. `manifest_catalog_lists_pushed_image` — `/v1/images` includes it.
36. `deploy_from_cluster_registry` — deploy an app whose image comes from Pickle (runc runtime only; skip on others).

**cluster-coordination** (requires: `Cluster`, `Council`, `MultiNode`)
37. `all_nodes_report_alive` — `/v1/cluster/nodes`: every node Alive, count matches capabilities.
38. `council_has_leader_and_quorum` — `/v1/cluster/council`: exactly one leader, quorum true.
39. `every_node_answers_health` — hit each member's `:9117/v1/health` directly.

---

### 7.8 The chaos suite (5 scenarios)

Location: `src/testkit/chaos/scenarios.rs`. Runs through the same runner (they are `TestCase`s with `requires: &[Capability::MultiNode, Capability::FaultInjection, ...]`), selected by `--chaos`. Each scenario seeds workloads, injects via the fault API (`BunClient` → `/v1/fault`, `/v1/chaos/partition`), asserts recovery, then **clears all faults in teardown even on failure** (`DELETE /v1/fault` + `/v1/chaos/heal` — make `TestContext::teardown` chaos-aware or give scenarios a `ChaosGuard` RAII-ish struct whose cleanup the runner always awaits).

Safety preamble (in `chaos/mod.rs`, runs once before any scenario):
1. `node_count >= 3`, else refuse: `chaos suite requires at least 3 nodes (found {n})`.
2. Production guard: if `capabilities.environment == Some("production")` and `--override` absent → refuse, exit 1.
3. Confirmation: interactive TTY → prompt `This will inject real faults into cluster '{name}'. Type 'yes' to continue:`; non-TTY without `--yes` → refuse with a message naming `--yes`. (`--override` implies consent for production; `--yes` skips the prompt anywhere. Two flags, two meanings.)
4. Smoker's own `SafetyRails` still apply underneath; the suite never asks for more than they allow.

Scenarios (whitepaper SLA: full operability after leader loss < 20 s — assert with margin, 30 s):

- **C1 `leader_failure_elects_new_leader_and_cluster_recovers`** — identify leader via `/v1/cluster/council`; `node-kill` it (duration 60 s, auto-recover); assert a *different* leader within 30 s; deploy a canary app during the outage window and see it Running; after recovery the old node rejoins Alive.
- **C2 `dead_worker_node_has_workloads_rescheduled`** — deploy 3-replica app spread over workers; `node-kill` a worker hosting ≥1 replica; assert membership marks it Suspect→Dead (guards H4–H7) and desired replica count is restored on surviving nodes within 90 s.
- **C3 `minority_partition_degrades_and_heals`** — partition one node away (`/v1/chaos/partition`); majority still accepts a deploy; partitioned node either serves stale reads or refuses writes, but never a second leader (guards C3-review); heal; membership reconverges, no duplicate instances left behind.
- **C4 `resource_exhaustion_degrades_gracefully`** — inject `memory 90%` + `cpu 80%` on one worker; assert cluster-level API stays responsive, health checks on *other* nodes don't flap, and (if scheduler pressure-awareness exists) new replicas avoid the pressured node — otherwise just assert no false Dead; clear faults; node returns to normal.
- **C5 `node_death_during_deploy_ends_clean`** — start a rolling deploy of a 4-replica app, `node-kill` a worker mid-roll; assert the deploy terminates in a defined state (Complete or RolledBack, not stuck), replica count eventually correct, no orphaned instances in `/v1/status` (guards M16/H1).

Report: chaos results are ordinary `TestCaseResult`s inside a `TestReport` with `chaos: true`.

---

### 7.9 The bench suites

Client-side orchestration in `src/testkit/bench/suites.rs`, all through public APIs, each suite returning `Vec<BenchMetric>` or a skip reason. Namespace `rbbench-{run_id}`; teardown always.

| # | Metric name | Unit | higher_is_better | How | `--quick` |
|---|---|---|---|---|---|
| 1 | `deploy_speed` | s | no | Deploy testapp ×10 replicas; time apply → all Running. 3 samples, report median. | ×3 replicas, 1 sample |
| 2 | `scheduler_throughput` | apps/s | yes | Apply 30 one-replica apps back-to-back; time until every one has a node assignment in `/v1/status` (placement, not readiness). | 10 apps |
| 3 | `discovery_latency_p99` | ms | no | 200 sequential `GET /v1/resolve/{name}`; report p99. | 50 calls |
| 4 | `network_throughput` | MiB/s | yes | testapp gains `GET /payload?bytes=N` (step 2); fetch 64 MiB from a backend's host:port directly; bytes/elapsed. 3 samples, median. | 8 MiB, 1 sample |
| 5 | `reconstruction_time` | s | no | Requires `FaultInjection` + `MultiNode`: node-kill leader, time until new leader answers `/v1/cluster/council` with quorum. Skip otherwise. | skipped |
| 6 | `image_distribution` | s | no | Requires `Registry` (+replication wired): push ~16 MiB synthetic image, time until N nodes hold it. Skip until review L10 lands. | skipped |
| 7 | `cluster_capacity` | apps | yes | Only with explicit `--capacity` flag (deviation from design doc — deliberately saturating a cluster must be opt-in; note it in cli-relish.md): deploy minimal apps until scheduling fails; count; tear down. | never |

Rules: p-quantiles computed on sorted samples (no interpolation needed at these sizes); every suite has a hard `tokio::time::timeout` budget (5 min full, 60 s quick) — a suite that overruns reports itself skipped with reason "timed out", the harness moves on.

---

### 7.10 The wtf pattern catalogue

`diagnose()` walks this table top-to-bottom. Every pattern is one small pure function `fn check_x(inputs: &WtfInputs, out: &mut WtfReport)` so each gets its own unit test.

| ID | Severity | Detection | Suggestion (verbatim starting point) |
|---|---|---|---|
| `unreachable-nodes` | CRITICAL | `inputs.unreachable_nodes` non-empty | "check the node's bun agent and network: `relish dev shell <node>` / `journalctl -u bun`" |
| `no-leader` | CRITICAL | council reachable, no member is leader | "council has no leader; check quorum and recent elections: `relish council`" |
| `quorum-loss` | CRITICAL | reachable council members ≤ half of council size | "restore failed council nodes or remove them from the council" |
| `node-dead` | CRITICAL | membership state Dead/Suspect | "node {n} is {state}; if expected, drain it; if not, check the host" |
| `crashloop` | CRITICAL | app restart count ≥ 3 within 15 min (status/metrics) | "app {a} is crashlooping; last log line: {line}. If this started after a deploy, `relish rollback {a}`" |
| `no-backends` | CRITICAL | a resolved service has 0 healthy backends while desired replicas > 0 | "service {s} has no healthy backends; check health-check config and instance logs" |
| `deploy-stuck` | WARNING | `/v1/deploys/active` entry older than 15 min | "deploy of {a} has been running {t}; consider `relish rollback {a}`" |
| `active-faults` | WARNING | fault list non-empty | "{n} active fault(s) injected by Smoker; clear with `relish fault clear`" |
| `alerts-firing` | WARNING | `/v1/alerts` non-empty | pass through alert text |
| `disk-high` | WARNING ≥85 % / CRITICAL ≥95 % | node disk usage from metrics summary (skip silently if absent) | "node {n} disk at {p}%; prune images (`relish pickle gc`) or logs" |
| `cpu-throttling` | WARNING | app cpu at/over limit per metrics summary | "app {a} is at its CPU limit; raise the limit or scale out" |
| `source-unavailable` | WARNING | any `WtfInputs` field None that capabilities says should exist | "metrics store not answering on {n}" |
| `cert-expiring` | WARNING | identity capability on and any surfaced cert < 14 days (skip if no cert data exposed yet — leave `// TODO(review L18)`) | "certificate for {x} expires {d}; rotation should be automatic — check the CA" |

Correlation (the differentiator): when `crashloop` fires, look for a deploy of the same app in `deploy_history` within the last 30 min → attach it as a `CorrelatedEvent` and change the suggestion to name the version to roll back to; attach the first ERROR-level line from `recent_logs` for that app. When `no-backends` fires and a matching `crashloop`/`deploy-stuck` exists for the same app, reference it in `details` instead of raising a second suggestion.

OK entries (emit when the corresponding check passes and data was available): "all {n} nodes alive", "council quorum healthy ({m}/{c})", "no crashlooping apps", "no active faults", "no firing alerts", "all services have healthy backends".

`--app <name>` scoping: run only app-relevant patterns (crashloop, no-backends, deploy-stuck, cpu-throttling, alerts filtered to the app) plus fetch that app's recent logs; deeper (more log lines: 200).

`--watch`: re-collect + re-render every 30 s, clear screen with ANSI `\x1b[2J\x1b[H`, stop on ctrl_c; incompatible with `--output json` (reject with an error). Exit code in watch mode: 0 on interrupt.

---

### 7.11 Trace: endpoint and semantics

**Endpoint:** `GET /v1/trace?from={app}&namespace={ns}&to={dest}&port={port}` (protected) in `src/bun/api.rs`; assembly logic in a helper module so it stays testable (`src/onion/trace.rs`: `pub async fn run_trace(deps..., from, to, port) -> TraceResult`).

Steps (destination is a cluster app):
1. **Service resolution** — look up `to` in the Onion service map. Details: VIP, whether the eBPF interception layer is live (`capabilities.ebpf`; if false, note `"userspace service map (eBPF not active on this node)"` — still Pass). Fail: `service map has no entry for '{to}'`.
2. **Backend health** — list backends with health; Fail if 0 healthy: `service has {n} backends, none healthy`.
3. **Firewall verdict** — call `firewall::evaluate::evaluate_app_connection(&desired_specs, from, to, port)`. This is a **pure function over declared policy** (`allow_from` in app specs), not a live nftables query — say so in `details` (`"policy verdict (declared allow_from), not live nftables"`). If firewall capability is off: Pass with note `"firewall disabled — all traffic permitted"`. Fail: `DROP: '{to}' does not list '{from}' in allow_from`.
4. **TCP probe** — pick the first healthy backend, `tokio::time::timeout(5s, TcpStream::connect(addr))`, measure elapsed → `latency_ms`. Fail: refused/timeout with which address was tried. **Documented limitation** (put it in details and in the book): the probe originates from the bun agent process on the source node, not from inside the source app's network namespace, so app-netns-specific issues can escape it. `// TODO(v2): optionally probe via exec inside the source instance.`

Destination is an external host (contains a dot and isn't a known service): step 1 becomes system DNS (`tokio::net::lookup_host`), step 3 evaluates the egress allowlist if wired (else Pass + note), `--port` becomes mandatory (CLI validates).

`overall_result`: Fail if any step failed (first failure's reason); steps after a failed step are still attempted where meaningful (probe is skipped if there's no backend to probe).

**CLI side** (`src/relish/trace.rs`): resolve which node hosts a `from` instance via `/v1/status` (any instance will do; take the first), construct a `BunClient` for that node's address (node API addresses come from `/v1/cluster/nodes`), call `/v1/trace`, render:

```
$ relish trace web --to redis

  trace: web → redis:6379

  1. Service resolution        PASS   redis → 127.128.0.3 (userspace service map)
  2. Backend health            PASS   2/2 backends healthy
  3. Firewall                  PASS   allow_from includes 'web' (declared policy)
  4. TCP probe                 PASS   10.0.1.5:30891 in 1.3 ms

  result: PASS (1.3 ms)
```

Failures print the failing step in red with its reason and stop the summary at `result: FAIL — <reason>`. Snapshot-test this rendering with `insta` from a fixed `TraceResult` fixture.

---

### 7.12 CLI wiring details

New variants in `Command` (`src/bin/relish.rs`), reusing the **global** `--output` flag rather than adding per-command ones:

```rust
/// Run the built-in integration test suite against the cluster.
Test {
    /// Comma-separated groups, e.g. "scheduling,firewall". Omit for all.
    #[arg(long)] filter: Option<String>,
    /// Maximum concurrently running tests.
    #[arg(long, default_value_t = 4)] parallel: usize,
    /// Per-test timeout, e.g. "120s", "5m".
    #[arg(long, default_value = "120s")] timeout: String,
    /// Run the chaos suite instead of the integration suite.
    #[arg(long)] chaos: bool,
    /// Allow chaos against a cluster tagged environment = "production".
    #[arg(long = "override")] override_production: bool,
    /// Skip the interactive chaos confirmation prompt (for CI).
    #[arg(long)] yes: bool,
    /// Run all tests inside one fixed namespace instead of one per test.
    #[arg(long)] namespace: Option<String>,
},
/// Run the performance benchmark suite against the cluster.
Bench {
    /// Abbreviated suite (~2 minutes) for CI.
    #[arg(long)] quick: bool,
    /// Baseline report (JSON from a previous `relish bench --output json`).
    #[arg(long)] compare: Option<std::path::PathBuf>,
    /// Include the cluster-saturating capacity probe.
    #[arg(long)] capacity: bool,
},
/// Diagnose cluster health with root-cause correlation.
Wtf {
    /// Scope the diagnosis to a single app.
    #[arg(long)] app: Option<String>,
    /// Refresh continuously every 30 seconds.
    #[arg(long)] watch: bool,
},
/// Trace connectivity from an app to a destination, step by step.
Trace {
    /// Source app name.
    from: String,
    /// Destination app or external host.
    #[arg(long)] to: String,
    /// Destination port (defaults to the destination app's declared port).
    #[arg(long)] port: Option<u16>,
},
```

Exit codes: `main()` already returns `ExitCode`. Give the four command fns a richer return (`Result<ExitCode, RelishError>` or a small `CommandOutcome`) instead of shoehorning warnings into errors — wtf's 0/1/2 contract needs it. Human rendering: plain aligned text like existing commands (see `relish status`); ✓/✗/– glyphs and colour only when stdout is a TTY and `--no-colour` absent (match however the existing code handles colour; if it doesn't, plain text is fine — don't add a colour crate).

Duration parsing: reuse the parser in `src/relish/fault.rs` (move it to a shared `src/relish/util.rs` if visibility demands).

---

## 8. Corrections to the catalogues above

The sections immediately above are the 6 July text. Apply these corrections when
implementing them:

- **Fewer skips.** Ingress, workload-identity, firewall, image-registry, volumes and cluster-coordination groups were expected to skip on today's wiring. They mostly work now. Write them to run, and let the capability gate catch what doesn't.
- **Chaos scenarios C1/C2/C5 depend on step 11.** Do not attempt stream D before stream C.
- **Bench suite 6 (`image_distribution`)** was "skip until review L10 lands" — L10 landed. Implement it.
- **wtf pattern `cert-expiring`** was `// TODO(review L18)` — L18 landed; certificate data is available via the identity endpoints. Implement it.
- **Trace step 3** honesty note stands: it evaluates *declared* `allow_from` policy, not live nftables. Say so in `details` and in the book.

---

## 9. Acceptance runbook

After step 14 and again after step 20, on macOS with Lima:

```bash
relish dev create --nodes 3
relish test --output json | tee /tmp/test-report.json   # 0 failed; skips carry reasons
relish test --filter scheduling
relish fault kill web --count 1 --reason "runbook"      # new flags land in `fault list`
relish fault clear web                                  # clear-by-app
relish fault node-kill <node> --duration 60s            # real now
relish test --chaos --yes                               # 5 scenarios; `fault list` empty after
relish bench --quick --output json > /tmp/base.json
relish bench --quick --compare /tmp/base.json           # ~0 regressions against itself
relish wtf                                              # exit 0 healthy
relish fault kill <app> --count 1 && relish wtf         # finding appears; exit non-zero
relish trace <appA> --to <appB>
relish dev destroy
```

Record date, cluster size and observed skips at the bottom of this file.

---

## 10. Step checklist

- [x] 1/20 capabilities endpoint + `[cluster] environment`
- [x] 2/20 testapp routes + alloc mode + `bun testapp`
- [x] 3/20 promote `TestHarness`
- [x] 4/20 testkit core types
- [x] 5/20 runner
- [x] 6/20 `relish test` CLI + `CommandOutcome`
- [x] 7/20 catalogue A
- [ ] 8/20 catalogue B
- [ ] 9/20 fault targeting flags + CLI fidelity
- [ ] 10/20 `[smoker]` duration config
- [ ] 11/20 node-level faults
- [ ] 12/20 partition honesty + memory-oom decision
- [ ] 13/20 fault authorisation + audit
- [ ] 13b/20 leader-mediated distribution *(in scope — D1 resolved)*
- [ ] 14/20 chaos suite
- [ ] 15/20 bench schema + comparison
- [ ] 16/20 bench suites
- [ ] 17/20 wtf engine
- [ ] 18/20 `relish wtf` CLI
- [ ] 19/20 trace endpoint
- [ ] 20/20 `relish trace` + close-out
- [ ] acceptance runbook on a 3-node dev cluster

---

## 11. Risks and gotchas

Carried from the 6 July plan (all still apply): don't assert broken behaviour; `start_paused` + `tokio::spawn` don't mix; teardown is the runner's job; never block the runtime; timeout everything; JSON is API (`schema_version` + insta); namespace hygiene (`rbtest-*`/`rbbench-*`, never a blanket stop); cross-node calls need addresses from `/v1/cluster/nodes`; eBPF is capability data not a compile decision; the book says where we deviated from the design.

New for this revision:

12. **Chaos work touches the cluster plane.** Run `make test-cluster` (21 tests) on every stream-C/D commit, not just `make ci`.
13. **`ROUTE_MATRIX` is enforced by a test.** Every new route (`/v1/capabilities`, `/v1/trace`) needs a matrix entry or `matrix_covers_every_mounted_route` fails. Routes naming `{app}` additionally need `authorize_scoped` (the C3 drift guard).
14. **Don't reintroduce a silent no-op.** Step 12 exists because `Partition` still succeeds while doing nothing. Any new fault arm either works, or returns an error saying why — never `Ok(())` with no effect.
15. **`injected_by` is currently spoofable.** Until step 13, treat it as a hint, not an audit record.

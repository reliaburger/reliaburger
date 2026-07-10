# Phase 15 Implementation Plan — Testing, Benchmarking & Diagnostics

**Date:** July 2026
**Scope:** `relish test`, `relish test --chaos`, `relish bench`, `relish wtf`, `relish trace`, plus book Chapter 15 ("Ready for Production").
**Audience:** the implementing model. This plan is deliberately prescriptive: exact file paths, type names, function signatures, test names, and commit boundaries. When this plan and reality disagree (an API changed, a struct moved), verify against the source and prefer reality — then note the deviation in this file.

---

## 0. Ground rules (non-negotiable)

These come from CLAUDE.md and standing feedback. Follow all of them on every step:

1. **Tests first.** Every step starts by writing failing tests, then implements until green.
2. **`make ci` before every commit** (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`). All three must pass.
3. **Ask before committing.** Show the commit message and file list, wait for confirmation. Never `git commit --amend`; always a new commit.
4. **Update the book with every step.** Each step below names the section of `docs/book/15-ready-for-production.md` it must add. Do not batch book writing to the end. Book prose: British English, explain each new Rust concept on first appearance, target audience knows C/Python/Go but not Rust. Follow the writing style guide in CLAUDE.md (contractions, active voice, no "Notably,/Crucially," openers, minimal em dashes).
5. **Code conventions:** no `unwrap()`/`expect()`/`panic!()` in production code; `thiserror` in the library, `anyhow`-style context in binaries (this crate uses `RelishError` in the CLI — extend it, don't bypass it); `tokio::sync` primitives only; explicit `tokio::time::timeout` on every network operation; British English in doc comments, American in serde derives.
6. **Progress tracking:** after each step, tick the corresponding checkbox in §13 of this file. After the final step, update `docs/progress.md` Phase 15, `docs/README.md`, and the top-level `README.md` (test counts, new commands, chapter status).
7. **Do not touch `prompts.md`** (untracked user file at repo root).

Commit message convention: `<summary> (Phase 15, N/15)`, e.g. `Add cluster capabilities endpoint (Phase 15, 1/15)`.

---

## 1. What Phase 15 delivers

From `docs/roadmap.md` §Phase 15 and `docs/design/cli-relish.md` §5:

| Command | Purpose | Exit codes |
|---|---|---|
| `relish test` | Built-in integration suite (39 tests, 13 groups) run against a **live cluster**, namespace-per-test, parallel, filterable, JSON output | 0 pass, 1 any failure |
| `relish test --chaos` | Integration suite + Smoker fault injection: leader failure, node failure, partition, resource exhaustion, cascading failure. ≥3 nodes, confirmation prompt, production guard | 0 pass, 1 failure or refused |
| `relish bench` | In-binary benchmark harness: deploy stress workloads, measure, tear down, report; `--compare <file>` flags >10 % regressions | 0 ok, 1 regressions/failure |
| `relish wtf` | Automated cluster diagnosis with root-cause correlation and remediation suggestions | 0 ok, 1 criticals, 2 warnings only |
| `relish trace <app> --to <dest>` | Four-step connectivity trace: service map → backends → firewall verdict → real TCP probe | 0 pass, 1 any step failed |

**Milestone:** `relish test` runs the full suite green against a 3-node dev cluster; `relish bench` produces a valid report; `relish wtf` diagnoses seeded failures; all Phase 15 unit/integration tests pass; Chapter 15 written.

---

## 2. Context the implementer must know

### 2.1 Current code surface (verified July 2026)

- **CLI:** `src/bin/relish.rs` holds the clap `Cli`/`Command` enum (22 commands) and `main() -> ExitCode`. Command bodies live in `src/relish/commands.rs` (async fns returning `Result<(), RelishError>`). `RelishError` is in `src/relish/mod.rs`; `OutputFormat` (global `--output` flag: human/json) in `src/relish/output.rs`. API client: `src/relish/client.rs` — `BunClient` wrapping `reqwest`, default `http://127.0.0.1:9117`, bearer-token auth, 300 s default timeout.
- **Node API:** `src/bun/api.rs` — `pub fn router(...)` builds the axum app; `ApiState` carries `Option`s for mayo/ketchup/council/membership/etc. Protected routes sit behind `sesame::auth::auth_middleware`. Follow the existing handler pattern (extract `State<ApiState>`, return `Json<T>` or a typed error).
- **Test app:** `src/bun/testapp.rs` (library `TestApp`, `TestAppMode::{Healthy, UnhealthyAfter(u32), Hang, Slow(Duration)}`) plus a standalone binary `src/bin/testapp.rs`.
- **Smoker:** `src/smoker/` — `FaultType`, `FaultRequest`, `FaultRegistry`, `SafetyRails`. API: `POST/GET/DELETE /v1/fault`, `POST /v1/chaos/partition|heal`. CLI: `relish fault ...` (`src/relish/fault.rs`, which already has duration/percentage parsers — **reuse them**).
- **Dev clusters:** `relish dev create --nodes 3` (`src/relish/dev.rs`) builds Lima VMs, installs `bun`/`relish`, launches `bun --cluster ... --runtime runc`. This is the acceptance environment for the phase.
- **Benchmarks today:** criterion benches in `benches/gossip.rs` / `benches/gossip_large.rs`. These stay as-is; `relish bench` is a separate live-cluster harness.
- **Existing repo tests:** `tests/integration.rs` has a `TestHarness` (in-process agent + ProcessGrill + ephemeral-port API + `BunClient`). Reuse this pattern for Phase 15's own integration tests.

### 2.2 The July review and graceful degradation

`docs/plans/2026-07-02-review-codebase.md` found many subsystems are library-only (Smoker enforcement L14/L15, eBPF L8, ingress L7, rollups L11, GitOps L13, egress L16, …). That wiring is being done **separately** and may land before, during, or after this phase.

**Decision (user-confirmed): graceful skip.** Phase 15 tools detect unavailable subsystems at runtime and report them explicitly instead of failing:

- `relish test`: a test whose required capability is missing reports `SKIPPED (capability X unavailable)` — counted separately from pass/fail, never causes exit 1.
- `relish bench`: a suite whose dependency is missing is reported as skipped in the report.
- `relish wtf`: an unavailable data source becomes a WARNING finding ("metrics store not configured on node-2"), never a crash.
- `relish trace`: steps that can't be evaluated (e.g. firewall disabled) return `Pass` with an explanatory note in `details`, not a hard failure.

The mechanism is a new `GET /v1/capabilities` endpoint (step 1). Do **not** invent per-feature cargo flags; this is a runtime concern.

### 2.3 Topology decision

**Decision (user-confirmed): real multi-host clusters only.** `relish test`, `--chaos`, and `bench` are designed for and validated against genuine multi-node clusters (the Lima dev clusters count — each VM is a real Linux host running `bun --cluster --runtime runc`). We do not build a local multi-process simulation mode. Consequences:

- Repo-level integration tests (`tests/…`, run by `cargo test`) exercise the **runner machinery** (filtering, parallelism, timeouts, report schema, skip logic) against the single-node in-process `TestHarness` with a small synthetic suite — they do not run the full 39-test catalogue.
- Full-suite and chaos acceptance is a documented manual/CI runbook against `relish dev create --nodes 3` (§12).
- `relish test --chaos` hard-requires ≥3 nodes at runtime and refuses otherwise (this refusal *is* covered by an automated test).

### 2.4 Test workloads on real clusters

The design says "test apps are compiled into the Bun binary". The dev clusters run `--runtime runc`, and pulling public images inside tests would make the suite network-dependent. So:

- **Step 2** embeds the test app into `bun` itself as a `bun testapp` subcommand (the library `TestApp` already exists; the binary just needs a mode). Every cluster node then carries the test workload at `/usr/local/bin/bun`.
- Test specs use process workloads: `image = "proc-grill:image-ignored"`, `command = ["/usr/local/bin/bun", "testapp", "--mode", "healthy", "--port", "{port}"]`. **Checkpoint for the implementer:** verify how process workloads dispatch when the agent runs `--runtime runc` (see `src/bun/supervisor.rs` and the `[process_workloads]` config; review item M23 notes gaps). If process workloads are not runnable under a runc agent at implementation time, gate the affected groups on a `process_workloads` capability and skip gracefully; do not block the phase on it.
- Only the `image-registry` group uses real OCI images (pushed to Pickle from a fixture generated in-test), and it is capability-gated.

---

## 3. Architecture overview

```
src/
  testkit/                  # NEW — everything compiled-in that tests/benches the cluster
    mod.rs                  # pub use; TestGroup, Capability
    report.rs               # TestReport, TestCaseResult, TestOutcome (serde)
    registry.rs             # TestCase, all_cases(), filter matching
    context.rs              # TestContext: client, namespace, helpers, teardown
    runner.rs               # parallel executor: Semaphore + JoinSet + timeout
    cases/
      mod.rs
      scheduling.rs         # 3 cases
      service_discovery.rs  # 3
      deployments.rs        # 3
      health_checks.rs      # 3
      secrets_config.rs     # 3
      firewall.rs           # 3
      workload_identity.rs  # 3
      ingress.rs            # 3
      volumes.rs            # 3
      process_workloads.rs  # 3
      jobs.rs               # 3
      image_registry.rs     # 3
      cluster_coordination.rs # 3   (13 groups × 3 = 39 tests)
    chaos/
      mod.rs                # ChaosScenario type, safety preamble
      scenarios.rs          # 5 scenarios
    bench/
      mod.rs
      report.rs             # BenchReport, BenchMetric, serde
      compare.rs            # pure regression detection
      suites.rs             # the seven measurements
  relish/
    test_cmd.rs             # NEW — `relish test` command body + rendering
    bench_cmd.rs            # NEW — `relish bench` command body + rendering
    wtf.rs                  # NEW — collector + pure correlation engine + rendering
    trace.rs                # NEW — client side of `relish trace` + rendering
  firewall/
    evaluate.rs             # NEW — pure policy evaluation for trace step 3
  bun/
    api.rs                  # MODIFIED — /v1/capabilities, /v1/trace
    testapp.rs              # MODIFIED — new modes (alloc, payload endpoint)
  bin/
    bun.rs                  # MODIFIED — `bun testapp` subcommand
    relish.rs               # MODIFIED — Test/Bench/Wtf/Trace variants
  config/node.rs            # MODIFIED — [cluster] environment
```

Naming note: the CLI command files are `test_cmd.rs`/`bench_cmd.rs` because `test.rs` inside `src/relish/` invites confusion with `#[cfg(test)]` modules. Register modules in `src/relish/mod.rs` and `src/lib.rs` (`pub mod testkit;`).

Division of labour:

- **`testkit` runs client-side** (inside the `relish` process), talking to the cluster over `BunClient` exactly like a human operator. Tests are honest end-to-end checks, not in-process shortcuts.
- **`wtf` is client-side**: fan-out GETs to existing endpoints, then a **pure** correlation function (unit-testable with fixtures).
- **`trace` is mostly server-side**: a new `/v1/trace` endpoint on the node hosting the source instance assembles steps 1–4 (it has the service map, firewall state, and network locality for the probe). The CLI locates the right node, calls it, renders.
- **`bench` is client-side** orchestration using `testkit::context` helpers, plus pure comparison logic.

---

## 4. Data structures (write these verbatim, adjust only if compilation forces it)

All wire-crossing types live where noted, derive `Debug, Clone, Serialize, Deserialize`, and use `#[serde(rename_all = "snake_case")]` on enums.

### 4.1 Capabilities — `src/bun/api.rs` (or `src/bun/capabilities.rs` if api.rs is crowded)

```rust
/// What this node/cluster actually has wired up. Diagnostics and the test
/// runner consult this instead of guessing from errors.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub version: String,                 // env!("CARGO_PKG_VERSION")
    pub environment: Option<String>,     // [cluster] environment, e.g. "production"
    pub container_runtime: String,       // "process" | "runc" | "apple"
    pub cluster: bool,                   // running with --cluster
    pub node_count: u32,                 // from membership, 1 if standalone
    pub metrics: bool,                   // ApiState.mayo.is_some()
    pub logs: bool,                      // ApiState.log_store.is_some()
    pub rollups: bool,                   // ApiState.rollup_store.is_some()
    pub council: bool,                   // ApiState.council.is_some()
    pub registry: bool,                  // ApiState.pickle_catalog.is_some()
    pub fault_injection: bool,           // smoker enforcement wired (see step 1)
    pub ebpf: bool,                      // cfg!(feature = "ebpf") && loader active
    pub ingress: bool,                   // wrapper proxy listener bound
    pub firewall: bool,                  // firewall enabled in config
    pub identity: bool,                  // workload identity wired (wrapping IKM present)
    pub process_workloads: bool,
}
```

Route: `GET /v1/capabilities` (protected). Handler derives every field from `ApiState` and config — no hardcoded `true`. For subsystems whose wiring lands later (ingress, eBPF), return `false` today; the field flips when the wiring commit sets it. Leave a `// TODO(review L7/L8):` marker where that will happen.

`Capability` (what a test requires) — `src/testkit/mod.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Cluster, Metrics, Logs, Council, Registry, FaultInjection,
    Ebpf, Ingress, Firewall, Identity, ProcessWorkloads, MultiNode,
}

impl Capabilities {
    pub fn has(&self, c: Capability) -> bool { /* match, MultiNode => node_count >= 3 */ }
    pub fn missing(&self, wanted: &[Capability]) -> Vec<Capability> { ... }
}
```

### 4.2 Test runner types — `src/testkit/report.rs`, `registry.rs`

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

### 4.3 TestContext — `src/testkit/context.rs`

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

### 4.4 Bench types — `src/testkit/bench/report.rs`, `compare.rs`

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

### 4.5 Wtf types — `src/relish/wtf.rs` (structs match `docs/design/cli-relish.md` §4, with one deviation)

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

### 4.6 Trace types — shared, so put them in `src/onion/trace.rs` or `src/bun/api.rs`; match the design doc exactly

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

## 5. The test catalogue (39 cases)

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

## 6. The chaos suite (5 scenarios)

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

## 7. The bench suites

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

## 8. The wtf pattern catalogue

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

## 9. Trace: endpoint and semantics

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

## 10. CLI wiring details

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

## 11. Commit-sized steps (the actual plan)

Each step: **(a)** failing tests first, **(b)** implementation, **(c)** book section, **(d)** `make ci` green → show commit → confirm → commit.

### Step 1/15 — Capabilities endpoint and environment tag

- **Tests first:** in `src/bun/api.rs` tests or `tests/capabilities.rs`: `capabilities_reports_wired_subsystems` (build `ApiState` with mayo=Some, council=None → JSON has `"metrics":true,"council":false`); `capabilities_reports_environment_tag` (config with `[cluster] environment = "production"`); `capabilities_defaults_to_no_environment`. Client test: `BunClient::capabilities()` deserialises.
- **Implement:** `Capabilities` + `Capability` (§4.1); `[cluster] environment: Option<String>` in `src/config/node.rs` (plumb into `ApiState` — likely a new plain field set from config in `src/bin/bun.rs` and `tests` harness); `GET /v1/capabilities` route (protected); `BunClient::capabilities()`; `Capabilities::has/missing`.
- **Book:** open Chapter 15 — replace the stub with the chapter intro ("Why a cluster should test itself") + §15.1 on capability discovery and graceful degradation as a design stance. Explain `Option` fields on `ApiState` as Rust's answer to nullable wiring.
- **Commit:** `Add cluster capabilities endpoint (Phase 15, 1/15)`

### Step 2/15 — Embed the test app in bun; extend its modes

- **Tests first:** in `src/bun/testapp.rs`: `alloc_mode_grows_memory` (mode allocates and holds N MiB), `payload_endpoint_serves_requested_bytes` (GET `/payload?bytes=1048576` returns exactly that many bytes), `echo_env_endpoint_returns_variable` (GET `/env/FOO` returns the value or 404). Binary-level: `bun testapp --mode healthy` starts and answers `/` (spawn in a test with a random port, curl via reqwest, kill).
- **Implement:** extend `TestAppMode` with `Alloc(usize /* MiB */)`; add `/payload` and `/env/{name}` routes to the test app server regardless of mode; add a `Testapp` subcommand to `src/bin/bun.rs`'s clap (delegating to the same code as `src/bin/testapp.rs` — keep the standalone binary, both are thin wrappers over the library).
- **Book:** §15.2 "A workload in your pocket" — why the test workload ships inside the orchestrator binary (no registry dependence, version-locked to the cluster), and a short note on clap subcommands sharing a library core.
- **Commit:** `Embed testapp in bun with alloc and payload modes (Phase 15, 2/15)`

### Step 3/15 — Testkit core: types, registry, filters

- **Tests first** (`src/testkit/` unit tests): `empty_filter_selects_all_groups`, `filter_matches_single_group`, `filter_matches_multiple_comma_separated_groups`, `filter_rejects_unknown_group_naming_valid_ones` (error text contains "scheduling"), `report_counts_match_outcomes`, `test_report_serialises_to_stable_json` (insta snapshot of a fixed `TestReport`), `skipped_outcome_carries_reason`.
- **Implement:** §4.2 types, `parse_filter`, `select`, `all_cases()` returning an empty vec for now, `TestReport::from_results(...)` aggregation.
- **Book:** §15.3 "A test framework in three types" — walk through `TestOutcome` as an enum with data (contrast Go's `(bool, error)` and Python exceptions), serde `tag = "status"`, and why `TestFn` is `fn(...) -> Pin<Box<dyn Future>>` (first-time explanation of `Pin`, boxed trait objects, and why async fn pointers don't exist).
- **Commit:** `Add testkit report types, registry and filters (Phase 15, 3/15)`

### Step 4/15 — Runner: parallelism, timeouts, teardown, skips

- **Tests first** (unit tests with synthetic `TestCase`s built from closures over channels/AtomicU32 — no cluster): `runner_respects_parallel_limit` (track max concurrency with an atomic; assert ≤ limit), `runner_times_out_hung_test` (a case that `pending()`s forever → `TimedOut` within budget; use a short 100 ms timeout, **not** `start_paused` — remembered pitfall: `tokio::spawn` + `start_paused` don't mix), `runner_runs_teardown_after_failure` (teardown flag set even when case errs), `runner_skips_cases_missing_capabilities` (case requiring `Ebpf` against caps without it → `Skipped` and its body never ran), `runner_isolates_namespaces` (two cases see different `ctx.namespace`).
- **Implement:** `src/testkit/runner.rs` — `pub async fn run(cases, caps, client, opts: RunOptions) -> TestReport`. Structure: `tokio::sync::Semaphore` (permits = parallel) + `tokio::task::JoinSet`; each task: acquire permit → build `TestContext` (fresh namespace unless `--namespace` pins one) → `tokio::time::timeout(opts.timeout, (case.run)(ctx.clone()))` → always `ctx.teardown().await` → outcome. Capability check happens *before* spawning (skip synchronously). Live progress line per finished test via a `mpsc` channel to the caller (the CLI prints `  ✓ name (1.2s)` as results arrive).
- **Book:** §15.4 "Running forty tests without stepping on production" — namespaces as the isolation unit, `Semaphore` vs a worker pool, `JoinSet`, and why teardown lives in the runner not the test (compare Go's `t.Cleanup`).
- **Commit:** `Add parallel test runner with timeouts and teardown (Phase 15, 4/15)`

### Step 5/15 — `relish test` command

- **Tests first:** `tests/testkit_cli.rs` using the in-process `TestHarness` from `tests/integration.rs`: register a tiny synthetic suite (pass/fail/skip) through a test-only hook (`runner::run` takes the case list as a parameter — the CLI passes `all_cases()`, the test passes fixtures), assert: JSON output parses back into `TestReport`; exit code 1 when a case fails, 0 when only skips; `--filter` narrows; human output snapshot (insta, strip durations with a regex before snapshotting). Also `test_context_apply_and_wait_running` — a real end-to-end: `TestContext` deploys `testapp_spec` against the harness and `wait_running` succeeds (this validates the helpers against ProcessGrill).
- **Implement:** `src/relish/test_cmd.rs` (`pub async fn run_test(...) -> Result<ExitCode, RelishError>`), `Command::Test` variant, dispatch in `main`, human renderer (grouped by `TestGroup`, summary line `39 tests: 35 passed, 0 failed, 4 skipped (2m 14s)`), JSON = `serde_json::to_string_pretty(&report)`.
- **Book:** §15.5 "The front door" — CLI wiring, exit codes as API (`ExitCode`), rendering the same struct as human text and JSON.
- **Commit:** `Add relish test command with runner wiring (Phase 15, 5/15)`

### Step 6/15 — Case catalogue part A

- **Tests first:** the cases *are* tests; the repo-level guard is `tests/testkit_catalogue.rs`: `all_cases_have_unique_names`, `all_cases_declare_valid_groups`, `catalogue_covers_every_group` (once part B lands: exactly 39; for now assert the five part-A groups have 3 each). Where a case's logic has a pure core (e.g. "did replicas land on ≥2 nodes" from a status response), extract it and unit-test it with fixtures.
- **Implement:** cases 1–12 + 28–33 (scheduling, deployments, health-checks, process-workloads, jobs — the groups exercisable on today's wiring) in `src/testkit/cases/`. Use `TestContext` helpers only; every wait has a deadline; failure messages must say what was expected vs observed (`"expected 3 running replicas within 60s, saw 1 (instances: […])"`).
- **Book:** §15.6 "What we actually test" (first half) — pick two cases (7 rolling-deploy and 10 restart) and walk through them as exemplars: poll-with-deadline shape, asserting via the public API only.
- **Commit:** `Add scheduling, deploy, health, process and job test cases (Phase 15, 6/15)`

### Step 7/15 — Case catalogue part B

- **Tests first:** extend `catalogue_covers_every_group` to 13 groups / 39 cases; unit-test pure helpers (e.g. the synthetic OCI image fixture builder: `fixture_image_has_valid_digests`).
- **Implement:** cases 4–6, 13–27, 34–39 (service-discovery, secrets-config, firewall, workload-identity, ingress, volumes, image-registry, cluster-coordination) with their `requires` capability lists (§5). These are the groups most exposed to pending wiring — every one must skip cleanly on a bare cluster today and pass on a fully wired one later. Build the OCI fixture (deterministic tar layer + config JSON, sha256 via existing deps) for the registry group.
- **Book:** §15.6 (second half) — the graceful-skip story: show `relish test` output on a partially wired cluster, and why SKIPPED is honest where a green fake would rot.
- **Commit:** `Complete the 39-case integration catalogue (Phase 15, 7/15)`

### Step 8/15 — Chaos suite

- **Tests first:** unit: `chaos_refuses_fewer_than_three_nodes`, `chaos_refuses_production_without_override`, `chaos_allows_production_with_override`, `chaos_requires_yes_when_not_a_tty` (factor the guard into a pure `fn chaos_preflight(caps, flags, is_tty) -> Result<(), RefusalReason>`); integration (`tests/testkit_cli.rs`): `--chaos` against the 1-node harness exits 1 with the "requires at least 3 nodes" message.
- **Implement:** `src/testkit/chaos/` — preflight + the 5 scenarios (§6) + `ChaosGuard` teardown that always clears faults and heals partitions; `--chaos`, `--override`, `--yes` handling in `test_cmd.rs`.
- **Book:** §15.7 "Chaos with a safety catch" — production guards, the confirmation UX, and one scenario (C1 leader failure) end to end; connect back to Chapter 8's Smoker.
- **Commit:** `Add the chaos test suite with production guards (Phase 15, 8/15)`

### Step 9/15 — Bench core and CLI skeleton

- **Tests first:** unit (`src/testkit/bench/`): `compare_flags_regression_over_threshold` (11 % worse → flagged), `compare_ignores_changes_within_threshold` (9 % → not), `compare_respects_metric_direction` (throughput drop = regression; latency drop = improvement), `compare_reports_missing_metrics_without_failing`, `baseline_rejects_unknown_schema_version`, `bench_report_serialises_to_stable_json` (insta).
- **Implement:** §4.4 types, `compare`, baseline load (`std::fs` is fine in the CLI path — it's not the agent runtime; use `tokio::fs` if already conventional), `src/relish/bench_cmd.rs` + `Command::Bench` with an empty suite list, human renderer for report + comparison table (`name  baseline  current  Δ%  verdict`), exit 1 on any regression.
- **Book:** §15.8 "Measuring without lying" (first half) — regression math, direction-aware comparison, why the threshold is 10 % and per-metric noise matters.
- **Commit:** `Add bench report schema and regression comparison (Phase 15, 9/15)`

### Step 10/15 — Bench suites A

- **Tests first:** pure helpers unit-tested: `p99_of_sorted_samples`, `median_of_samples`; integration: `bench_quick_produces_valid_report_against_harness` — `--quick` against the in-process harness runs suites 1–3 (tiny sizes) and yields a parseable report with ≥3 metrics.
- **Implement:** suites 1–3 (deploy_speed, scheduler_throughput, discovery_latency_p99) in `suites.rs`, `--quick` scaling, per-suite timeout envelope, live progress lines.
- **Book:** §15.8 (second half) — walk suite 1: what "deploy speed" includes and excludes and why medians beat means here.
- **Commit:** `Add deploy, scheduler and discovery bench suites (Phase 15, 10/15)`

### Step 11/15 — Bench suites B

- **Tests first:** `network_throughput_computes_mib_per_second` (pure maths from bytes+duration), `capacity_suite_requires_explicit_flag`, plus skip-reason assertions for suites 5/6 against a caps struct without fault-injection/registry.
- **Implement:** suites 4–7 (network via `/payload`, reconstruction via leader kill, image distribution, capacity behind `--capacity`), skip plumbing into `BenchReport.skipped`.
- **Book:** §15.9 "The expensive numbers" — reconstruction and capacity: benchmarks that hurt, and why they're opt-in/skip-by-default.
- **Commit:** `Complete the bench suites (Phase 15, 11/15)`

### Step 12/15 — Wtf engine (pure)

- **Tests first** (all against hand-built `WtfInputs` fixtures; one per pattern in §8): `wtf_reports_ok_for_healthy_cluster`, `wtf_flags_dead_node`, `wtf_flags_quorum_loss`, `wtf_flags_missing_leader`, `wtf_flags_crashlooping_app`, `wtf_links_crashloop_to_recent_deploy` (the correlated event is attached and the suggestion names the previous version), `wtf_flags_service_without_backends`, `wtf_deduplicates_no_backend_finding_when_crashloop_explains_it`, `wtf_warns_on_active_faults`, `wtf_warns_on_high_disk_and_criticals_at_95`, `wtf_warns_when_source_unavailable`, `wtf_unreachable_node_is_critical`, `wtf_app_scope_limits_findings`. Plus an insta snapshot of the human rendering of a mixed report.
- **Implement:** `src/relish/wtf.rs` — `WtfInputs`, `diagnose` composed of per-pattern `check_*` fns, renderer (`✗ CRITICAL`, `! WARNING`, `✓ OK` sections + summary + suggestions indented under each finding). Update `docs/design/cli-relish.md` with the `CorrelatedEvent` deviation note.
- **Book:** §15.10 "wtf: diagnosis, not dashboards" — the case for correlation over enumeration; show a crashloop→deploy correlation as narrative. Rust angle: keeping the engine pure made all thirteen patterns unit-testable with zero mocks.
- **Commit:** `Add the wtf correlation engine (Phase 15, 12/15)`

### Step 13/15 — `relish wtf` command

- **Tests first:** integration against the harness: `wtf_healthy_harness_exits_zero`, `wtf_reports_active_fault_as_warning_exit_two` (inject a fault via the API first), `wtf_json_output_round_trips`; unit: `watch_rejects_json_output`.
- **Implement:** `collect()` fan-out (`tokio::join!` over the endpoints, 10 s per-request timeouts, unreachable-node accounting via `/v1/cluster/nodes` addresses), `Command::Wtf` wiring, `--app`, `--watch` loop, exit-code mapping 0/1/2 through `ExitCode`.
- **Book:** §15.11 "Fanning out politely" — parallel collection with `tokio::join!`, partial results as a feature, and exit codes as a contract with CI.
- **Commit:** `Add relish wtf with fan-out collection (Phase 15, 13/15)`

### Step 14/15 — Trace endpoint

- **Tests first:** unit for the pure firewall evaluator (`src/firewall/evaluate.rs`): `evaluate_permits_listed_source`, `evaluate_denies_unlisted_source`, `evaluate_permits_all_when_no_allow_from_declared` (match current semantics — check how allow_from is defined in config; adjust name to reality), `evaluate_identifies_matching_rule_text`; endpoint integration (harness): `trace_passes_between_two_running_apps` (deploy two testapps, trace → 4 passes, latency present), `trace_fails_at_resolution_for_unknown_service`, `trace_fails_on_no_healthy_backends` (deploy then stop), `trace_probe_reports_refused_port` (trace to a service whose backend died).
- **Implement:** `evaluate.rs`; `src/onion/trace.rs::run_trace`; `GET /v1/trace` handler; external-host branch (system DNS + mandatory port).
- **Book:** §15.12 "Following one connection" (first half) — the four layers a packet crosses and how each is interrogated; be explicit about the two honesty notes (declared policy vs live nftables; node-origin probe).
- **Commit:** `Add /v1/trace connectivity endpoint (Phase 15, 14/15)`

### Step 15/15 — `relish trace` command + phase close-out

- **Tests first:** integration: `relish_trace_end_to_end_exit_codes` (pass → 0; unknown dest → 1); insta snapshots of pass and fail renderings from fixed `TraceResult`s.
- **Implement:** `src/relish/trace.rs` (locate source node via `/v1/status` + `/v1/cluster/nodes`, call, render §9), `Command::Trace` wiring, `--port` defaulting from the destination app's declared port.
- **Close-out (same step):** finish Chapter 15 — §15.12 second half, §15.13 "Lessons learned" (what was tricky: async fn pointers, teardown guarantees, honest skips), chapter intro/outro pass for flow; tick every Phase 15 box in `docs/progress.md`; update `docs/README.md` and top-level `README.md` (new commands with one-line descriptions, test counts from a fresh `cargo test` run, Phase 15 status, chapter table); update the roadmap milestone line if wording drifted; sweep this plan's §13 checklist.
- **Commit:** `Add relish trace and close out Phase 15 (Phase 15, 15/15)`

---

## 12. Acceptance runbook (manual/CI, real cluster)

Run after step 8 and again after step 15, on macOS with Lima installed:

```bash
relish dev create --nodes 3          # real 3-node Linux cluster (runc)
relish dev status
# point relish at a node (see dev status output / --cluster flag or RELIABURGER_* env)
relish test --output json | tee /tmp/test-report.json      # expect: 0 failed; skips only for unwired capabilities, each with a reason
relish test --filter scheduling                            # only scheduling group runs
relish test --chaos --yes                                  # 5 scenarios, cluster healthy afterwards, `relish fault list` empty
relish bench --quick --output json > /tmp/base.json
relish bench --quick --compare /tmp/base.json              # ~0 regressions against itself
relish wtf                                                 # exit 0 on the healthy cluster
relish fault kill <some-test-app> --count 1 && relish wtf  # crashloop/backends finding appears; exit non-zero
relish trace <appA> --to <appB>                            # 4 steps, PASS
relish dev destroy
```

Record the outcome (date, cluster size, skips observed) at the bottom of this file.

---

## 13. Step checklist

- [ ] 1/15 capabilities endpoint
- [ ] 2/15 embedded testapp + modes
- [ ] 3/15 testkit core types
- [ ] 4/15 runner
- [ ] 5/15 `relish test` CLI
- [ ] 6/15 catalogue A
- [ ] 7/15 catalogue B
- [ ] 8/15 chaos suite
- [ ] 9/15 bench core
- [ ] 10/15 bench suites A
- [ ] 11/15 bench suites B
- [ ] 12/15 wtf engine
- [ ] 13/15 `relish wtf` CLI
- [ ] 14/15 trace endpoint
- [ ] 15/15 `relish trace` + close-out
- [ ] acceptance runbook on a 3-node dev cluster

---

## 14. Risks and gotchas (read before starting)

1. **Don't assert broken behaviour.** Several catalogue cases guard known review bugs (H1, H2, H8). If the fix hasn't landed when you get there, the case will genuinely fail on a real cluster — that's the point. In repo CI (single-node harness) those specific assertions may be unreachable; keep them capability-/topology-gated rather than watering them down.
2. **`start_paused` + `tokio::spawn` don't mix** (standing feedback). Runner timing tests use short real timeouts or manual channel driving.
3. **Teardown is the runner's job.** A panicking or timed-out test must still get its namespace cleaned. Since case bodies return `Result` (no panics in our code), timeout + always-await-teardown covers it; if a case panics anyway, `JoinSet` reports it — map to `Failed { message: "panicked: …" }` and still tear down.
4. **Never block the runtime.** SHA-256 over the OCI fixture and any file I/O in the agent path go through `spawn_blocking`; CLI-side small file reads are fine.
5. **Timeout everything.** Every reqwest call, every poll loop, every TCP probe. A diagnosis tool that hangs is worse than no tool.
6. **JSON stability.** `schema_version` on both report types; insta snapshots pin the shape. Changing a field name after this ships breaks users' CI — treat the JSON as API.
7. **Namespace hygiene on real clusters.** `rbtest-*`/`rbbench-*` prefixes are the safety net; teardown stops by namespace, never by "everything the cluster runs". Grep yourself honest: no code path may call stop/clear endpoints without a namespace or fault-id filter — except chaos fault-clearing, which clears only faults the suite injected (track injected fault ids in `ChaosGuard`).
8. **Cross-node client calls** (wtf fan-out, trace node targeting) need node API addresses from `/v1/cluster/nodes` and the same bearer token; verify the token is honoured cluster-wide once review C5a wiring lands — until then, single-node collection with `source-unavailable` warnings is acceptable and must not crash.
9. **eBPF fields** are capability data, not compile decisions: the same relish binary must behave sensibly against Linux (eBPF on) and macOS (off) clusters.
10. **Keep the book chapter honest.** Where we deviated from the design docs (`--capacity` flag, `CorrelatedEvent`, node-origin probe, `--yes`), the chapter says so and why — that's the "what we decided not to do" the book promises.

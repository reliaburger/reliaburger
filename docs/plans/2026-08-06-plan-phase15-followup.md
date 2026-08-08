# Phase 15 follow-up plan — review of PR #151 (`codex/phase15-correctness-2026-07-28`)

Reviewed 6 August 2026 against `origin/main` (f7eb560). The branch is 27 commits,
~28k insertions across 147 files. This plan turns the review into PR-sized tasks,
grouped in phases, with enough detail for direct implementation. Work through the
phases in order; tasks inside a phase are independent unless noted.

## Where we are

The branch delivers the Phase 15 implementation: conclusive test outcomes, durable
leases, capability/readiness evidence, chaos primitives and five scenarios, real
data-plane benchmarks, `relish wtf` and `relish trace`, plus prerequisite fixes to
Pickle auth, trust domains, rootless networking, deploy state and ingress TLS.
The safety architecture held up under review: operation grants, target-side
re-authorisation, principal-bound audit and refusal-not-green-skip all have real
code paths and tests. The remaining phase gate is the three-real-node acceptance
runbook (`docs/plans/2026-07-06-plan-chaos.md` §9).

The review found four CI failures (all root-caused below), three
branch-introduced regressions that break real clusters (builds, self-upgrade and
replication 401 on any cluster with a routable advertise address), one critical
race in the new cluster lease cleanup, two authorisation gaps in fault clearing,
and a set of honesty bugs that make `relish wtf` exit 0 unreachable and let a
broken data plane produce green benchmark numbers.

### CI status (run 31051812645, commit 708ea3c)

The branch tip (88b194d) has **no CI run at all**: the last two commits were
pushed with credentials that do not trigger workflows. Re-trigger by pushing an
empty commit with normal user credentials, or close and reopen PR #151.

| Job | Cause |
|---|---|
| dependency advisory audit | rkyv RUSTSEC-2026-0235. **Already fixed at tip** (`.cargo/audit.toml` ignore, expires 18 Aug 2026); the failing run predates the fix. |
| portable Linux | clippy `redundant_pattern_matching` at `src/smoker/node_pressure.rs:204` (new stable). Task A1. |
| privileged Linux | node-pressure helper start timeout: helper does its ~300 MB ballast memset *inside* the 5%-CPU-throttled cgroup, in a debug build, against a 4 s readiness timeout. Task A1. |
| cluster upgrade | all 3 `self_upgrade_cluster` tests: blob push 401. The branch's fail-closed clustered registry auth is correct; the harness pushes anonymously. Task A2. |

### Time-boxed deadline

**Before 18 August 2026** the advisory exceptions in `.cargo/audit.toml` expire
(`make audit` fails closed after that date). Task G4 must land before then or
CI on main starts failing.

---

## Phase A — green CI on PR #151 (land on the branch itself)

### A1. Fix node-pressure helper startup and the clippy lint
- [ ] `src/smoker/node_pressure.rs:204`: replace `matches!(&started, Err(_))` with `started.is_err()`.
- [ ] Move the CPU throttle out of the helper's startup path. In
  `prepare_fault_cgroup` (~line 337) stop writing `cpu.max`; keep
  `memory.max`/`memory.swap.max`/`memory.oom.group` pre-spawn (memory is the
  safety bound and must precede allocation). In `NodePressureController::apply`,
  write `cpu.max` immediately **after** the "ready" line is read from the helper.
  Keep `HELPER_START_TIMEOUT` at 4 s. Update the module doc comment: the quota is
  applied post-readiness; the unthrottled window is one startup, bounded.
- [ ] In `run_helper`, after `ballast.resize(memory_len, 0)`, write `0xa5` at
  4096-byte stride instead of memsetting every byte (equally resident, ~4000×
  less work in a debug build).
- [ ] Update the matching walkthrough in `docs/book/15-ready-for-production.md`.
- Verify: `make ci`, then `RELIABURGER_NODE_PRESSURE_TESTS=1` via `make test-linux`
  in the Lima VM (recipe in memory / `docs/plans/2026-07-18-plan-codebase-review-follow-up.md`).

### A2. Authenticate the self-upgrade cluster harness
The fail-closed clustered write policy is deliberate; fix the tests, not the server.
- [ ] In `tests/self_upgrade_cluster.rs` `ClusterHarness::start` (~line 104):
  generate a 32-byte key, write `hex::encode(key)` to
  `root.path().join("master.key")` with permissions `0o600` (required by
  `sesame::bootstrap::check_permissions`).
- [ ] Add `[security]\nmaster_key_path = "…"` to every node's config template
  (safe: `require_mtls` stays false, loopback advertise passes
  `enforce_cluster_transport_security`; the shared file also shares the gossip
  HMAC key).
- [ ] Store `reliaburger::sesame::token::derive_service_token(&key)` on the
  harness; send it as `Authorization: Bearer …` on the blob push in
  `start_upgrade` (line ~380).
- Verify: `RELIABURGER_UPGRADE_TESTS=1 make test-upgrade-cluster`.

### A3. Re-trigger CI and confirm the audit job
- [ ] Push an empty commit with user credentials (or close/reopen PR #151) so the
  tip actually runs CI; confirm the audit job passes with the rkyv ignore.

---

## Phase B — branch-introduced regressions that break real clusters
These are not caught by CI (which only exercises loopback clusters). They should
merge right after #151 as stacked PRs — or fold into #151 if preferred.

### B1. Restore loopback registry service and authenticate the build pipeline
`plan_registry_bind` now binds ONLY the advertised IP on clustered nodes, but the
build pipeline is hardwired to `localhost:{port}`; and clustered writes now
always need credentials that buildah never presents. Every `rbrg build` on a
routable cluster fails (connection refused, then 401).
- [ ] `src/pickle/capability.rs` (`plan_registry_bind`, ~47–86): when the
  configured bind is the loopback default and the cluster advertises a routable
  IP, bind the **unspecified address** of the advertise family (`0.0.0.0`/`::`)
  instead of the advertise IP. `peer_reachable` stays true; `require_read_auth`
  stays true because non-loopback.
- [ ] `src/pickle/build.rs`: pass credentials on push — either `--creds` with the
  service token in `buildah_push_args` (~232–242), or route context
  upload/download (`_buildcontext`, lines ~173/254/269) and push through the
  bearer-carrying `registry_client`.
- [ ] Add an integration test running a clustered build against a non-loopback
  bind (gated, `tests/`).

### B2. Present the service token on the self-upgrade binary fetch
`UpgradeManager::fetch_binary` (src/upgrade/manager.rs:559–598) downloads via
`ClusterHttp`, which attaches no bearer; on routable clusters
`require_read_auth = true`, so **every production self-upgrade 401s**.
- [ ] Give `ClusterHttp` an optional bearer (constructed in `src/bin/bun.rs` from
  `service_token`, mirroring `build_cluster_http_client_with_bearer` at
  ~1934–1944), or add a dedicated bearer client on `UpgradeManager`.
- [ ] Unit test: the `fetch_binary` request carries the bearer.

### B3. Keyless-cluster warning and honest startup banner
- [ ] `src/bin/bun.rs`: when `cli.cluster && service_token.is_none()`, print a
  prominent warning (or refuse — decide; refusing matches the fail-closed
  philosophy) that registry writes and cross-node replication require
  `[security] master_key_path`. Today replication silently 401s forever on
  keyless clusters.
- [ ] Fix the "plaintext, unauthenticated" banner (~1984–1989): clustered
  listeners now authenticate writes and routable reads.

---

## Phase C — criticals in the new Phase 15 machinery

### C1. Fix the cluster lease-cleanup snapshot race
`cleanup_cluster_lease` (src/testkit/lease.rs:610–663) snapshots
`desired_state()` **before** the `TestLeaseBeginCleanup` Raft write; a resource
attached in that window leaks forever, and `TestLeaseFinishCleanup`
(src/council/state_machine.rs:747–762) then destroys the ownership record.
- [ ] After `TestLeaseBeginCleanup` commits, re-read `council.desired_state()`
  and iterate *that* lease's resources.
- [ ] Defence in depth: `TestLeaseFinishCleanup` refuses removal while any owned
  `App`/`Namespace` still exists in `state.apps`/`state.namespaces`.
- [ ] `config_to_leased_writes` (src/council/apply.rs:69–97) silently drops
  non-app/namespace kinds (jobs, volumes); make it **reject** such configs.
- [ ] State-machine test: attach an app between snapshot and BeginCleanup; assert
  resumed cleanup deletes it and FinishCleanup refuses early.

### C2. Surface Raft refusals in `cluster_apply`
`src/bun/api.rs:2344`: the write loop treats `Ok(CouncilResponse::Refused)` as
committed; a refused leased write streams "committed to the cluster" and the
case later dies as `Unknown(TimedOut)`.
- [ ] Match `Refused { reason }` → emit `ApplyEvent::Error` and stop (mirror
  `write_cluster_lease_request`).
- [ ] Router test: leased apply against a lease in `Cleaning` streams an error
  event, not "committed".

### C3. Close the clear-all fault authorisation gaps
Two paths let a Deployer reverse Admin-class node faults:
`DELETE /v1/fault?service=` with an empty string matches every node fault
(src/bun/api.rs:4376–4384 + src/smoker/registry.rs:100–113), and blanket
`DELETE /v1/fault` clears `NodePressure` because it is not `is_node_operation()`
(src/bun/agent.rs:2952–2961).
- [ ] `fault_clear_all_handler`: reject empty `service` with 400.
- [ ] `clear_by_service`: skip faults where `is_node_operation()` or
  `NodePressure`.
- [ ] `clear_workload_faults`: also retain `NodePressure`.
- [ ] api.rs tests: Deployer with the workload grant issues both endpoints while
  a NodeKill and a NodePressure fault are active → both survive.
- [ ] Note in chapter 15: pressure reversal is Admin-only on every path.

### C4. Un-wedge NodePressure expiry cleanup
`expire_faults` (src/bun/agent.rs:3958–3979) drains the registry rule first; if
`remove_fault_cgroup` fails, the handle is re-inserted into `controller.active`
but the rule is gone — every future pressure fault refuses ("already active")
until Bun restarts, and the cgroup leaks.
- [ ] Give `NodePressureController` a `retry_pending_cleanup()` invoked each
  health tick that re-attempts `remove_fault_cgroup` for orphaned handles and
  only then frees the `active` slot.
- [ ] Make `remove_fault_cgroup` async-friendly: replace the `std::thread::sleep`
  retry loop (lines 381–401) with `tokio::time::sleep` or `spawn_blocking`.
- [ ] Unit test: clear with an injected populated-cgroup failure → second attempt
  succeeds and `apply` works again.

### C5. Harden node-fault routing safety
- [ ] TOCTOU in `check_node_fault_cluster_safety` (src/bun/api.rs:3968–4028):
  evidence is SWIM-only, so two NodeKills against different voters inside the
  suspicion window both pass and can break quorum. Keep a short-TTL local ledger
  of node faults this API injected or forwarded (target, expiry) and union it
  with SWIM-unavailable voters; document the residual cross-entry-node race.
  Test: NodeKill voter A via node X, immediately request NodeKill voter B via X
  → refused before SWIM notices A.
- [ ] When `state.node_name` is `None` and a `target_node` is named
  (src/bun/api.rs:3812–3818), return 400 instead of applying locally.

---

## Phase D — mixed-version / rolling-upgrade safety

### D1. Readiness must not fence pre-Phase-15 nodes
`src/cluster/orchestrate.rs:685–692` requires present-and-true readiness
evidence; old binaries never send it, so during a rolling upgrade a new leader
marks every not-yet-upgraded node not-ready and the scheduler stops placing on
them.
- [ ] Treat a missing readiness report as ready when the node's capability
  report shows a pre-Phase-15 version (or start with
  `is_none_or(|r| r.evidence.ready)` and flip to fail-closed in a later
  release).
- [ ] Mixed-version test mirroring `cache_includes_only_alive_nodes_with_reported_capacity`.

### D2. Guard the new Raft variants during rolling upgrades
Seven new `RaftRequest::TestLease*` variants (src/council/types.rs:224–259) are
undecodable by old binaries: one leased write while a pre-Phase-15 voter is
alive wedges that follower's replication. Related: `rbtest-*` namespace refusal
(state_machine.rs:181–193) changes apply semantics for replayed legacy logs.
- [ ] Leader-side check: refuse `TestLeaseCreate` proposals while the upgrade
  coordinator has an active operation.
- [ ] Book/design note (agent-bun or a new upgrade-compat section): new
  `RaftRequest` variants must not be proposed until a roll completes; snapshot
  fields unknown to old nodes are dropped silently.

---

## Phase E — diagnostics and benchmark honesty

### E1. Validate benchmark workload probes
`network_throughput` (src/testkit/bench/suites.rs:408–414) and
`discovery_latency` (~354–359) never check probe exit status; busybox `nslookup`
prints "Address" even on NXDOMAIN, and `grill.exec` discards exit codes — a
broken data plane produces excellent numbers.
- [ ] Reuse the trace marker-script pattern (`DNS_TRACE_SCRIPT` /
  `TCP_TRACE_SCRIPT` in src/bun/agent.rs + `parse_probe_output` in
  src/onion/trace.rs): wrap probes in `sh -c '…; printf MARKER=%s $?'`, fail the
  sample on non-zero, and require a DNS answer line naming the app. Extract the
  shared helper (e.g. `src/testkit/probe.rs`).
- [ ] Unit tests: NXDOMAIN-shaped busybox output; `wget: not found`.
- [ ] Consider renaming `discovery_latency_p99` → `discovery_exec_roundtrip_p99`
  (the exec spawn dominates DNS latency).

### E2. Make `relish wtf` exit 0 reachable
`collect_restarts`/`collect_deploys` (src/relish/wtf/collect.rs:483–487,
532–535) and the api.rs certificate source always mark evidence `Degraded`;
`diagnose()` maps Degraded → unknown → exit 2. A healthy cluster can never
exit 0, contradicting the documented contract.
- [ ] Add a `limitation: Option<String>` on Available evidence (or an
  `AvailableWithCaveat` variant) for inherent limitations with no collection
  errors; render as caveats, not unknowns. Genuine errors keep mapping to
  unknown/exit 2. Bump `WTF_SCHEMA_VERSION` if the wire shape changes.
- [ ] Update the book (docs/book/15-ready-for-production.md ~2010–2016).
- [ ] End-to-end test: healthy snapshot exits 0.

### E3. Missing metrics must fail benchmark comparison
`missing_in_current` (src/testkit/bench/compare.rs:141–142) affects neither the
exit code nor human output — a regression can hide by disappearing.
- [ ] Render `missing_in_current`/`missing_in_baseline` in `render_human`; make
  non-empty `missing_in_current` count toward the non-zero exit unless the run
  is informational.
- [ ] Tests: vanished metric ⇒ `Problems` exit, named in output.

### E4. Reconcile test profiles with `failed_any`, and type the skips
`failed_any` fails on any `Unknown` in **every** profile, but four catalogue
cases return `unknown(...)` unconditionally (secrets_config ×2,
image_registry `deploy_from_cluster_registry`, workload_identity
`workload_receives_spiffe_certificate`) — a full-catalogue run can never exit 0.
- [ ] Pass the profile into the verdict: development maps non-required
  `Unknown`/`Skipped` to `CommandOutcome::Warnings` (exit 2, currently never
  returned by test_cmd.rs:118–122); full profiles stay strict. Update the module
  doc and tests.
- [ ] Convert the four unconditional `unknown()` cases into typed skips behind
  new capabilities (e.g. `AgePubkeyApi`, `RegistryFixture`).

### E5. wtf diagnosis accuracy and watch resilience
- [ ] Expired certificates (`rotation_state == "expired"` or `not_after` past)
  become **critical**, not a "expires in 0 days" warning (diagnose.rs:629–659).
- [ ] Council `Degraded` fallback with `member_count == 0`
  (collect.rs:415–447) records unknown instead of firing a critical quorum-loss.
- [ ] Key `coalesce_disk_filesystems` (collect.rs:720–738) on a real filesystem
  identifier, not `(node, used, total)`.
- [ ] Replace substring matches with exact/token-bounded matches: instance ids in
  `src/bun/diagnostics.rs:416–425` and the expected-VIP check in
  `src/bun/agent.rs:8146` (id `api-1` currently matches `api-10`).
- [ ] `--watch` (src/relish/wtf_cmd.rs:51–68): render a transient collection
  failure and retry next tick instead of terminating; Ctrl-C returns the last
  report's outcome, not unconditional 0. Test with a stub that fails once.

### E6. Bound testkit lock waits and the ChaosGuard window
- [ ] `begin_cleanup_operation` (src/testkit/lease.rs:719–739): `try_lock_owned`
  (or short timeout) + new `LeaseError::Busy`; the reaper logs and skips so one
  wedged deploy defers only its own lease. Test: guard held on lease A, lease B
  expires, B reaped while A deferred.
- [ ] `ChaosGuard::inject_fault` (src/testkit/chaos/mod.rs:135–146): reserve the
  `OwnedFault` entry before the inject HTTP call so a runner abort in the
  track window can't orphan a fault; id-less entries clean up as
  `CleanupOutcome::Unknown` naming the fault type.

### E7. Readiness means ready, not spawned
`spawn_owned`/`spawn_reconstructible` (src/bun/readiness.rs:237–240, 269–271)
mark subsystems `Ready` at spawn, before resources are acquired; crash-looping
reconstructibles flap Ready↔Degraded.
- [ ] Change the factory contract to a readiness callback/handle the task calls
  once its listener is bound; update call sites in `src/bin/bun.rs` and
  `src/cluster/runtime.rs`.
- [ ] Test: a task that never signals stays `Starting`.

---

## Phase F — hygiene batches (one PR each)

### F1. Testkit hygiene
- [ ] Replace `Result<(), String>` + `UNKNOWN_MARKER` (src/testkit/registry.rs)
  with `Result<(), CaseError>` where `enum CaseError { Fail(String), Unknown(String) }`;
  mechanical sweep of `src/testkit/cases/*` and `chaos/scenarios.rs`.
- [ ] Collision-proof `generate_run_id` (src/relish/test_cmd.rs:163–172): mix in
  4 random bytes; two runs in the same second currently collide on namespaces.
- [ ] fsync the parent directory in `persist_leases` (src/testkit/lease.rs:500–529).
- [ ] Move `attach_app` under `#[cfg(test)]`; delete dead `Deadline::child` and
  `renew_test_lease`/`/renew` (or wire renewal for cases where
  `timeout + 30s > max_lease_seconds`); replace the three runner `.expect()`s
  with a validated `RunConfig::new` returning `Result`.

### F2. Smoker hygiene
- [ ] Restructure `chaos_preflight`'s match to drop the `unreachable!()`
  (src/testkit/chaos/mod.rs:96).
- [ ] Run `cleanup_stale_cgroups` before the `limits.enabled()` early-return in
  `prepare_controller` so disabling pressure still sweeps crash leftovers.
- [ ] Fix the PDEATHSIG comment (fires on spawning-*thread* exit, fail-safe
  direction only); read helper stderr after `child.wait()` instead of a 100 ms
  timeout.

### F3. Misc small fixes
- [ ] slirp socket wait: `tokio::fs::try_exists` with a 5 s deadline
  (src/grill/rootless.rs:270–283); shut down a displaced `slirp_handles` entry
  on insert (src/grill/runc.rs:806–826).
- [ ] Single `live_egress_report_state()` call per health tick shared between
  readiness and reporting (src/bun/agent.rs:2179–2210).
- [ ] Dual-stack allowance (or a better error) for `plan_registry_bind` family
  mismatch (src/pickle/capability.rs:66–77).
- [ ] Refactor `router_with_upgrade`'s ~25 positional args into a config struct
  shared with tests (tests/support/cluster_harness.rs:70–99).
- [ ] `// provably infallible` comments (or const restructure) for the three
  `.expect()`s in src/testkit/bench/runner.rs:145,160,163.
- [ ] Typed alerts: replace `BunClient::alerts() -> Vec<serde_json::Value>` with
  a shared `AlertStatus` struct; replace the `contains("no eligible nodes")`
  match in the capacity bench with a machine-readable scheduler error code.

### F4. Deploy-conflict ergonomics
`TargetBusy` (src/bun/agent.rs:2493–2530) now refuses a fix-deploy while a bad
deploy sits in health-check retries, and `DeployTarget` equality ignores `kind`.
- [ ] Allow supersede (cancel + replace, recording `Cancelled`) or add
  `?force=true`; include the active operation's age/phase in the error; make
  target equality include `kind`.

### F5. Decide chaos gating for non-Linux clusters
Suite-wide preflight demands `NodePressure`, so an Apple-silicon cluster can
never run *any* chaos scenario.
- [ ] Either move the `NodePressure` requirement into C4's per-case `requires`
  (capability-unmet chaos cases report refusal-grade outcomes, not green skips),
  or document explicitly in `docs/design/chaos-smoker.md` + the book that chaos
  is Linux-cluster-only.

---

## Phase G — documentation

### G1. Consistency sweep
- [ ] `docs/progress.md:1303–1305`: the header note still calls the diagnostic
  commands "the largest block of genuinely unbuilt feature work" above a
  checklist where they are all `[x]`; rewrite to name the true remaining set
  (acceptance runbook, Pickle push streaming, cert rotation, cold-cache bench
  ownership).
- [ ] `docs/roadmap.md:419`: Phase 8 milestone still says `relish fault delay …
  works`; delay is now a reserved contract that refuses. Use a drop/partition
  example.
- [ ] `docs/README.md:426` ("rootless runc" dev cluster) vs `docs/progress.md:80`
  (sudo/rootful) — reconcile, and state whether a dev cluster satisfies the
  chaos catalogue's rootful cgroup-v2 prerequisite.
- [ ] Checkbox sweep: `docs/progress.md:1458` parent vs done children; chapter 15
  status marker in top-level README ("in progress" vs progress.md "[x] Complete
  chapter 15"); "six starter chapters" → seven (`docs/progress.md:1839`); stale
  O3 doctest note in the follow-up plan (line 646).

### G2. Chapter 15 close
- [ ] Add the missing "Lessons learned" section (green tests that lie, skip vs
  unknown, evidence expiry, exact fault ownership, the sandbox-vs-real
  acceptance boundary); move the acceptance-limitation footnote there and
  reference the §9 runbook. Follow the style guide in CLAUDE.md.

### G3. Manual: diagnostics chapter
- [ ] Nothing in `docs/manual/` mentions `relish wtf`, `relish trace` or
  `relish bench`. Extend 04/05 or add a chapter; update the rust-embed chapter
  list.

### G4. Advisory-exception review — **deadline 18 August 2026**
- [ ] Re-review `thrift`, `lru` RUSTSEC-2026-0002, `anyhow` RUSTSEC-2026-0190,
  `rkyv` RUSTSEC-2026-0235 and the maintenance notices (bincode,
  rustls-pemfile, paste, proc-macro-error); extend or fix each; update
  `.cargo/audit.toml`, the `make audit` expiry date and the table in
  `docs/plans/2026-07-18-plan-codebase-review-follow-up.md:71–75`.

---

## Phase H — acceptance

### H1. Gated cluster end-to-end lease test
- [ ] New test in the `make test-cluster` suite: 3 nodes, create lease via
  `/v1/test/leases`, leased apply, kill the leader, assert the new leader's
  reaper transitions Cleaning → gone and the app is deleted cluster-wide.
  (This is exactly the test that would have caught C1.)

### H2. Execute the three-node acceptance runbook (human in the loop)
- [ ] Amend `docs/plans/2026-07-06-plan-chaos.md` §9 first: state which
  `--profile` each run uses (`full-runc` on Linux nodes, `full-apple` on macOS)
  and that C4 needs rootful cgroup v2.
- [ ] Run §9 on three independent nodes/VMs; record date, cluster size and
  observed skips in the plan; then check `docs/progress.md:1374` ("All Phase 15
  tests green"), M8 (line 1448), follow-up plan line 503, and finally rerun the
  review matrix (progress.md line 1414).

## Carried-forward engineering residuals (tracked, not in this plan's scope)

- Pickle push-side request-body streaming (`docs/progress.md:1731`).
- Certificate rotation/renewal before the wtf TLS-expiry diagnostic counts as an
  accepted capability (follow-up plan line 531).
- Leased image deletion/eviction for the cold-cache image-distribution benchmark
  (`docs/progress.md:1349–1351`).
- Lease-ownership models for jobs, faults, tokens, images, mounts and node state
  (follow-up plan lines 615–616).

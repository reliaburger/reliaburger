# Unify behavioral contracts across entry points and add lifecycle regression coverage

Suggested priority: **P2 — cross-cutting hardening, implemented last after the accepted individual fixes.** Baseline: v0.1.4 at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

This is a follow-up implementation issue derived from the audit, rather than a 28th independently reproduced runtime defect. It consolidates the remaining architectural and test-harness work after the concrete regressions have been fixed.

### Problem

The main weakness is **consistency across paths**. The same policy or behavior is implemented separately in manual apply, clustered apply, follower forwarding, GitOps, batch dispatch, registry publication and node launch. Individual components have substantial tests, but the complete paths do not consistently preserve authorization, resource identity, configuration meaning or completion guarantees.

Examples from the audit:

- Ordinary apply enforces token scope and Deploy/HostExec grants; batch dispatch bypasses them (draft 03).
- Standalone deploy waits for `run_before`; clustered apply publishes apps first, and GitOps drops the job (07, 22).
- CLI directory compilation applies image defaults and path namespaces; GitOps uses a separate loader (19–21).
- The direct scoped registry read check works, while publication can create the reference that defeats it (04).
- API authentication bounds/offloads Argon2; registry authentication uses the synchronous verifier (12).

The other recurring weaknesses are lifecycle coverage and the meaning of success. Tests often establish an immediate result without verifying expiry, reuse, concurrent writers, failed persistence or reopening. An accepted request, completed run, unchanged preview or successful flush must correspond to the promised observable outcome. A configured line-coverage floor cannot establish those guarantees on its own.

### Scheduling and scope

Execute this as the **final hardening step** after the accepted audit fixes, using their expected-behavior regressions as the baseline. Do not defer urgent authorization/data-loss fixes until this umbrella work lands. The earlier fixes should already add their own failing-first tests; this issue connects those tests, eliminates remaining rule drift and makes future gaps visible in CI.

Before implementation, read the approved issue list and current source. References below identify the audited implementation; upstream fixes may have moved or replaced it. Replace draft numbers with actual GitHub issue numbers when the maintainer publishes them.

Implement the approved audit work through **one merge train PR**, under one dated plan in `docs/plans/`, following `CLAUDE.md`. All smaller implementation PRs merge into its integration branch. The train receives the consolidated maintainer review and final qualification, then merges into the maintainer-selected destination once.

### Merge train workflow

1. Create one integration branch from the maintainer-selected base, using a name such as `codex/audit-hardening-train`, and one train PR targeting the selected release branch or `main`. Keep its description current with the final scope, child-PR checklist, dependency order and qualification evidence. Its initial description must state that qualification is incomplete.
2. Target every smaller implementation PR at that integration branch. Merge the accepted individual fixes (drafts 01–27) in dependency order, then implement draft 28 as the final phase on the same train. Child PRs are implementation units; the maintainer reviews the combined result through the train rather than separately approving every child.
3. Run failing-first regressions and focused local checks for each child, plus any required checks on its target. Do not wait for optional full-cluster/release qualification between child merges. Rebase or resolve integration conflicts against the current train before merging each child; preserve dependency order and the regressions from earlier children.
4. Use a `codex/` integration target rather than a `release-*` child target. The current CI selector enables heavy suites for PRs targeting `main` or `release-*`; other stacked targets use the lighter selection unless labelled `full-ci`. Reuse this existing policy. Do not add `full-ci` to every child or weaken required checks. See `scripts/ci/select-jobs.sh` and its fixtures.
5. The outer train PR can still start full CI on intermediate updates. Existing PR concurrency cancels superseded runs. Integrate ready children without waiting for each obsolete outer run; wait for full qualification of the **final integrated commit** before requesting consolidated approval and merging the train. If a required-check or CI-selection policy prevents this workflow, resolve that explicit policy rather than silently bypassing it.
6. Finish all child integration, run the complete applicable CI/runtime/qualification matrix, attach its evidence to the train and request one consolidated maintainer review. Any changes after that qualification invalidate affected evidence; rerun the applicable checks on the new final commit. Merge the train only after its required checks and maintainer approval.

Preserve intentional semantics: manual apply is additive, GitOps also computes removals, imperative jobs have run identities, and webhook admission authenticates a signed delivery rather than a user bearer. Equivalent guarantees do not require identical transport or execution behavior.

### Required contract matrix

Create a machine-readable inventory, suggested location `tests/contracts/manifest.json`, with stable contract IDs, applicable entry points, exact Rust test names, owning CI/manual gates, and any explicitly unsupported combinations. Generate a human-readable matrix for `docs/testing.md` from that inventory. Each supported combination must be exercised; an unsupported combination must have an explicit-refusal test rather than silent success.

| Contract family | Entry points / dimensions to cover | Observable invariant | Audit drafts |
| --- | --- | --- | --- |
| Authorization and admission | Direct leader, follower forwarding, local/remote batch, registry reads/publication; scoped Deployer, denied grants, allowed and forbidden namespaces | Original principal is preserved; refusals create no desired resource, run, catalogue reference or storage side effect; token hashing uses bounded async admission | 03, 04, 12 |
| Effective configuration and preview | CLI file/directory, manual API, GitOps commit, online/offline preview; defaults, existing namespaces, same-name resources | Equivalent supported inputs produce equivalent namespace-qualified effective specs; invalid/incomplete input cannot claim complete success; material changes are visible | 19–25 |
| Deployment ordering and triggers | Standalone, cluster leader/follower, GitOps; blocked/failed migration, leader handover | Dependent app revision becomes schedulable only after prerequisite success; an accepted webhook reaches a coordinator or is retained for retry | 07, 22, 26 |
| Run identity and completion | Batch dispatch/callback/pull watcher, repeated names, concurrent batches, restart | Only the acknowledged execution generation can settle its record; every admitted job is represented and reaches the correct terminal state | 08, 15 |
| Persistence and artifact identity | Log flush/replay/reopen, metrics multiwriter storage, managed volume creation/remount | Acknowledged durable content is retained; no checkpoint passes missing data; distinct owners/paths cannot overwrite one another’s artifacts | 01, 05, 06 |
| Capacity and storage admission | Cluster apps/batches, stale/missing reports, simultaneous submissions/uploads, consumer publication | Reservations survive the admission/reporting gap; physical usage is bounded; unsupported service size is refused explicitly and does not block unrelated updates | 11, 13, 16 |
| Identity/time and network lifecycle | Signer expiry, routing replacement, active stream lifetime, redirects, DNS UDP-to-TCP retry | Earlier success remains valid for the documented lifetime; current endpoint health persists appropriately; protocol behavior reaches the client intact | 02, 09, 10, 17, 18 |
| Numeric boundary behavior | Cron parser in debug/release, autoscale targets | Invalid inputs are refused; accepted values have identical defined semantics across profiles and finite valid control parameters | 14, 27 |

Keep the matrix finite. For each contract, name the specific supported combinations and why they differ; do not generate every possible cross-product of unrelated runtime, role and transport settings.

### Phase 1 — shared fixtures and assertions

1. Extend existing `tests/support/cluster.rs`, `cluster_harness.rs`, `bun_process.rs` and `task_harness.rs`. Reuse process cleanup, bounded readiness and real API/Raft fixtures. Put portable cases under `tests/suite/` and register them in `tests/suite/main.rs`; keep gated/process-isolated cases in the existing heavy binaries.
2. Add reusable authenticated fixtures. Mint actual scoped credentials and persist realistic permission/namespace state. Exercise at least one refusal and one allowed control for each principal-facing mutating path. Default tokenless/system-token fixtures cannot substitute for a scoped-principal case.
3. Represent entry-point adapters in test support: CLI subprocess, direct HTTP apply, follower HTTP apply, GitOps repository sync and batch submission. Adapters drive the public path and collect observable evidence; they must not reproduce the production policy logic themselves.
4. Add assertions over canonical effective specs, qualified resource identities, execution generations, persisted rows/checkpoints and admitted footprints. Compare exact expected fixtures or independently stated invariants. Reusing the same production helper for both the tested transformation and the expected answer would conceal a shared bug.
5. Retain a small real-binary/real-council check for each important contract family. Fake agent commands are useful for deterministic race scheduling, but at least one real run must prove that admission, forwarding and runtime wiring invoke the tested code.

### Phase 2 — consolidate rules where the matrix finds drift

Inventory remaining duplicated decisions after the individual fixes. Extract only the common domain rule and keep transport-specific orchestration local.

- **Workload admission:** one clearly named admission function for principal scope, namespace grants, host execution and applicable lease/resource restrictions. Call it before writes or dispatch in every supported mutating workload path. Forwarding must retain the original authority or a verified delegation carrying its limits. Internal service credentials must not accidentally replace the user’s scope.
- **Configuration resolution:** one typed directory resolver operating on a deterministic set of path/content inputs, usable from filesystem compilation and a verified Git commit. Separate intrinsic validation from checks requiring committed namespaces. Use namespace-qualified identity and effective specs consistently in apply, diff and preview. Do not interpret every TOML file as a workload declaration when some are defaults.
- **Execution/storage identity:** use existing typed identity/generation structures where available. Carry them across tracker records, status, callbacks and durable artifact naming; avoid a parallel string convention in each subsystem. If a persisted/wire shape must change, apply the repository’s compatibility-generation rules and update restart fixtures.
- **Persistence/admission ownership:** make pending bytes, batches and reservations have a clear owner until durable success or confirmed release. Shared helpers may support this, but log checkpoints, registry quota and volume provisioning have different commit boundaries and need explicit adapters.

Preserve the regression tests while extracting helpers. Introduce a typed admission/resolution result only where it makes bypassing an important rule harder; avoid a generic policy framework or broad code reorganization unrelated to a failing contract. Each extraction child PR must demonstrate the behavior of all callers it changes before joining the train; its results contribute to the consolidated review.

### Phase 3 — deterministic lifecycle and boundary tests

Add narrow clock and storage fault seams where the existing interfaces cannot drive the required transition. Production defaults must retain real time and real storage. Do not add operator-facing fault injection switches.

- Distinguish wall-clock certificate/token validity from Tokio’s monotonic deadlines. Advancing a paused Tokio clock does not advance `SystemTime::now()`. Pass an explicit clock/time into lifecycle decisions or use a narrowly injectable clock; test expiry just before, at and after the boundary, renewal and reopen/restart. Do not require a one-hour sleep to detect signer expiry.
- Fail log persistence at directory creation, Parquet write and checkpoint publication; then ingest more rows, retry and reopen. Assert all promised rows appear exactly once and checkpoints never advance beyond the committed data boundary. Exercise concurrent ingestion and cancellation where the contract permits them.
- Open two metrics writers before either flushes. Test interleaved/concurrent writes and restart using the same prefix. Assert immutable chunk identity, retention of both writers’ samples and correct query ownership.
- Delay new batch acknowledgement while publishing an old terminal status; then deliver old/new callbacks out of order. Assert only the new acknowledged run settles the new batch and its actual failure cannot be overwritten by old success.
- Hold a prerequisite migration behind an explicit barrier. Read committed app state and observe launch activity before releasing it. Cover success, failure, timeout and leader handover without a race-prone sleep.
- Test missing/stale reports and two admissions before runtime reports catch up. Assert reservations account for both pending and reported work without duplication.
- Rebuild routing while a backend remains failed; replace its address/generation and deliver a delayed old probe result. Assert the verdict belongs to the correct endpoint identity.
- Generate parser/control inputs around numeric extremes: cron field boundaries and maximal steps; zero, negative, NaN and infinite autoscale targets; backend capacity at 31/32/33 and rollout surge boundaries. Add a small release-profile parser test lane because debug overflow checks can hide release-only behavior.
- Keep a real ingress test active beyond the former 30-second limit, and real redirect/DNS transport tests. Use virtual time only where all timing consumers are injectable; retain the bounded wall-clock protocol test in its appropriate gate.

Assertions must state expected fixed behavior. The retained audit observation tests assert existing faults and must be inverted or replaced when adopted. A passing test that merely confirms an expired image is refused does not establish the promised retained-image deployment contract.

### Phase 4 — CI evidence, sensitivity and documentation

1. Extend the existing ignored-test/JUnit evidence checks to the new contract inventory. Verify exact required test names were discovered and executed by the assigned lane. Detect removed/renamed cases, empty applicable sets and filters that silently stop selecting a required combination. Preserve current job-selection tests and leak/timeout checks.
2. Run portable contract cases through the ordinary `make test`/coverage lane. Route privileged and real-council cases through their existing gates. Keep the small release-profile boundary lane separate and retain its named JUnit evidence. Manual-host cases must have a documented owner, command and qualification receipt.
3. Demonstrate test sensitivity with bounded, deliberate mutations in disposable checkouts: omit one scope check, compare a bare name instead of a run ID, drop a failed flush batch, or skip directory defaults. At least one named regression must fail for each representative mutation. Revert every mutation; none is part of the shipped implementation. Do not add a permanent expensive whole-repository mutation campaign to every PR.
4. Record per-family contract execution and relevant changed-code coverage alongside the existing aggregate coverage result. Keep the current aggregate floor; raising that percentage alone is not this issue’s completion criterion.
5. Update `docs/testing.md`, `docs/design/test-harness.md` and the affected existing book chapters, explaining the rules, test boundaries and why paired paths share domain logic. Link user-visible claims to their contract IDs and state unsupported behavior explicitly. Record exact commands, commit, host/runtime, failures and sensitivity checks in a dated qualification report.

### Completion criteria

- Every applicable matrix entry has an expected-behavior test and an execution owner; explicit-refusal cases cover unsupported paths.
- Equivalent inputs retain authorization, qualified identity and effective configuration meaning across the named entry points. Intentional differences are documented and tested.
- Lifecycle tests cover expiry, repeat execution, failed persistence, concurrent writers/admissions and restart with observable outcomes; no current-fault observation is mistaken for a passing regression.
- New shared rules are exercised through each production caller, including at least one real transport/runtime check per important family.
- Representative negative controls prove the regressions fail when the relevant rule is bypassed; all temporary mutations are removed.
- CI evidence fails when a required contract case disappears or is not executed. Existing portable, privileged, cluster and release gates remain effective, with new cases assigned appropriately.
- Required formatting, Clippy, doctest and applicable runtime checks pass on the final integrated train commit; documentation and a qualification receipt explain exactly what was exercised.
- All implementation children target and merge into one integration branch. The single train PR contains the combined result, current checklist and final qualification evidence for consolidated maintainer approval.

### Existing issue comparison

Drafts 01–27 own the concrete defects and their immediate fixes. This issue owns the final shared-contract architecture, fixtures and cross-path/lifecycle evidence. Existing #303/#304 cover ignored-test ownership and CI/JUnit evidence; build on those mechanisms rather than reopening their completed work. #351 and #505/#508 own agent-loop timing defects; retain their existing starvation harness as a model for deterministic adverse-condition tests.

### Current implementation snippets

These excerpts are verbatim from the audited baseline and illustrate why paired-path contracts are needed. Re-read the current implementation after the individual fixes; they may already have consolidated some of this logic.

Ordinary apply enforces scope and permission grants.

[src/bun/api/apply.rs:273–299](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L273-L299)

```rust
    for (app_name, namespace, host_execution) in targets {
        if let Err(resp) =
            crate::sesame::auth::authorize_scoped(auth.as_deref(), app_name, namespace)
        {
            return resp;
        }
        if let Err(resp) = crate::sesame::auth::authorize_permission(
            auth.as_deref(),
            crate::config::PermissionAction::Deploy,
            app_name,
            namespace,
            &permissions,
        ) {
            return resp;
        }
        if host_execution
            && let Err(resp) = crate::sesame::auth::authorize_permission(
                auth.as_deref(),
                crate::config::PermissionAction::HostExec,
                app_name,
                namespace,
                &permissions,
            )
        {
            return resp;
        }
    }
```

Batch checks the role and forwards before inspecting workload admission.

[src/bun/batch.rs:700–712](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L700-L712)

```rust
    // Submitting work is a Deployer action (AUTH2 — it used to take no auth).
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    // Followers forward the raw body to the leader (the tracker and
    // the aggregated capacity view live there).
    if let Some(council) = &state.council
        && !council.is_leader().await
    {
        return forward_to_leader(&state, council, "/v1/batch", body).await;
    }
```

CLI defaults have their own field-specific resolver.

[src/relish/compile.rs:194–209](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L194-L209)

```rust
/// Apply defaults to a config. For each app, if a field from defaults
/// is missing, inject it. Currently supports the `image` default.
fn apply_defaults(config: &mut Config, defaults: &BTreeMap<String, toml::Value>) {
    let default_image = defaults
        .get("image")
        .and_then(|v| v.as_str())
        .map(String::from);

    for app in config.app.values_mut() {
        if app.image.is_none()
            && let Some(ref img) = default_image
        {
            app.image = Some(img.clone());
        }
    }
}
```

GitOps parses and merges its directory contents separately.

[src/lettuce/sync.rs:257–281](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/sync.rs#L257-L281)

```rust
    for (path, content) in ordered {
        let file_config = match Config::parse(content) {
            Ok(config) => config,
            Err(e) => {
                errors.insert(path.clone(), e.to_string());
                continue;
            }
        };

        // A resource named in two files is ambiguous: report it against
        // this later-sorted file and let the earlier definition stand,
        // rather than silently letting hash order pick a winner.
        if let Some(duplicate) = first_duplicate(&merged, &file_config) {
            errors.insert(
                path.clone(),
                format!("duplicate resource {duplicate} already declared in an earlier file"),
            );
            continue;
        }

        merged.app.extend(file_config.app);
        merged.job.extend(file_config.job);
        merged.namespace.extend(file_config.namespace);
        merged.permission.extend(file_config.permission);
        merged.build.extend(file_config.build);
```

Shared desired-state writes alone do not cover parsing, imperative jobs or admission.

[src/council/apply.rs:1–19](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/council/apply.rs#L1-L19)

```rust
//! The one path that turns a parsed `Config` into desired-state writes
//! (12b.2 T6).
//!
//! Manual `relish apply` (`bun::api`) and GitOps sync (`lettuce`) both
//! call [`config_to_desired_writes`]. Sharing one function is what makes
//! "the same config converges identically whether you apply it by hand
//! or through git" true *by construction*: there's no second code path
//! that could drift.
//!
//! Only the declarative kinds live here: apps, namespaces, permissions.
//! Jobs run to completion (not reconciled desired state) and builds are
//! dispatched imperatively; both are validated but not written by this
//! function. See chapter 7 for why builds aren't a reconciling resource.
//!
//! Deletion is *not* the concern of this function. Manual apply is
//! additive: it writes what's in the file and never prunes what isn't,
//! matching how app apply already behaves. GitOps reconciles a whole
//! repo against desired state, so it computes deletions separately (in
//! `lettuce`) and layers them on top of these writes.
```

Completion matching lacks the execution generation needed by lifecycle tests.

[src/bun/batch.rs:461–475](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L461-L475)

```rust
fn job_outcome(statuses: &[InstanceStatus], name: &str, namespace: &str) -> Option<bool> {
    for status in statuses
        .iter()
        .filter(|s| s.app_name == name && s.namespace == namespace)
    {
        let outcome = match (status.state.as_str(), status.exit_code) {
            ("failed", _) => Some(false),
            ("stopped", Some(0) | None) => Some(true),
            _ => None,
        };
        if outcome.is_some() {
            return outcome;
        }
    }
    None
```

Persistence tests must exercise the drained batch and checkpoint together.

[src/ketchup/log_store.rs:600–613](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L600-L613)

```rust
    pub fn take_flush_batch(&mut self) -> Result<Option<LogPendingFlush>, KetchupError> {
        let Some(batch) = self.buffer_to_batch()? else {
            return Ok(None);
        };
        let filename = format!("logs_{:06}.parquet", self.flush_counter);
        let path = self.data_dir.join(filename);
        self.buffer.clear();
        self.flush_counter += 1;
        Ok(Some(LogPendingFlush {
            data_dir: self.data_dir.clone(),
            path,
            batch,
            checkpoint: self.ingested.clone(),
        }))
```

Reuse absolute deadlines in adapters and assertions instead of resetting budgets.

[src/testkit/deadline.rs:77–90](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/testkit/deadline.rs#L77-L90)

```rust

    /// Run a future without allowing it to exceed this deadline.
    pub async fn run<T, F>(&self, operation: &str, future: F) -> Result<T, DeadlineError>
    where
        F: Future<Output = T>,
    {
        tokio::time::timeout_at(self.expires_at, future)
            .await
            .map_err(|_| DeadlineError::Exceeded {
                operation: operation.to_string(),
                budget_ms: self.budget_ms,
            })
    }
}
```

### Suggested first child PR into the merge train

Record the dated plan when setting up the train. In its first draft-28 child PR, implement the contract inventory and reusable authenticated fixture. Implement one paired-path regression covering direct leader apply and follower-forwarded batch with the same scoped principal, including a permitted control and a refusal with zero side effects. Register it in the portable suite, retain its CI evidence, and demonstrate that a temporary scope-check bypass makes it fail. Then expand by family in the phase order above. Merge this child into the train after its focused and required checks, then continue the remaining children without separate full-qualification waits. This gives the next agent a bounded first deliverable and exercises the hardest fixture requirements early.

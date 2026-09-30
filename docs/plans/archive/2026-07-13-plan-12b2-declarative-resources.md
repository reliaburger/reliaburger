# Phase 12b.2 — Complete declarative resources (T6)

Theme: `docs/progress.md` §12b.2 "Complete declarative resources".
Findings: DEP7, D12.
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

**This is the FINAL theme of the 12b.2 tier** — checking its box completes
"Make the cluster converge". Acceptance is the tier's cleanest test: **one
config containing every resource kind (app, job, namespace, permission,
build) converges identically via manual `relish apply` and via GitOps.**

Runs after T5 (transactional desired state) lands, because "the same path"
is the unified desired-state path — and T5 has been reshaping `api.rs`
apply and `council/state_machine.rs`. Re-verify all line numbers below
(the seam map was taken on main before T5 PRs 2/3 merged).

## Ground truth (verified on main, 13 July — re-verify post-T5)

- Apply path: `apply_handler` (`src/bun/api.rs:892-953`) → `cluster_apply`
  (`api.rs:961-1076`). `cluster_apply` proposes `RaftRequest::AppSpec` for
  each `config.app` (~1008-1037) only; jobs run locally
  (`AgentCommand::Deploy`, ~1040-1060, "not cluster-scheduled yet");
  namespaces/permissions/builds are silently dropped.
- `Config::validate` (`src/config/validate.rs:11-24`) validates apps + jobs
  only; nothing for namespace/permission/build.
- State machine (`src/council/types.rs`): `DesiredState` (~242-305) has
  `apps`, scheduling, deploys, autoscale, security, batch/build trackers,
  recovery_epoch — **no `namespaces`/`permissions` fields**. `RaftRequest`
  (~97-195) has `AppSpec`/`AppDelete` but no namespace/permission variants.
- **T4 quota handoff is real and ready:** `QuotaLedger`
  (`src/meat/quota.rs:172-227`) is wired + tested but fed
  `QuotaLedger::default()` (empty) at `src/cluster/orchestrate.rs:150`
  because namespaces aren't desired state. Populating it from
  `desired.namespaces` is the whole point of strand 2.
- Config types exist and are production-dead: `NamespaceSpec`
  (`config/namespace.rs:11-22`: cpu/memory/gpu/max_apps/max_replicas),
  `PermissionSpec` (`config/permission.rs:9-18`: actions/apps/namespaces),
  `BuildSpec` (`config/build.rs:16-43`). `check_namespace_scope`
  (`pickle/build.rs:271-295`) is complete but only called in tests.
- **Lettuce D12 defects (`src/lettuce/`):** `runner.rs:112-133` applies
  changes one-by-one via `council.write` per change (non-atomic; a partial
  batch half-applies). `resource_change_to_request` (`runner.rs:163-176`)
  returns `None` for anything non-App → jobs/namespaces/permissions
  silently skipped. `runner.rs:135-146` advances `last_applied_commit`
  **regardless of per-change success** — the core D12 bug: a failed write
  is never retried because the commit is marked applied.

## Implementation strands (tests first for each)

### 1. Namespaces + permissions as desired state

- Add `NamespaceSpec`/`PermissionSpec` `RaftRequest` variants (+ their
  `Delete` forms) and `namespaces`/`permissions` maps to `DesiredState`
  (`council/types.rs`) — append-only, serde-default, self-describing JSON;
  a pre-theme snapshot must load cleanly (the #83 envelope loader fails
  loudly — fixture test).
- `cluster_apply` (`api.rs`) proposes namespace/permission writes alongside
  apps. Deletion semantics: an apply that omits a previously-declared
  namespace should NOT auto-delete it (apply is additive/declarative per
  file, not a whole-cluster reconcile) — document this and match how apps
  behave today. If apps DO get pruned on apply, mirror that; verify first.
- `Config::validate` (`validate.rs`) validates namespaces (resource
  budgets non-negative, max_apps/max_replicas sane) and permissions
  (known action verbs, referenced namespaces exist in the same config or
  desired state).
- Tests: applying a config with a namespace + permission writes both to
  Raft; validation rejects an unknown permission action and a
  negative/overflowing quota (reuse the DEP9 checked-arithmetic pattern).

### 2. Enforce namespaces (close the T4 quota handoff) + permissions

- Feed `QuotaLedger` from `desired.namespaces` in the scheduler pass
  (`orchestrate.rs:150` — replace `QuotaLedger::default()` with one built
  from desired-state namespace quotas). Now a namespace quota actually
  rejects over-budget placements at deploy time with a clear error on the
  `relish apply` path (the enforcement seam #92 built lights up).
- Permission enforcement: wire `PermissionSpec` into the API authorisation
  layer so a scoped principal's allowed actions/apps/namespaces are
  honoured (this connects to AUTH1 scope enforcement, a 12b.3 concern —
  scope this to making permission *resources* affect authz, and note the
  boundary with 12b.3 in the book rather than duplicating it).
- Tests: an app whose replicas exceed its namespace CPU quota is rejected
  on apply; a namespace with headroom admits it. Permission-resource test
  as far as the authz seam reaches.

### 3. Build resources + scope

- Decide and document: is a declarative `[[build]]` in a config file
  applied as *desired state* (a build that should exist/run) or validated-
  and-dispatched through the existing imperative `/v1/build` durable path
  (T7/#90)? Given builds are run-to-completion, the imperative path with
  desired-state *validation* is likely right — do not invent a reconciling
  build controller. Wire `Config::validate` to validate builds
  (`validate_build_namespace`) and `check_namespace_scope`
  (`pickle/build.rs:271-295`) against `desired.namespaces` so a build's
  push destination must target an existing namespace.
- Tests: a build targeting a non-existent namespace is rejected; a build
  scoped to an existing namespace validates.

### 4. Lettuce through the unified path, atomically (D12)

- `resource_change_to_request` handles apps, jobs, namespaces, permissions
  (no more silent `None` skips) — route every kind through the SAME
  `cluster_apply`-equivalent desired-state write used by manual apply
  (extract a shared "apply a parsed Config as desired state" function that
  both `api.rs` apply and Lettuce call, so they can't diverge — this is
  what makes the acceptance test's "identically" true).
- Atomicity: apply a sync's changes so a partial failure does not advance
  `last_applied_commit`. Options: batch the writes and only advance the
  commit if ALL succeed; or make the write set idempotent + retried and
  advance only once fully applied. Pick the simpler correct one — at
  minimum, `last_applied_commit` must NOT advance on any skipped/failed/
  partial change (`runner.rs:135-146`).
- Tests (extend `tests/gitops.rs`): a git repo with app+job+namespace+
  permission syncs all four to Raft; a sync where one write fails leaves
  `last_applied_commit` unadvanced and re-applies next tick; the reused
  clone's remote/branch is honoured (D12 also flags reused-clone checks).

### 5. Reject or wire every remaining parsed-but-dead field

Audit config types for fields that parse but cannot affect the binary
after strands 1-4 (the review's "reject or remove every parsed field that
cannot affect the binary"). For each: wire it, or remove it from the
config type and document the removal. Name what you found in the PR.

### Acceptance (the tier-closing test)

One config file containing an app, a job, a namespace (with a quota that
the app fits under), a permission, and a build converges to the same Raft
desired state via `relish apply` AND via a Lettuce GitOps sync of the same
file — assert byte-identical resulting `DesiredState` for the declarative
kinds. Add as a binary-driven test.

## Acceptance checklist

- `make ci` green (fmt, clippy `-D warnings`, full default suite); README
  counts from your run (measure — main will be ~2,399+ after T5).
- Gated cluster suite (`RELIABURGER_CLUSTER_TESTS=1`) stays green.
- `docs/progress.md`: nested `- [x]` items, **check the theme box** — and
  note the tier is complete.
- Book: chapter 7 ("Ship It") for the declarative-resources + GitOps-
  atomicity narrative (why apply and GitOps must share one path; why
  `last_applied` advancing on partial failure silently drops resources).
  British English, CLAUDE.md style guide, explain new Rust syntax on first
  use.

## Constraints

- Runs solo (T5 is the only other T-theme and finishes first). You own
  `src/config/*`, `src/bun/api.rs` apply path, `src/council/*` (namespace/
  permission desired state — substantive, following the AppSpec pattern),
  `src/meat/quota.rs` wiring at `orchestrate.rs:150`, `src/lettuce/*`,
  `src/pickle/build.rs` scope check wiring.
- The shared "apply a Config as desired state" function is the linchpin —
  build it once, call it from both api.rs and lettuce, so divergence is
  impossible by construction.
- New Raft state stays self-describing JSON and loads a pre-theme snapshot
  cleanly (fixture test).
- Error messages lowercase, no trailing full stop; thiserror; no unwrap in
  production code.

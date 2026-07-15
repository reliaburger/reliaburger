# Phase 12b.5 — GitOps convergence and webhook security (Theme G)

Theme: `docs/progress.md` §12b.5 "GitOps convergence and webhook security".
Findings: GIT2, GIT3, GIT4, D12 (GIT1 already done by #98). Source:
[2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

**1 PR** (`src/lettuce/*`, the webhook route + router split in
`src/bun/api.rs`, council gitops state).

## Harness contract (PR #106 is merged — this is now the law)

Green gate = **`make ci`** (nextest portable + doctest + no-default), NOT
`cargo test`. Deterministic tests only (retries=0, 60s timeout) — observable
synchronisation, no sleeps; the harness owns tasks/ports/temp. Env/cluster
tests → named `#[ignore]` + the right `make test-*` filter, never a silent
gate. **Do NOT update a headline test count** — update suite/design prose.

## Ground truth (verified post-#106)

- GIT1: DONE (#98). `runner.rs` advances `last_applied_commit` only on full
  success. Do NOT redo.
- GIT2a apps: removals key by bare name → `AppId::new(name, "default")`
  (`src/lettuce/runner.rs` ~245-266) — cross-namespace deletion diverges
  (the carried-forward post-#98 gap). GIT2b jobs: `current_job_names` is
  always an empty `BTreeSet` (`src/lettuce/diff.rs:181`), so every job is
  treated as Add — jobs never converge (no update/delete).
- GIT3: `WebhookValidator` (HMAC + replay + rate) exists
  (`src/lettuce/webhook.rs:15`) but has **no production caller** (only
  tests). The live route (`src/bun/api.rs:3376`-ish `gitops_webhook`)
  ignores the signature/body and sits behind bearer auth GitHub/GitLab
  can't supply.
- GIT4: clone reuse without remote/branch verify (`src/lettuce/git.rs:28`);
  missing `--` git option terminators (`git.rs:136,173,198` — option
  injection); non-deterministic HashMap file merge (`src/lettuce/sync.rs`
  ~208); `backoff_delay` (`sync.rs:229`) + `select_coordinator`
  (`src/lettuce/coordinator.rs:17`) defined but unused; hard sync errors
  eprintln-only, not durable in `SyncState` (`runner.rs:92`).

## Implementation steps (tests first for each)

### 1. GIT2 — namespaced diff + job convergence

- Key the diff (and removals) by a namespaced identity, not bare name, so a
  `prod/web` deletion targets `prod/web` not `default/web`. Reuse the
  `ServiceId`/`InstanceIdentity` namespacing convention where it fits, and
  the `config_to_desired_writes` app-id derivation.
- Job convergence: track current job state (from Raft desired state) so jobs
  update/delete, not just accumulate. `current_job_names` must reflect
  reality.
Tests: a `prod/web` app removed from git deletes `prod/web` (not
`default/web`); two same-named apps in different namespaces converge
independently; a job removed from git is removed (not re-added every sync).

### 2. GIT3 — public signature-authenticated webhook (the security headline)

- Wire `WebhookValidator` into the live webhook handler: validate the
  provider HMAC signature (`X-Hub-Signature-256` for GitHub /
  `X-Gitlab-Token` for GitLab), reject replayed delivery ids, apply the rate
  limit — BEFORE triggering a sync.
- Make the route **public** (exempt from the 12b.3 bearer-auth middleware —
  real Git providers can't send a bearer token) in the router public/
  protected split, but HMAC-gated in the handler. This edits the router
  split in `api.rs` (Theme G owns the webhook route + this split; the
  sibling metrics/logs agent does NOT touch it).
- Config: the webhook HMAC secret lives in `[gitops]` config.
Tests: a wrong/absent signature → 401/403 (no sync); a replayed delivery id
→ rejected; a valid GitHub-shaped signed request with no bearer token →
accepted and triggers a sync; rate limit trips on a flood.

### 3. GIT4 — git safety, determinism, backoff, coordinator, durable errors

- Clone reuse: verify the existing clone's remote URL + branch match config;
  re-clone/reset on drift (`git.rs:28`).
- Option injection: add `--` before user-controlled refs/paths in every git
  invocation (`git.rs:136,173,198`). Test: a ref/path beginning with `-`
  cannot inject a git option.
- Deterministic file merge: replace the HashMap-order `.extend()` overwrite
  (`sync.rs` ~208) with a deterministic order + a clear error/warning on a
  duplicate resource across files.
- Wire `backoff_delay` (`sync.rs:229`) into the runner's retry so failures
  back off instead of tight-looping; wire `select_coordinator`
  (`coordinator.rs:17`) so a single coordinator runs the sync (the runner is
  leader-only today — decide whether coordinator replaces or complements
  that and document).
- Durable errors: record hard sync failures (git/verify/apply) in `SyncState`
  (Raft) so `relish`/the UI can see them, not just stderr (`runner.rs:92`).
Tests: a ref/path with a leading `-` is safe; duplicate resource files
resolve deterministically (or error); a failed sync backs off; a hard
failure is recorded in SyncState.

## Acceptance

- `make ci` green (nextest portable + doctest + no-default). Any cluster/
  coordinator test → named `#[ignore]` + `make test-cluster`; quote results
  or state what's only unit-verified.
- `docs/progress.md`: nested `- [x]` items under "GitOps convergence and
  webhook security"; **check the theme box** (this completes it).
- Book: chapter 7 "Ship It" GitOps section — the webhook-security story
  (why a public HMAC route, not bearer; replay/rate control), namespaced
  convergence, git option-injection safety, durable sync errors. British
  English, CLAUDE.md style guide, explain new Rust syntax on first use. **No
  headline test count.**

## Constraints

- **Seam ownership:** `src/lettuce/*`, the gitops webhook route + the router
  public/protected split in `src/bun/api.rs`, the gitops state/coordinator
  in `src/council/*`, and the gitops runner spawn in `src/bin/bun.rs`
  (~1124). A sibling Theme O (metrics/logs) agent owns the metrics/logs
  handlers in `api.rs` and the collection/export tasks in `bun.rs` — stay
  out of those regions; do NOT touch `src/mayo/*` or `src/ketchup/*`.
- Council changes additive (serde-default; pre-theme snapshot loads cleanly
  — fixture test).
- Error messages lowercase, no trailing full stop; thiserror; no unwrap in
  production code; constant-time compare for the webhook HMAC (reuse the CT
  helper from #100 if present).

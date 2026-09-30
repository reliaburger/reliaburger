# Phase 12b.2 — Durable batch and build execution (T7)

Theme: `docs/progress.md` §12b.2 "Durable batch and build execution".
Findings: D18, JOB3–JOB7, old "batch allocation order" Low.
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Already landed in #85 (do not redo): unique per-build `ScopedDir` temp
dirs with cleanup on every exit path, sandboxed/bounded context
extraction, Dockerfile confinement, Buildah process-group kill on
timeout, server-owned registry destination, `require_system` on run/report
routes. JOB5 was re-validated as NOT reproducing as written — the residual
"locally-uploaded `_buildcontext` is neither replicated nor catalogued"
concern is yours to confirm and close (or refute with a test).

Out of scope: scope enforcement in authorize (12b.3), the desired-state
reconciler rework (T5 — batch dispatch stays direct HTTP, not the
placements reconciler, by design; see the Phase 12 note in progress.md).

## Ground truth

- JOB3: batch has two namespace sources — the CLI can deploy into one
  namespace while the tracker watches another for an hour. Non-success
  dispatch/callback responses are ignored; unreachable jobs stay pending
  forever; `/v1/batch/{id}/report` accepts forged/unknown state
  transitions; unschedulable jobs are silently omitted from the batch;
  callback delivery has no durable retry.
- JOB4: `BatchTracker` (`src/meat/batch_tracker.rs`, in-memory
  `HashMap<u64, BatchState>` + `next_id`) and the `BuildRegistry`
  (in-memory in `src/bun/api.rs:103`) are process-local: leader restart
  loses/reuses IDs and orphans in-flight work; nothing GCs terminal
  state; `HashMap` iteration makes some allocation non-deterministic (the
  old Low — use `BTreeMap` or sort before allocating).
- JOB6 residue: CLI polling (`relish batch-status` wait loops /
  `relish build` poll) has no terminal bound or cancellation.
- JOB7: required signing is best-effort — the build path can create an
  ephemeral external key no policy trusts, and `AttachSignature` can
  no-op for an unknown digest while the build still reports success.
- D18: no retry to another builder when the chosen one fails; registry
  endpoints partially derived from requests (the #85 fix covered the
  submit path — audit the delegated path); build context transfer relies
  on the entry node's local registry.
- Raft plumbing that exists: placements/deploys already live in the state
  machine (`src/council/state_machine.rs:200–257`); follow that pattern.
  Self-describing JSON only (bincode rejected — Stage 4 note).

## Implementation steps (tests first for each)

### 1. One authoritative namespace (JOB3)

The namespace travels once, server-side: the submit handler resolves it
(request field validated against the job specs, or derived from them) and
the tracker, dispatch and completion watching all use that single value.
Test: a submit whose CLI-supplied namespace disagrees with the specs is
rejected; end-to-end batch in a non-default namespace completes.

### 2. Durable IDs, trackers and terminal state (JOB4)

- Batch and build IDs become monotonic and Raft-allocated (a counter in
  the state machine — append-only new `RaftRequest` variants; keep the
  wire self-describing JSON).
- Tracker state (batch job states, build states including `Delegated`)
  persists to Raft on transitions; a restarted leader reconstructs
  in-flight batches/builds from the state machine and resumes watching
  (re-poll dispatched nodes for liveness rather than assuming).
- Terminal-state GC: completed/failed batches and builds older than a
  retention window are pruned via a periodic leader task (follow the
  deploy-history cap-at-50 precedent).
- Determinism: allocation iterates jobs in a stable order (sort or
  `BTreeMap`) — pin with a test that a same-input batch produces the same
  assignment plan.

### 3. Bounded, retried dispatch and callbacks; idempotent reports (JOB3)

- Dispatch checks responses; a non-2xx or unreachable node marks the job
  for redispatch (bounded retries, then Failed with reason) instead of
  pending-forever. Unschedulable jobs appear in the batch result as
  `Unschedulable`, not omitted.
- Completion callbacks get bounded retries with backoff, and the tracker
  also *pulls* (poll the running node) as a liveness backstop so a lost
  callback cannot strand a batch (test: drop the callback, batch still
  terminates).
- `/v1/batch/{id}/report` validates the transition: unknown batch/job or
  an illegal state transition is rejected; duplicate terminal reports are
  idempotent (200, no double-counting). Forged-state test stays green
  with `require_system` (#85) plus transition validation.

### 4. Build: context transfer, endpoint derivation, builder retry (D18/JOB5)

- Confirm-or-close JOB5 residual: when a build is delegated, the context
  blob must be readable by the builder — either replicate it (catalogue
  the `_buildcontext` repo through the normal holder/heal path, which
  REG1's `referenced_digests()` machinery now supports) or transfer it
  directly on delegation; test delegation from a non-builder entry node
  whose context exists only locally.
- Audit the delegated-run path for any registry endpoint still derived
  from request data; derive from node config/membership only.
- Builder failure → retry on another capable builder (bounded), then
  honest terminal failure.

### 5. Signing as part of terminal success (JOB7)

When the cluster policy requires signatures, the build is `Completed`
only after `AttachSignature` succeeds for the pushed digest with a key
that the policy trusts (workload/cluster key, not an ephemeral untrusted
one); signature failure → build Failed with reason. `AttachSignature` for
an unknown digest is an error, not a no-op. Test both.

### 6. Bounded CLI polling (JOB6 residue)

`relish batch-status --wait`/`relish build` polling gets a `--timeout`
(default sane, e.g. matching `build_timeout_secs` + margin) and clean
Ctrl-C behaviour; on timeout, exit non-zero with the last known state.

### 7. Acceptance tests

- Leader restart mid-batch and mid-build (in-process harness or the
  binary-driven pattern from `tests/batch.rs`/`tests/build.rs`): work
  resumes or terminates honestly, IDs never reused.
- Lost callback → batch still terminates (pull backstop).
- Delegation from a non-builder entry node (context transfer) — buildah-
  gated where a real build is needed (`RELIABURGER_BUILDAH_TESTS`),
  mock-runner variant in the default suite.

## Acceptance checklist

- `make ci` green (fmt, clippy `-D warnings`, full default suite; main is
  at 2,175 tests — set the new true count in README.md/docs/README.md,
  rebasing on sibling PRs as they merge).
- Gated results (buildah/Lima) quoted in the PR or explicitly listed as
  unverified.
- `docs/progress.md`: nested `- [x]` items, theme box checked.
- Book: chapter 12 (`docs/book/12-squeezing-every-drop.md`) batch/build
  sections — the durability story (why in-memory trackers and fire-and-
  forget callbacks fail a distributed system, pull-as-backstop, signing
  in the terminal-state definition); touch chapter 8 only if its batch
  prose now reads wrong. British English, CLAUDE.md style guide, explain
  new Rust syntax on first use.

## Constraints

- **Seam ownership:** `bun/batch.rs`, `bun/build_runner.rs`,
  `meat/batch_tracker.rs`, `pickle/build.rs` and the batch/build routes in
  `bun/api.rs` are yours. `state_machine.rs` changes append-only (new
  variants + state fields with serde defaults) — two sibling agents are
  working elsewhere in the tree and one may also append there on a later
  wave. Do not touch `cluster/runtime.rs`, `reporting/*`,
  `council/selection.rs`, or `src/bun/agent.rs`.
- Raft snapshot compatibility: new state fields must default cleanly when
  loading a pre-theme snapshot (serde defaults; fixture test — the #83
  envelope loader will surface decode failures loudly).
- Error messages lowercase, no trailing full stop; thiserror; no unwrap
  in production code.

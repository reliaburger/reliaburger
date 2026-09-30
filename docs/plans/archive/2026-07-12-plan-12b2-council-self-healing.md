# Phase 12b.2 — Council membership self-healing (T2)

Theme: `docs/progress.md` §12b.2 "Council membership self-healing".
Findings: H2/D2 (P0).
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Tier acceptance owned by this theme: **quorum recovers with healthy spares
and the reconciler never removes the active leader mid-change.**

Out of scope: full-council-loss disaster recovery, encrypted backups and
disk-pressure resignation (T3, next wave — but design your reconciler so
T3 can hang recovery hooks off it), the reporting/directory work (a
sibling T1 agent owns `cluster/runtime.rs` ~335–481 and `reporting/*`).

## Ground truth

- H2/D2: the council reconciler
  (`src/cluster/runtime.rs::spawn_council_reconciler`, ~567–658, with
  `compute_desired_council` at ~522–565) only **grows** the voter set: it
  retains every current voter unconditionally (the L5 fix's "never demote
  the leader" became "never remove anyone") and adds members via
  `add_learner` + `change_membership`. A dead voter occupies its seat
  forever; with a 5-voter council and 2 dead, quorum is 3-of-5 with only
  3 alive — one more failure loses the cluster even with 10 healthy
  spares gossiping.
- `src/council/selection.rs` (~675 lines) has the tested scoring
  (stability, zone diversity, size bounds 3–7) — reuse it to pick
  replacements; do not reinvent scoring.
- Gossip states (Alive/Suspect/Dead/Left, `src/mustard/`) are the health
  signal. SWIM refutation works (H4–H7 fixed), so Dead is a reasonably
  strong signal, but flapping must not churn membership — hysteresis is
  required (a voter must be Dead/Left for a sustained window before
  removal; a candidate must be Alive for a sustained window before
  promotion).
- openraft supports learners and joint-consensus membership changes;
  leadership transfer exists in the self-upgrade path (Phase 14 — check
  `src/upgrade/` for the existing leadership-transfer call to reuse).

## Implementation steps (tests first for each)

### 1. Pure replacement planner

A pure, unit-testable function (next to `compute_desired_council` or in
`council/selection.rs`) that takes the current voter set, Raft health
(replication lag/last-contact from metrics), gossip membership with
states, and spare candidates, and returns a **single next action**:
`AddLearner(node)`, `Promote(node)` (learner caught up), `RemoveVoter
(node)`, `TransferLeadershipThen(remove self)` or `Nothing`. One change
in flight at a time — the planner never proposes a second change while
one is pending. Rules:

- Never remove the current leader; if the leader itself must leave (T3
  will need this seam), plan a leadership transfer first (reuse the Phase
  14 transfer call).
- Never let planned changes take live voters below quorum of the *current*
  configuration.
- Prefer add-before-remove: bring the replacement learner in, wait for
  catch-up, promote via joint consensus, then remove the dead voter —
  quorum never depends on the dead node's vote returning.
- Hysteresis windows on both death and candidacy (config with sane
  defaults, e.g. 30s dead-window; document under `[cluster]` or the
  existing council config section).

Proptest candidate: for arbitrary (voters, health, spares) states, the
planner never proposes removing the leader, never goes below quorum, and
proposes at most one action.

### 2. Catch-up gating

Promotion requires the learner to be caught up: compare the learner's
replicated log index (openraft metrics) against the leader's committed
index within a bound. Test with a learner that is artificially behind
(feed metrics through the planner's inputs — pure function, no cluster
needed).

### 3. Drive it in the reconciler

Rework `spawn_council_reconciler` (~567–658 — **you own this region**;
do not touch ~335–481, a sibling agent owns the reporting glue there) to
execute the planner's single action with timeouts and logged errors (keep
the M15 non-wedging property), re-planning each tick from observed state
rather than assuming the last action succeeded. Joint consensus via
openraft `change_membership` with retain-learners semantics; failures are
logged and retried next tick (idempotent planning makes this safe).

### 4. Acceptance tests

Env-gated (`RELIABURGER_CLUSTER_TESTS=1`, in-process harness per the
`tests/agent_cluster.rs` pattern):

- 3 voters + 2 spares; kill one voter; within a bounded time the council
  is 3 healthy voters (spare promoted, dead removed), the cluster commits
  writes throughout, and the leader was never removed.
- Kill a *learner* mid-catch-up: no membership change lands until a
  healthy learner catches up.
- Flapping node (Dead then Alive within the hysteresis window): no churn.

Default-suite: the planner/catch-up unit tests + proptest above; a
state-machine-level test that joint-consensus requests compose (add then
remove) against the in-memory Raft harness used by existing council
tests.

## Acceptance checklist

- `make ci` green (fmt, clippy `-D warnings`, full default suite; main is
  at 2,175 tests — set the new true count in README.md/docs/README.md,
  coordinating expectation: a sibling PR may land first, rebase then).
- Gated suite results quoted in the PR.
- `docs/progress.md`: nested `- [x]` items, theme box checked.
- Book: chapter 2 (`docs/book/02-finding-friends.md`) council section —
  the add-before-remove/joint-consensus story, why "never shrink" was the
  previous bug and how hysteresis prevents flap-churn. British English,
  CLAUDE.md style guide, explain new Rust syntax on first use.

## Constraints

- **Seam ownership:** `cluster/runtime.rs` ~522–658 + `council/
  selection.rs` are yours; ~335–481 (reporting/leader-target) belongs to
  a sibling agent running now — do not edit it. `state_machine.rs` is
  being appended to by another sibling (batch/build state); if you must
  touch it, keep changes append-only and minimal.
- Do not implement full-council-loss recovery, backups or disk-pressure
  resignation (T3) — leave the planner seam clean for it.
- Error messages lowercase, no trailing full stop; thiserror; no unwrap
  in production code.

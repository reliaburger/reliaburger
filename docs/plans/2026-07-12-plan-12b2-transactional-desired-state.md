# Phase 12b.2 — Transactional desired state and deployment (T5)

Theme: `docs/progress.md` §12b.2 "Transactional desired state and deployment".
Findings: H7/D10, DEP1–DEP6, codex-M3.
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

This is the tier's largest theme and it restructures the seams every other
theme hangs off, so it runs **solo** (no concurrent sibling) and lands as
**2–3 sequential PRs**, each with nested `- [x]` items; the theme box is
checked only when the last PR merges. Re-verify all line numbers below —
several PRs (#88–#93, the TUI #91) have moved code since the seam map was
taken.

## Ground truth (verify against current main before editing)

- DEP1: instance IDs omit namespace. `default/api` and `payments/api` both
  key `api-0` in the supervisor; rolling (`{app}-g{gen}-{i}`) and job IDs
  have the same collision class. Stop/restart/adoption/status can hit the
  wrong workload. Two ID formats exist (`{app}-{index}` and the canary
  `{app}-g{gen}-{i}`) — unify into one identity carrying namespace +
  generation + ordinal. ~20–30 call sites (supervisor, reporting, health,
  log-forwarding, service-map, adoption) — greppable via the newtype.
- DEP2: cluster stop sends only `AgentCommand::Stop`; it never proposes
  the `AppDelete` Raft request, so the desired app persists and
  reconciliation can resurrect it. A leader without a local replica can
  404 instead of deleting cluster state.
- DEP3/H7/D10: placement is marked "applied" when the command enters the
  queue (the process-local `applied` fingerprint map in
  `cluster/orchestrate.rs`, ~389–391 pre-merge — re-find); deploy events
  are discarded; a restart forgets work it must reconcile. The durable,
  health-gated deploy controller/rollback state machine
  (`meat/orchestrator.rs`) exists but is library-only.
- DEP4/codex-M3: image preparation, init polling, runtime ops and rolling
  health waits run serially in the central agent task
  (`src/bun/agent.rs` command loop). One slow deploy stalls health checks,
  restarts and every later command.
- DEP5: rolling deployment bypasses egress programming (the NET6 half is
  fixed; the drain/surge half is unverified — confirm) and does not use
  Wrapper's drain tracker. `max_unavailable`, surge, `drain_timeout`,
  automatic rollback do not reliably govern the live path.
- DEP6: ordinary stop can report Stopped without waiting for exit and
  escalating; container state and supervisor state diverge.

## PR split (agent finalises; this is the recommended shape)

### PR 1 — Instance identity + durable applied-state (DEP1, DEP2, DEP3)

- New instance identity: a newtype carrying `{namespace, app,
  generation, ordinal}` with one canonical string form; replace both
  existing formats. Migrate every call site; adoption must parse both the
  new form and (best-effort) the legacy form so an in-place upgrade across
  the change re-adopts running workloads rather than orphaning them
  (fixture/round-trip test; mirror the #87 adoption-compat approach).
- Cluster stop proposes `AppDelete` through Raft (DEP2), so desired state
  actually clears and reconciliation cannot resurrect the app; a leader
  without a local replica still deletes cluster state (no spurious 404).
- Durable applied-state (DEP3/H7/D10): the placement reconciler records
  what it has actually applied in a form that survives a Bun restart
  (Raft or a durable local checkpoint reconciled against Raft on boot),
  and marks a placement applied on the deploy's *terminal outcome*, not on
  queue acceptance. Consume the reconstruction corrections T-earlier work
  left on the floor (CP4 is 12b.2's separate concern, but if the seam is
  right here, wire the applied-state to honour `MissingApp`/`ExtraApp`).
- Tests: two same-name apps in different namespaces coexist (the DEP1
  collision regression); cluster stop clears desired state (no
  resurrection next tick); a simulated restart re-derives applied-state
  and does not double-deploy or forget in-flight work.

### PR 2 — Deploy work off the command loop (DEP4/codex-M3)

Move image preparation, init-container polling, runtime create/start and
rolling health waits out of Bun's central command task into per-deploy
spawned tasks that report back via channels, so a slow deploy no longer
blocks health checks, restarts or later commands. This is the big
`agent.rs` restructure — keep the state machine authoritative (the spawned
task drives transitions through the supervisor, it does not own state).
Preserve every existing behaviour (health-gating, crash-restart, the #86
pre-start networking ordering, the #87 per-instance identity lifecycle).
Tests: a deploy that blocks on image pull does not delay a health-check
transition on another app; concurrent deploys of two apps interleave.

### PR 3 — Deploy semantics (DEP5, DEP6)

- Rolling deploy honours `max_unavailable`, surge, `drain_timeout` and
  automatic rollback through the live path, using Wrapper's drain tracker
  (wire the library-only tracker in) so in-flight HTTP/WebSocket traffic
  drains before an old instance dies.
- Ordinary stop waits for actual exit and escalates SIGTERM → grace →
  SIGKILL (the M12 machinery exists — ensure the stop *reporting* waits
  for it), so container and supervisor state cannot diverge (DEP6).
- Tests: a rolling deploy with `max_unavailable=1` never drops more than
  one instance; a draining instance finishes an in-flight request before
  termination; stop reports Stopped only after exit.

## Acceptance (whole theme)

- Each PR: failing-first tests, `make ci` green (fmt, clippy `-D
  warnings`, full default suite), README counts updated from the run.
- The gated cluster suite (`RELIABURGER_CLUSTER_TESTS=1`) stays green —
  your identity and applied-state changes sit under
  `tests/cluster_failover.rs`/`tests/placement.rs`.
- `docs/progress.md`: nested `- [x]` per PR; theme box checked on the last.
- Book: chapter 7 ("Ship It") is the natural home for the deploy-semantics
  and durable-applied-state narrative; chapter 2 for the instance-identity
  change. British English, CLAUDE.md style guide, explain new Rust syntax
  on first use.

## Constraints

- Runs solo — no concurrent sibling. You own `src/bun/agent.rs`,
  `src/cluster/orchestrate.rs` (placement reconciler), `src/meat/
  orchestrator.rs`, the instance-identity newtype and its call sites,
  `src/council/state_machine.rs` (AppDelete/applied-state — this time you
  may change it substantively, not just append).
- A small disk-pressure→gossip follow-up PR may land on main while you
  work (it touches `mustard/*`, the council reconciler in
  `cluster/runtime.rs`, `disk_pressure.rs` — not your seams). Rebase on it
  if it merges first; the only likely overlap is mechanical in
  `src/bin/bun.rs` wiring.
- Instance-identity wire/state changes stay self-describing JSON and load
  a pre-theme snapshot cleanly (the #83 envelope loader fails loudly on
  decode errors — fixture test).
- Error messages lowercase, no trailing full stop; thiserror; no unwrap in
  production code.

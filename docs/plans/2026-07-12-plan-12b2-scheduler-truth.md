# Phase 12b.2 — Scheduler truth, labels, quotas and autoscaling (T4)

Theme: `docs/progress.md` §12b.2 "Scheduler truth, labels, quotas and
autoscaling". Findings: CP7, CP8 (re-validated scope), DEP8 (re-validated
scope), DEP9, D13's scheduling slice.
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Builds on (merged, read first): #89's gossip `DirectoryExtension` (the
authenticated per-datagram extension carrying endpoints + leader hint —
the natural vehicle for labels), #89's epoch-scoped honest aggregation
(CP5) and terminal-workload filtering (CP6) — the reports the scheduler
reads are now truthful; this theme makes the scheduler *use* them
correctly.

Out of scope: per-app metric *collection* (OBS3, 12b.5 — autoscaling
correctness lands here, signal availability lands there; be honest about
that in the book), council recovery (a sibling T3 agent owns `council/*`
and the reconciler/recovery regions of `cluster/runtime.rs`), desired-
state transactionality (T5, next wave).

## Ground truth

- CP7: node labels parse from node.toml (`src/config/node.rs:279`) but
  `src/mustard/protocol.rs:73` inserts an empty map, and the wire never
  carries them — remote members always have empty labels, so
  `filter_nodes`' label filtering and zone-aware council selection are
  inert. #89 added `DirectoryExtension` (HMAC-authenticated, version-
  tolerant); carry labels there.
- CP8 (re-validated): `filter_nodes` does check resources/labels/
  readiness at placement time; the real defect is the scheduler builds a
  fresh resource cache per app (`build_cluster_cache` called inside the
  per-app loop — verify current shape at `src/cluster/orchestrate.rs`
  ~65–182) so concurrently-planned apps double-book the same headroom.
  One mutable reservation cache per planning pass: each commitment
  subtracts from it before the next app plans.
- Revalidation set (theme text): generation, resources, labels, readiness
  and cordon before committing a decision. The upgrade-cordon helper
  exists and is ready (`meat::filter::apply_upgrade_cordon`, Phase 14
  deferred-wiring note) — wire it into filtering; 12b.6 then only has to
  verify it.
- Daemon workloads: converge against *eligible* nodes — a daemon app must
  gain an instance when a node joins/becomes eligible and lose it when
  the node leaves, not only at first scheduling.
- Quotas: `meat`'s namespace-quota logic is Phase 2 library code with no
  production caller (the old L1 note "quotas never integrated"). Wire it:
  the scheduler rejects placements that exceed a namespace's quota
  resource, with a clear deploy-time error surfaced to `relish apply`.
  (Namespace *resources* as desired state land with T6; until then read
  quotas from wherever they exist today — if they are genuinely
  unreachable pre-T6, implement the enforcement seam + tests against an
  injected quota table and note the T6 handoff in progress.md.)
- DEP8 (re-validated): autoscale cooldown is checked before the Raft
  write commits (a failed write still starts the cooldown); stale
  overrides survive when the app spec's replica baseline changes or the
  app is deleted; min>max is clamped (fix to a validation error);
  windows/durations unvalidated. `spawn_autoscaler` at
  `cluster/orchestrate.rs` ~208–277; overrides applied in
  `council/state_machine.rs` ~258–273 (append-only there — a T3 sibling
  may also touch the file; keep your diff additive and small).
- DEP9: numeric size/resource parsing can overflow (e.g. huge memory
  strings) instead of returning a validation error — audit
  `config` parse paths for unchecked multiplications (`* 1024`,
  `as u64` casts) and use checked arithmetic with proper errors.

## Implementation steps (tests first for each)

### 1. Labels travel (CP7)

Extend #89's `DirectoryExtension` with the node's labels (BTreeMap;
bounded — reject/truncate absurd label sets with a documented cap, e.g.
32 keys × 128 bytes, so gossip datagrams stay well under MTU-ish sizes).
Membership stores them; `build_cluster_cache` exposes them; scheduler
label filtering becomes live. Keep both-direction version tolerance
(pinned like #89's legacy-decoder tests). Zone-aware council selection
inputs stop being empty (verify with a test that a zone label reaches
`selection` — do not otherwise touch `council/selection.rs`, T3 owns it).

### 2. One reservation cache per planning pass (CP8)

Restructure the scheduler tick: build the cluster cache once, then plan
every app against a mutable reservation view (committed decisions
subtract immediately). Revalidate per decision just before the Raft
commit: generation unchanged, node still Alive/ready/uncordoned, labels
still satisfied, reserved resources still fit. Wire
`apply_upgrade_cordon` into the filter stage. Tests: two apps that
together exceed one node's capacity get spread/one-rejected instead of
double-booked (regression for the double-count); a cordoned node receives
nothing; a decision against a node that died mid-pass is dropped.

### 3. Daemon convergence

Scheduler tick reconciles daemon apps against current eligible nodes:
missing instance on an eligible node → place; instance on a newly
ineligible/departed node → remove decision. Tests: node joins → daemon
gains a placement next tick; node cordoned → placement removed.

### 4. Namespace quotas wired

Enforcement in the placement pass (cumulative per-namespace usage vs
quota; clear error on the deploy path). See ground-truth caveat: if quota
specs are unreachable before T6, land the enforcement seam against an
injected table + tests, and note the handoff.

### 5. Autoscaling correctness (DEP8 + DEP9)

- Validation: min ≤ max required (error, not clamp), windows/durations
  bounded and validated at config parse; checked arithmetic in
  size/resource parsing with `QuotaError`-style messages (DEP9).
- Cooldown starts only after the Raft write commits (move the tracker
  update behind the successful write).
- Stale overrides: an override is cleared (Raft write) when the app spec
  baseline changes (redeploy with different replicas) or the app is
  deleted; the scheduler ignores overrides older than their app's
  current generation. Tests: baseline change clears the override;
  deletion clears it; failed Raft write does not start cooldown.
- Use configured evaluation windows rather than hardcoded ones; metric
  queries stay namespace-scoped (they already are — pin with a test).

## Acceptance checklist

- `make ci` green (fmt, clippy `-D warnings`, full default suite); update
  README.md test counts from your run (measure — main includes the Phase
  13 TUI draft; rebase on the sibling T3 PR if it lands first).
- The gated cluster suite (`RELIABURGER_CLUSTER_TESTS=1`) still passes —
  your scheduler changes sit under `tests/cluster_failover.rs`'s
  placement assertions; run it and quote results.
- `docs/progress.md`: nested `- [x]` items, theme box checked.
- Book: chapter 2's scheduler section and chapter 9's autoscaling section
  where the prose now reads wrong (the double-booking lesson: a cache you
  rebuild per decision is a cache that lies between decisions). British
  English, CLAUDE.md style guide, explain new Rust syntax on first use.

## Constraints

- **Seam ownership:** `src/meat/*`, `src/mustard/*` (the
  DirectoryExtension + membership label plumbing), the scheduler and
  autoscaler regions of `src/cluster/orchestrate.rs` (~65–277), config
  parse paths for DEP9. Do NOT touch `src/council/selection.rs`, the
  reconciler/recovery regions of `src/cluster/runtime.rs`, `src/bun/
  disk_pressure.rs` (sibling T3 owns them concurrently), or `src/bun/
  agent.rs`. `council/state_machine.rs`: additive, minimal (override-
  clearing writes) — T3 may also touch it.
- Preserve #89's wire-compat pinning style for any gossip change.
- Error messages lowercase, no trailing full stop; thiserror; no unwrap
  in production code.

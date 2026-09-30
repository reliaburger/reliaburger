# Phase 12b.2 — Council disaster recovery (T3)

Theme: `docs/progress.md` §12b.2 "Council disaster recovery".
Findings: D21, CP12 (the re-validated remainder: full-council-loss recovery,
encrypted external backup/restore, explicit reconstruction thresholds,
disk-pressure council *resignation* — the disk-pressure *check* exists).
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Tier acceptance owned by this theme: **black-box full-council loss and
recovery**, not just candidate-selection helpers.

Builds on (merged, read first): #88's self-healing machinery —
`plan_council_action`/`CouncilAction` in `src/council/selection.rs`, the
reconciler in `src/cluster/runtime.rs` (~`reconcile_council_once`/
`execute_council_action`), `change_membership_evicting`/`remove_learner`
on `src/council/node.rs`. #83's snapshot envelope (versioned, checksummed)
is the natural backup payload format. #89's gossip directory tells
survivors where everyone is.

Out of scope: scheduler/autoscale work (a sibling T4 agent owns `meat/*`,
`mustard/*` and the scheduler/autoscaler regions of
`cluster/orchestrate.rs`), TPM sealing (deferred to v2).

## Ground truth

- D21/CP12: if every voter dies, the cluster is unrecoverable today —
  workers keep running workloads but nothing can elect, schedule or serve
  cluster state; there is no backup, no restore path, and no operator
  command. The whitepaper (§§8.2–8.3) promises reconstructable state.
- Reconstruction machinery exists and is wired at the leadership edge
  (learning period, 95%/15s — `src/reconstruction/`), but its thresholds
  are hardcoded rather than explicit config, and it assumes a surviving
  Raft state to diff against.
- `src/bun/disk_pressure.rs` detects pressure; nothing acts on it for
  council membership.
- **Known gap from #88:** openraft 0.9 exposes no graceful leadership
  transfer (`Raft::trigger()` has elect/heartbeat/snapshot/purge only).
  The planner made "never remove the leader" absolute and left the
  transfer seam to this theme. Resolve it: the pragmatic mechanism is
  `trigger().elect()` on a chosen healthy voter (it campaigns with a
  higher term and deposes the current leader), after which the deposed
  node is an ordinary voter the #88 planner can remove. Verify this works
  against openraft 0.9 in a test before building on it; only if it
  proves unreliable, evaluate the openraft version bump as a fallback
  (flag BEFORE taking it — it is a wire/storage compatibility risk).

## Implementation steps (tests first for each)

### 1. Encrypted external council backup

A leader-only periodic task exports the state-machine snapshot (reuse the
#83 envelope bytes — version + checksum already inside) sealed with a key
derived from the cluster master key (HKDF, new info string; every node
holds the master key via `/etc/reliaburger`, so any surviving node can
restore — follow the existing derived-key patterns in `sesame`). Upload
via `object_store` (`file://`/`s3://`/`gs://` — the snapshot-worker and
log-export precedents). Config: `[cluster.backup] { url, interval_secs,
retain }` (off by default). Retention pruning; the `uploaded`-flag
checkpoint pattern from `bun/snapshot_worker.rs` applies. Tests: seal →
tamper → restore refuses; round-trip restores identical DesiredState;
retention prunes oldest; disabled by default.

### 2. Full-council-loss recovery

Operator-triggered: `relish council recover --from <backup-url or
node-data-dir> [--force]` against a chosen surviving node. The node
verifies no live council exists (gossip directory shows no voter alive;
`--force` overrides the check with a loud warning), restores the state
machine from the backup (or its own durable snapshot if it was a voter),
re-bootstraps a fresh single-voter Raft with a **new epoch marker** in the
restored state (so pre-loss reports/tokens are distinguishable), then the
#88 reconciler grows the council from healthy members as usual. After
recovery, the reconstruction learning period runs against live worker
reports before scheduling resumes (existing gate). Document what is lost:
writes after the last backup. Tests: state-machine-level restore +
re-bootstrap; refusal when a voter is still alive; `--force` override.

### 3. Explicit reconstruction thresholds

Promote the hardcoded 95%/15s to `[reconstruction] { coverage_percent,
timeout_secs }` (config section exists — check what `[reconstruction]`
already carries from Stage 4 W9 and extend, don't duplicate). Validate
bounds (0 < coverage ≤ 100, timeout > 0). The recovery path (step 2) and
the ordinary leadership edge use the same values. Tests: config parsing
bounds; the gate honours a non-default coverage.

### 4. Disk-pressure council resignation

When a voter's disk pressure crosses the (existing) threshold for a
sustained window: if follower, mark itself unsuitable so the #88 planner
replaces it (a new `ObservedMember` health input — extend the planner's
inputs, not its invariants); if leader, first depose itself via the
verified `trigger().elect()` mechanism (step 0 above), then get replaced
as an ordinary voter. Hysteresis so a node oscillating around the
threshold doesn't churn the council. Tests: planner treats a
disk-pressured voter as replaceable-but-healthy-enough-to-wait (never
below quorum — existing proptest must keep passing with the new input);
leader deposition test (gated).

### 5. Acceptance: black-box loss and recovery

Gated (`RELIABURGER_CLUSTER_TESTS=1`, in-process harness): a 3-voter +
2-worker cluster with the backup task running; kill all three voters;
run the recovery flow on a worker; assert the council re-forms, restored
desired state matches (apps still known), reconstruction completes
against the surviving workers' reports, and a new placement converges.
Plus: leadership-deposition test (leader resigns under injected disk
pressure and the planner replaces it) — this doubles as the transfer-seam
verification.

## Acceptance checklist

- `make ci` green (fmt, clippy `-D warnings`, full default suite); update
  README.md test counts from your run (main should be ~2,250 area after
  the TUI merge — measure, don't assume; rebase on the sibling T4 PR if
  it lands first).
- Gated suite results quoted in the PR.
- `docs/progress.md`: nested `- [x]` items, theme box checked.
- Book: chapter 2 (`docs/book/02-finding-friends.md`) — disaster recovery
  section: what "reconstructable state" costs (backup freshness), why the
  election-deposition trick substitutes for leadership transfer in
  openraft 0.9, and the epoch marker. British English, CLAUDE.md style
  guide, explain new Rust syntax on first use.

## Constraints

- **Seam ownership:** `src/council/*`, the reconciler/recovery regions of
  `src/cluster/runtime.rs`, `src/bun/disk_pressure.rs`, new backup module,
  `relish` command plumbing. Do NOT touch `src/meat/*`, `src/mustard/*`
  (except read-only use of the directory), or the scheduler/autoscaler
  regions of `src/cluster/orchestrate.rs` (~65–277) — a sibling T4 agent
  owns them concurrently. `src/bun/agent.rs` is off-limits.
- Extend the #88 planner via inputs; its invariants (leader-safe, quorum-
  safe, one action) and existing tests must survive unchanged.
- Backup payloads stay self-describing JSON inside the sealed envelope.
- Error messages lowercase, no trailing full stop; thiserror; no unwrap
  in production code.

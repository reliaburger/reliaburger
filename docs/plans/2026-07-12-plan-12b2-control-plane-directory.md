# Phase 12b.2 — Control-plane directory and reporting robustness (T1)

Theme: `docs/progress.md` §12b.2 "Control-plane directory and reporting
robustness". Findings: H1/D1 (P0), CP1, CP5, CP6, CP10.
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Tier acceptance owned by this theme: **an 8+ node cluster reconciles and
reports through leader failover.**

Out of scope: council membership changes (T2 owns `cluster/runtime.rs`
~522–658 and `council/selection.rs` — do not touch), scheduler-side use of
reports (T4, next wave), instance-identity changes (T5).

## Ground truth

- H1/D1/CP1: workers outside the voter set derive no leader API/reporting
  route — placement polling and reporting depend on local Raft metrics
  (`src/cluster/orchestrate.rs:400-418`, `src/cluster/runtime.rs` — the
  leader-target maintainer at ~444–481 reads local metrics, which
  non-voters do not have). An eighth node in a seven-voter council stops
  converging. The whitepaper promises two-layer scale; today the cluster
  is capped at council size.
- CP5: `ReportAggregator` (`src/reporting/aggregator.rs:36,76-103`) keys
  reports by sender wall-clock `timestamp`; stale entries satisfy
  reconstruction coverage and feed scheduling, and a future-dated
  timestamp stays "fresh" forever. Needs a receive-time epoch tied to the
  current leadership term, and eviction of departed/stale nodes (nothing
  evicts today — entries live until process exit).
- CP6: every supervisor entry is reported as running and consumes
  capacity — stopped, failed and completed jobs included. Reports are
  built in `src/reporting/worker.rs::build_report` (~193–230) from agent
  snapshots (`src/bun/agent.rs::persist_instance_record` ~967). Terminal
  workloads must be excluded from running/capacity (they may still appear
  in a separate terminal section if useful for observability).
- CP10: Raft IDs derive from a weak DJB2-style hash of the node id;
  reporting parent assignment (`src/reporting/assignment.rs:21-35`) uses
  `std::collections::hash_map::DefaultHasher`, whose output is not
  guaranteed stable across Rust versions — two binaries on different
  toolchains can disagree about the tree. Reporting/gossip loops can spin
  hot after their watch/transport closes (find the `loop`s that ignore
  `changed()` errors); long-lived spawned tasks die silently.
- Gossip wire: `MembershipUpdate` (`src/mustard/message.rs:126-137`)
  carries `node_id`, `address`, `state`, `incarnation`, `lamport` — no API
  address, no reporting address, no council/leader flags. Gossip datagrams
  are already HMAC-signed (Stage 3b), so fields carried here are
  authenticated to holders of the cluster master key.

## Implementation steps (tests first for each)

### 1. Publish authenticated node endpoints + leader identity via gossip

Extend the gossip wire (`MembershipUpdate` and the membership table) to
carry each node's advertised API address (and reporting address if it
differs) and a leader hint (the sender's known leader node-id + term, or
an explicit council/leader flag set by the runtime — pick the smaller
change and document why). Version-tolerant decoding: an old peer's message
without the new fields must still parse (serde defaults), and the new
fields must not break the existing 10k-member gossip test. Wire the
already-parsed node config values in at join (`protocol.rs:73` currently
inserts empties).

### 2. Non-voters follow leader failover (H1/D1/CP1)

Replace the local-Raft-metrics dependency for **routing** with the gossip
directory: the leader-target maintainer and the placement reconciler's
leader lookup work on every node, voter or not. On leader change, workers
re-point reporting and placement polling without restart. Voters keep
using Raft metrics as the authoritative source *when available*; gossip
is the fallback/propagation path for everyone else. The derived
`gossip-ip:9117` assumption (Phase 14 note) dies here — use the advertised
API address.

### 3. Honest, epoch-scoped aggregation (CP5)

Aggregator entries record receive time (monotonic, aggregator-side) and
the leadership term they were received under; freshness is judged on
receive time, never sender wall-clock. On leadership change, the new
leader's aggregator starts a new epoch — reports from a previous epoch
cannot satisfy coverage/reconstruction (the reconstruction gate in
orchestrate reads through the same accessor). Evict entries for members
gossip reports Dead/Left (subscribe to the membership watch) and entries
stale past a bound. Unit tests: future-dated report does not outlive its
receive-time window; eviction on member death; epoch bump invalidates
coverage.

### 4. Terminal workloads out of running/capacity (CP6)

The snapshot → `build_report` path classifies instances; terminal states
(Stopped/Failed/completed jobs) are excluded from `running_apps` and from
resource-usage sums. Keep the wire shape backward-tolerant. Test: a node
with 2 running + 3 completed jobs reports 2 running and only their
resources.

### 5. Stable hashes, no silent task death, no hot spins (CP10)

- Parent assignment: replace `DefaultHasher` with an explicitly stable
  hash (e.g. FNV-1a or xxhash implemented locally / an in-tree stable
  hash — check what `pickle`/`mustard` already use before adding a dep)
  and pin the assignment with a snapshot-style test so it cannot drift.
- Raft node-id derivation: same treatment (document the migration: ids
  are derived at join; existing durable state keyed by old ids must keep
  working — if migration is genuinely risky, keep the old function for
  existing state and use the stable one for new joins, and say so).
- Sweep reporting/gossip loops for `while let`/`loop` patterns that spin
  or exit silently when a watch/channel/transport closes: exit cleanly on
  closure, log fatally, and supervise — long-lived tasks get a
  supervisor wrapper (respawn with backoff or escalate to shutdown;
  choose per task and document).

### 6. Acceptance: 8+ nodes through leader failover

New env-gated suite (`RELIABURGER_CLUSTER_TESTS=1`, the
`RELIABURGER_UPGRADE_TESTS` precedent — too heavy for 2-core CI): an
in-process 8-node cluster (3 voters + 5 workers, `tests/agent_cluster.rs`
harness pattern) where (a) every worker learns the leader and reports,
(b) an app placed on a worker outside the council converges, (c) the
leader is killed, a new leader elected, and within a bounded time every
worker re-points, reporting coverage recovers in the new epoch, and
placements still reconcile. Plus default-suite unit/integration tests for
each step above.

## Acceptance checklist

- `make ci` green (fmt, clippy `-D warnings`, full default suite; main is
  at 2,175 tests — set the new true count in README.md/docs/README.md).
- Gated 8-node suite passes locally; results quoted in the PR.
- `docs/progress.md`: nested `- [x]` items under the theme, theme box
  checked.
- Book: chapter 2 (`docs/book/02-finding-friends.md`) — the directory
  story (why gossip carries the endpoints and Raft metrics alone cannot
  scale past the council) — and chapter 11 where the reporting/aggregation
  prose now reads wrong. British English, CLAUDE.md style guide, explain
  new Rust syntax on first use.

## Constraints

- **Seam ownership:** you own `cluster/runtime.rs` ~335–481 (reporting
  glue + leader-target maintainer) and may add new functions; do NOT edit
  the council reconciler region (~522–658) or `council/selection.rs` — a
  sibling agent owns them. `cluster/orchestrate.rs`: only the leader-
  lookup/reporting-read seams (~373–484 leader lookup + 154–159 cache
  read glue); do not restructure the scheduler or autoscaler.
- Touch `src/bun/agent.rs` only in the snapshot-building path (CP6).
- Gossip wire changes must be serde-default tolerant both directions.
- Error messages lowercase, no trailing full stop; thiserror; no unwrap
  in production code.

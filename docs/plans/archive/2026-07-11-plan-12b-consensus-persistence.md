# Phase 12b.1 — Consensus persistence safety (remainder)

Theme: `docs/progress.md` §12b.1 "Consensus persistence safety".
Finding: CP3 (P0) — the undecodable-snapshot half is already fixed (a
present-but-undecodable snapshot is a hard startup error). This plan lands the
rest: snapshot versioning + checksums, snapshot/log boundary validation, and
propagating (not swallowing) vote/log/initialisation and freshness errors.
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Out of scope: council membership self-healing (H2/D2, 12b.2), disaster
recovery/backup (D21/CP12, 12b.2), connection deadlines/frame bounds (CP11,
12b.3).

## Ground truth

- `src/council/state_machine.rs`: `persist_snapshot` (line ~54) writes raw
  snapshot bytes into redb with no version tag and no checksum; a torn or
  bit-rotted (but still JSON-decodable) snapshot loads silently wrong, and a
  future format change has no way to detect an old snapshot. `with_store`
  (~282) and `get_current_snapshot`/`install_snapshot` are the load/store
  seams.
- `src/council/durable_log.rs`: `is_fresh` (~81) returns `bool` — check how it
  treats read errors; if a redb read error comes back "fresh", a corrupt store
  triggers re-bootstrap and the exact split-brain C3 fixed. The log store
  purges compacted entries (`purge`, ~307), so after compaction the snapshot
  is the *only* copy of the prefix — nothing validates on startup that the
  snapshot actually covers the purged range (a snapshot older than
  `last_purged` means an unreconstructable gap).
- Bootstrap/freshness call sites (`src/bin/bun.rs`, `src/cluster/runtime.rs`,
  `src/council/node.rs`): audit for `.ok()`, `.unwrap_or_default()`,
  `if let Ok(..)` patterns that turn storage errors into "empty state" or
  "fresh store" on the vote/log/snapshot/init paths.

## Implementation steps (tests first for each)

### 1. Versioned, checksummed snapshot envelope

Wrap persisted snapshots in an explicit envelope: `{ version: u32, checksum,
payload }` (a small header struct serialised with the payload, or separate
redb meta keys — pick whichever keeps `persist_snapshot`/load symmetric and
atomic within one redb write txn). Checksum the payload (crc32fast is already
in the dependency tree via other crates — check; otherwise a simple sha256 via
the existing sha2 dependency is fine). On load:

- checksum mismatch → hard error naming the path and expected/actual sums;
- unknown version → hard error naming both versions;
- **legacy compatibility**: a snapshot written before this change (no
  envelope) must still load — detect it, log a warning, and rewrite it in the
  new format on the next snapshot persist. A fixture test pins this so
  existing dev clusters and the Lima rigs survive the upgrade.

Tests: round-trip; flipped byte in payload → error (not empty state); flipped
byte in stored checksum → error; unknown version → error; legacy blob loads.

### 2. Snapshot/log boundary validation

At startup (the `with_store` / log-store open seam, before Raft starts):
compare the durable log's `last_purged` with the loaded snapshot's
`last_applied`. If entries were purged beyond what the snapshot covers
(`last_purged.index > snapshot.last_applied.index`), state cannot be
reconstructed — refuse startup with an error that says exactly that (which
snapshot index, which purged index) rather than silently starting with a gap.
Also cover the "log purged, snapshot entirely absent" case — that must be an
error too, not a fresh bootstrap.

Tests (unit-level against redb temp dirs, mirroring the existing
`durable_log_round_trips_across_reopen` style): compact → delete/corrupt the
snapshot → reopen returns an error; compact → valid snapshot → reopen fine;
no compaction → no snapshot → fresh bootstrap still works.

### 3. Propagate vote/log/init/freshness errors

- Change `is_fresh` to return `Result<bool, …>` (or equivalent) and make every
  caller treat a storage error as fatal-at-startup, never as "fresh".
- Sweep the council/bootstrap startup path for swallowed storage errors on
  vote read, log state, snapshot read and SecurityState seeding; convert to
  propagated errors with context. Runtime (post-startup) paths keep their
  current error handling — this plan is about *startup* refusing to lie.
- The acceptance shape from progress.md: **compact → corrupt → restart returns
  an error instead of an empty cluster state** — write this as the theme's
  binary-driven test (drive `DurableLogStore` + state machine through openraft
  the way `tests/agent_cluster.rs` / existing council tests do; a full Lima
  cluster is not required if the store-level test drives the real startup
  code path in `cluster::runtime`).

## Acceptance

- All new tests + full `make ci` green (fmt, clippy `-D warnings`, tests).
- `docs/progress.md`: add nested `- [x]` lines under the theme in the existing
  style and check the theme box (this plan completes the theme).
- Book: chapter 2 (`docs/book/02-finding-friends.md`) grew a durable-Raft
  section with C3; extend it with why "can't decode" must differ from "isn't
  there", the envelope/versioning pattern, and the purge-boundary invariant
  (compaction makes the snapshot load-bearing). British English, style guide
  in CLAUDE.md, explain new Rust syntax on first use.

## Constraints

- Stay inside `src/council/` plus the startup call sites in
  `src/cluster/runtime.rs` / `src/bin/bun.rs`; do not touch `src/bun/agent.rs`
  (other in-flight themes edit it).
- Keep the snapshot payload self-describing JSON (bincode was already rejected
  for `deserialize_any` reasons — see the Stage 4 L1 note in progress.md).
- No new dependencies unless nothing in-tree can checksum (sha2 is in-tree).
- Error messages lowercase, no trailing full stop; thiserror in library code.

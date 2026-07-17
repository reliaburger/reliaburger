# Phase 12b.6 — Self-upgrade convergence and adoption (Theme SU)

Theme: `docs/progress.md` §12b.6. Findings: UPG1, UPG2, D20.
Source: `docs/plans/2026-07-10-review-past-phase-12.md`,
`...2026-07-09-review-design-discrepancies.md`.

Harness contract (#106): green = `make ci`; deterministic, observable sync,
no sleeps; env/cluster tests `#[ignore]` + named (`make test-cluster`/
`test-upgrade`); coverage floor 78.65 — cover new code; **do NOT run `make
coverage` without 40+ GiB free**; no headline count.

## Ground truth (verified 15 Jul)

- **UPG1 cordon: DONE** — `apply_upgrade_cordon` (`meat/filter.rs:36`) IS
  called from `cluster/orchestrate.rs:171` in the leader scheduling loop
  (the review's "no caller" was stale; `ClusterStateCache` exists). VERIFY
  and drop from scope.
- **UPG1 quorum (real):** `quorum_headroom_ok` (`src/upgrade/orchestrator.
  rs:519`) counts CONFIGURED voters (`metrics.membership_config...voter_
  ids().count()`), not LIVE ones — an upgrade can remove the vote keeping
  quorum. Live health in Raft metrics / gossip (`mustard/state.rs`,
  `council/node.rs:885`).
- **UPG2 client-trusted roles/addresses:** the upgrade handler
  (`src/bun/api.rs:580-687`, `StartUpgradeNode`) copies client
  `node_id`/`address`/`role` into `NodeUpgradeRecord` with no server-side
  derivation (only a non-empty check). Source of truth: gossip membership
  (`mustard/membership`), Raft identities (`council/identity`).
- **UPG2 gossip-rejoin:** post-upgrade verify (`orchestrator.rs:427` version
  poll) checks HTTP health + version, not gossip rejoin.
- **UPG2 Apple adoption:** `grill/apple.rs:107` declines `adopt()` (trait
  default `Ok(false)`, `grill/mod.rs:260`) → Apple workloads restart across
  a swap. runc adoption (`runc.rs:530` `runc state`) + process adoption
  (`process.rs:397` pidfile) work, for contrast. Rootless is a spec-modifier
  (n/a).
- **D20 prose:** `docs/design/agent-bun.md §5.5` has an honest caveat but
  the old superseded reporting-tree/reverse-order sequence remains
  (~994-1092); `docs/progress.md:409-424` too.

## Implementation steps (tests first, each asserting the refusal/proof)

### 1. UPG1 — live-voter quorum headroom
`quorum_headroom_ok` must count LIVE, reachable voters (cross-reference the
voter set against gossip Alive / Raft last-contact health), so an upgrade
that would drop the vote keeping quorum is refused. Test: with a
configured 3-voter set where one voter is already dead, upgrading another
voter is refused (would break quorum); with all live, it proceeds.

### 2. UPG2 — server-derived roles/addresses
Derive each node's role + address server-side from gossip/Raft; ignore or
validate-against client-supplied values (reject a mismatch). Test: an
upgrade request with a spoofed role/address for a node is rejected or
corrected to the authoritative value — a client can't upgrade a node under
a false identity.

### 3. UPG2 — prove gossip rejoin post-upgrade
Post-upgrade verification asserts the node rejoined the gossip mesh (Alive
in membership), not only HTTP health + version. Test (gated cluster/
upgrade): a node that upgrades and rejoins passes; one that comes back
HTTP-healthy but not in gossip fails verification.

### 4. UPG2 — Apple adoption (implement or honestly decline)
Implement Apple Container adoption via `container inspect` (rebuild runtime
state from the running container) so an Apple workload survives an
exec-in-place swap — OR, if Apple has no recoverable handle, keep the
decline but make it EXPLICIT + documented (Apple workloads restart across
upgrade; not a silent gap). Prefer implementing if `container inspect`
gives a stable pid/state; else document. Test (macOS/Apple-gated
`#[ignore]` `make test-apple`, or a unit test of the inspect-parse): an
adopted Apple instance is re-tracked, or the decline is explicit.

### 5. D20 — reconcile the prose
Replace the superseded sequence in `docs/design/agent-bun.md §5.5`
(~994-1092) with the actual implementation (authenticated HTTP `/v1/upgrade/
apply`, `GET /v1/version` polling, leader-last in-place, leader-last
rollback); reconcile `docs/progress.md:409-424`. (The broader doc pass is
Wave B; here fix only §5.5 / the upgrade progress lines your code touches.)

## Acceptance
- `make ci` green. Cluster/upgrade acceptance → `#[ignore]` +
  `make test-cluster`/`test-upgrade`; Apple → `make test-apple`. Quote
  results or state unverified (real multi-host / Apple runs may need the
  user's environment — flag them).
- `docs/progress.md`: nested `- [x]` under "Self-upgrade convergence and
  adoption"; check the theme box. Book: `docs/design/agent-bun.md §5.5` +
  chapter 14 "Changing the Tyres". No headline count.

## Constraints (seam ownership — sibling PW + SM run concurrently)
- YOURS: `src/upgrade/*`, the upgrade handlers in `src/bun/api.rs`
  (~580-687), `src/grill/apple.rs` (adoption — you own it, not PW),
  `src/cluster/orchestrate.rs` (live-voter read; do not rework the
  scheduler), read-only use of `mustard`/`council`, and `docs/design/
  agent-bun.md §5.5`. If you must touch `src/bun/agent.rs` for the adoption
  caller, keep it to a SMALL isolated region and note it (PW owns
  ~1195-1300, SM owns ~2777-3137 — stay clear of both).
- NOT YOURS: `src/bun/supervisor.rs`, `src/grill/{process_workload,rootless}
  .rs`, `src/bun/gpu.rs` (PW); `src/smoker/*`, the fault region of agent.rs
  (SM).
- Council/Raft changes additive (serde-default; pre-theme snapshot loads
  cleanly — fixture test).
- thiserror; no unwrap in production; lowercase errors, no trailing full
  stop; British English.

# Confirm cluster lease retirement before forgetting ownership

Status: proposed; implementation is incomplete and must not be released.

The regression in `src/council/state_machine.rs` demonstrates the defect: an
application moves from one worker to another, cleanup deletes desired state,
and `TestLeaseFinishCleanup` succeeds without either worker confirming runtime
retirement. A missing worker can still be running the application.

## Proposed protocol

1. Each application lease stores the union of every `(application, node)` pair
   ever committed by scheduling. Raft records ownership in the same entry that
   publishes placement. History is deduplicated and bounded at 65,536 pairs per
   lease; exceeding the bound refuses scheduling rather than losing ownership.
2. Beginning cleanup durably fences further attachments and scheduling for the
   lease. Deleting desired state leaves all possible runtime owners recorded.
3. Placement reads require a current quorum-confirmed leader. They include
   retirement instructions containing the immutable lease ID, app and node.
   Instructions include former owners even when the current placement is empty.
4. Each node runs the existing bounded `AgentCommand::Retire` operation, even if
   its recovered placement journal contains no matching entry. Busy workers,
   runtime errors and uncertain outcomes retain the instruction for retry.
5. Only after confirmed runtime retirement and local checkpoint persistence
   does the reconciler acknowledge the exact tuple through an internal endpoint.
   The endpoint requires the existing system service identity. Ordinary users,
   including administrators, cannot attest runtime absence through this route.
6. Raft accepts acknowledgements only for cleaning leases with no desired app.
   Repeats are harmless. An acknowledgement for an old lease ID cannot remove
   ownership from a replacement lease, even if it reuses the namespace.
7. A lease cannot finish while any possible runtime owner remains. An offline
   owner keeps cleanup pending until it returns and confirms retirement.
   DELETE returns 202 while pending, 204 only when finished. Relish polls with
   a deadline and reports unknown on timeout. Lease GET requests use the leader,
   preserving the caller's credentials, so stale follower absence is not proof.

## Compatibility and operational impact

The proposal changes protocol generation 6 to 7, durable state generation 7 to
8, and lease schema 3 to 4. It requires fresh development clusters. Existing
incompatible state is refused without migration or deletion. This follows the
user-approved pre-release policy; it makes no compatibility promise between
these development snapshots. Compatible binaries retain the existing explicit
format-equality upgrade policy after release.

No production deployment, merge, tag or release is part of this change. The
uncommitted implementation and temporary-state tests are confined to PR #167's
branch. The release candidate must use the final format generations, and both
actual-binary compatibility and upgrade qualification must run again.

## Required verification before commit

- The original lost-owner regression must fail before implementation.
- Rescheduling retains both former and current owners; cleanup rejects stale
  scheduling and requires all owners, including across snapshot restoration.
- Duplicate acknowledgements are idempotent; active/unknown/replacement lease
  acknowledgements cannot clear ownership.
- An empty local journal still triggers runtime retirement; a failed or timed
  out retirement cannot send an acknowledgement; later retry can finish.
- Real authenticated API tests reject anonymous and ordinary admin assertions.
- A real three-node case holds a worker out of reconciliation, observes pending
  ownership, changes leader, then restores reconciliation and confirms cleanup.
- Complete council, cluster, lease, API and library suites; strict Clippy and
  formatting on macOS/Linux; actual compatibility and rolling upgrade checks.

Current evidence: the lost-owner regression fails before implementation. The
initial core changes pass 207 council, five compatibility and 15 lease tests on
macOS. Runtime/API wiring and the full verification matrix remain incomplete.

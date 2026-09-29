# Confirm cluster lease retirement before forgetting ownership

Status: confirmed cleanup and operator decommissioning are implemented and
qualified in separate commits, under the user-approved permanent retirement
policy. Returning machines require fresh enrolment under a new node identity.

The regression in `src/council/state_machine.rs` demonstrates the defect: an
application moves from one worker to another, cleanup deletes desired state,
and `TestLeaseFinishCleanup` succeeds without either worker confirming runtime
retirement. A missing worker can still be running the application.

## Approved confirmed-cleanup protocol

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

## Operator decommissioning extension

The user also approved retiring a node's identity and requiring fresh enrolment
before it returns. A maintenance cordon alone cannot prove that workloads have
stopped. The decommission operation therefore requires an unscoped operator
administrator to explicitly attest that the node's workloads have been stopped
or fenced outside the cluster.

The separate operator feature provides:

- Commit a durable retired-node record with the node identity, operator and
  reason. In the same state transition, resolve every cluster lease placement
  awaiting that node and fence new scheduling to it. Resolve a node-chaos
  obligation owned by that same fenced identity, retain the allocation counter
  and record the released sequence in the operator audit. A fault on another
  node still blocks the membership-affecting operation.
- Reject attempts by the retired identity to renew or resume membership/work.
  Rejoining requires fresh state, credentials and a new node identity. A stale
  acknowledgement cannot undo retirement or affect a replacement node.
- Report operator decommissioning separately from runtime-confirmed retirement.
  The control plane cannot remotely prove that a disconnected machine stopped;
  the operator's explicit attestation is the authority for this path.
- Expose the operation through authenticated API and CLI, with an explicit
  acknowledgement and a reason. Repeated requests must be safe; leader changes
  and snapshot restoration must retain the retired identity and cleared duties.
- Test multiple leases, unavailable nodes, auth/scope refusal, stale scheduling,
  certificate/identity reuse and fresh replacement enrolment. Update the final
  format generations and runbook with the implemented contract.

## Compatibility and operational impact

Confirmed retirement changes protocol generation 6 to 7, durable state generation
7 to 8, and lease schema 3 to 4. The subsequent operator retirement feature adds
permanent identity records and fenced serial allocation, advancing protocol/state
to 8/9 while retaining lease schema 4. It requires fresh development clusters. Existing
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

Current evidence: the lost-owner, missing-journal and HTTP response regressions
fail before implementation. Runtime and authenticated API wiring now pass,
including uncertain responses, persistence failure and isolated-leader reads.
The actual three-node paused-worker/leader-change case passes on macOS (19.50s).
Full library suites pass 3,343 macOS and 3,397 Linux tests; strict Clippy and
both actual-binary compatibility cases pass on both platforms. The Linux
three-node case passes in 18.79s. All three actual rolling upgrade/pause-revert/
cluster rollback tests pass in 177.58s.

Operator retirement adds failing-first admission, TLS, renamed-identity and
node-fault regressions. It covers snapshot restoration, duplicate decisions,
stale membership and quorum refusal, enrolment/renewal fencing and existing API
connections. Full library suites pass 3,351 macOS/3,405 Linux tests (five/19
explicit gates), with strict all-target/all-feature Clippy, both binary suites,
actual compatibility checks and 13 renewal tests per platform. All four real
cluster-failover cases pass on macOS/Linux (42.82s/43.27s), including retirement
through a follower, cleanup completion, old-identity admission refusal, fresh
replacement enrolment and a subsequent leader change. All three Linux rolling
upgrade/revert/rollback cases pass in 176.56s.

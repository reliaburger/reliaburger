# Registry ownership and retirement for 0.1.0

C34 registry leases and C30's runnable pushed-image test remain release gates.
Implement each independently reviewable repair in its own commit.

1. Make local catalogue mutation durable before acknowledging a push. Propagate
   file/directory sync failures, serialise persistence against other writers and
   garbage collection, and retain transaction ownership across cancellation.
   Recheck required blobs under that same guard so GC cannot remove a layer
   between validation and publication. GC must persist its decision before
   deleting bytes and keep the guard until deletion finishes. Test failed
   persistence, concurrent pushes, cancelled callers and push/GC interleavings.
2. Add explicit repository ownership under a server-issued application lease.
   Register every node that may accept uploads before it writes bytes. Bind
   upload sessions to that repository and principal, and fence admission and
   manifest commits after cleanup starts. Conditional Raft commits must check
   lease authority at application time, not only in an earlier HTTP check.
3. Supply authenticated leader forwarding for authoritative registry mutations.
   Followers must never report a local-only proposal as a durable cluster push.
   Keep single-node durability explicit and retain uncertainty after a timeout.
4. Retire repository metadata and outstanding uploads only after leased
   workloads stop. Each writer node confirms its own cleanup; retain the lease
   until all confirmations arrive or the operator decommissions an identity
   under the already-approved isolation contract. Preserve shared image blobs
   and ordinary repositories; reclaim unreferenced bytes through normal GC.
5. Make GC retirement idempotent after a holder was removed but physical deletion
   failed or Bun died. A retry must not mistake another node's sole advertised
   copy for a reason to retain this node's already-approved extra copy forever.
6. Make catalogue fixtures use their lease namespace for repository names. Push
   a real runnable image, deploy the exact digest through Pickle and verify the
   actual response. Qualify live three-node/client-death/leader-failure cleanup,
   snapshots, concurrent commit/refusal and preserved ordinary/shared content.

Update chapters 5 and 15, the installation/status documentation and progress
ledger with evidence. Any new replicated or durable schema requires the matching
explicit compatibility generation; development clusters remain fresh-only.

## Catalogue transaction prerequisite completed

The manifest-persistence regression fails against the previous implementation:
an unwritable catalogue still returned HTTP 201. Publication now follows private
atomic persistence under one owned write guard. GC holds that same guard through
its persisted decision and physical deletion, and pushes recheck dependencies
under it. Blocking transactions retain ownership when their caller disappears;
a dedicated physical cancellation test remains part of sustained qualification.

Five regressions cover failed push persistence, failed GC persistence and retry,
both push/GC orderings, and simultaneous acknowledged pushes surviving reload.
All 240 Pickle tests and the 17 cluster/five integrity/two upload integration
tests pass on Linux; the native full library suite passes 3,384 tests and native
CI-profile registry cases pass. Strict all-target/all-feature Clippy passes on
both platforms. Full native CI still fails the separately tracked job launch
recovery window; these registry checks do not close C34 or V02. No schema changed.

## GC retry repair completed

Step 5 is implemented. All three regressions fail before the repair: a repeated
approval is refused after reload, nomination protects the wrong node's extra
copy, and an actual failed deletion cannot finish after repair. They now pass
with all 243 Pickle and 82 Raft state-machine tests on macOS/Linux, plus strict
Clippy on both. Reapproval still refuses a newly referenced blob and deletion
of the final advertised copy. No schema changed.

## Unconfirmed cluster acceptance repair completed

The real Raft regression reproduces HTTP 202 before the fix. Unknown/failed
cluster writes now return 503. After electing the node, retry returns 201 and
publishes the tag in the authoritative catalogue. Successful state-machine
writes return Applied with a log index; treating only generic Ok as success was
also incorrect and is covered by this regression. All 244 Pickle tests and
strict all-target/all-feature Clippy pass on macOS/Linux. Local copies remain
available for retry after refusal. Authenticated leader forwarding, including
workers without a local council handle, remains step 3; it is not implemented
by this status-code repair.

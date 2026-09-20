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
   Restrict leased image references to workloads belonging to the same lease;
   ordinary workloads must not acquire an undeclared dependency that cleanup
   would later remove. Apply this check to declarative apps and node-local jobs.
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

## Authoritative forwarding completed

Every clustered Bun now has a registry forwarder independently of whether it
has a local council handle. It attempts the local council, then resolves the
advertised leader endpoint when forwarding is required. The dedicated client
uses live node credentials and refuses HTTP redirects. Local-only mode remains
explicit; loss of the cluster leader cannot silently turn a worker into a
standalone registry.

The restricted `/v1/registry/propose` endpoint accepts only manifest and GC
operations with explicit compatibility. It requires the service principal and
an actual TLS peer leaf. A fresh quorum-backed security view validates the
certificate and retirement state, and the proposed holder IDs must match that
node. A follower refuses; it cannot replace the sender's identity by forwarding
again. Raft also fences retired writers at application time, closing the race
between TLS authorisation and committed node retirement. Requests and responses
are limited to 8 MiB; deadlines include request-body extraction, consensus,
networking and response streaming.

The route absence, commit-after-retirement and fresh-worker stale-term
regressions fail before their fixes. All four real TLS integration cases pass on
macOS and Linux (10.02s each), covering worker/follower manifest pushes, current
credentials, foreign/revoked leaves, forged holders, a three-node Raft election,
an isolated old leader and lost quorum. Oversized and stalled request/response
bodies refuse within their limits. The election fixture discovers and
quorum-confirms the actual leader before injecting faults; it does not assume
its bootstrap node still leads after TLS setup.

All 248 Pickle, 83 Raft state-machine and eight authorisation-audit tests pass,
as do the 13 renewal, 17 registry cluster, five integrity and two upload cases.
Strict all-target/all-feature Clippy passes on macOS/Linux. Protocol 8/state 12
and lease schema 4 remain unchanged. Repository ownership/retirement and the
complete live catalogue remain open.

## Exact upload creator prerequisite

Upload authentication now preserves the exact credential identity. Session
admission, PATCH and completion bind to that identity as well as the repository.
Role checks alone previously admitted another deploy token's PATCH (202); the
new HTTP regression reproduces that failure before the fix. Reusing a token
name or presenting the internal service credential cannot take over a user's
session. Revoked owners refuse on reauthentication, and refused writers leave
the temporary bytes unchanged. Sessions remain ephemeral, with restart/TTL
cleanup; no durable or replicated schema changes for this prerequisite.

All 249 Pickle tests, 17 cluster/five integrity/four authority/two upload
integration cases and strict all-target/all-feature Clippy pass on macOS/Linux.
Repository lease admission and durable writer receipts remain step 2; exact
upload identity alone does not implement them.

## Repository retirement implementation order

The replicated lease will retain repository names and each storage node that
may have accepted a writer. A repository belongs under its server-selected
`rbtest-.../` namespace. Admission records the node before any upload bytes,
checks the exact owner, and refuses expired or Cleaning leases. A manifest
commit repeats the active-lease check in Raft, so earlier HTTP admission cannot
outlive cleanup. Ordinary writes cannot bypass the reserved namespace.

Cleanup first deletes desired workloads and waits for every retained placement
to confirm retirement. Only then may registry workers fence repository writes,
wait for in-flight transactions, cancel partial uploads, durably remove all
local repository metadata and acknowledge their exact obligation. Global
metadata removal and lease deletion wait for all storage acknowledgements.
Operator decommission clears only the retired identity's obligations and
records the counts in its immutable audit. Shared digests remain protected by
ordinary repository references. Unreferenced metadata locations must not leave
orphaned test bytes permanently protected as a last copy.

The same lifecycle must apply to standalone leases. A local reaper needs a
registry cleanup handle and durable completion ordering, rather than assuming
that agent cleanup also removes image metadata. Pull-through/P2P writers and
ordinary job/app image references need the same reserved-namespace checks;
adding a header only to the catalogue fixture would leave bypasses.

Validate the state transitions and snapshots first, then HTTP writer admission
and physical cleanup. Each meaningful repair remains its own commit. The
replicated lease format change advances explicit compatibility before it is
used; existing development state still requires a fresh cluster.

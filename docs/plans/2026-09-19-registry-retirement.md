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

## Durable registry receipt contract

The lease model now includes bounded repository-to-writer receipts and a
workload-retirement barrier. Raft atomically attaches writers, conditionally
publishes leased manifests, records the barrier only after desired resources
and placements disappear, and accepts exact repository/node acknowledgements.
Finish retains the lease until every writer resolves; only then does it remove
global repository metadata. Node decommission clears and audits that node's
registry receipts. Standalone records preserve the same obligations through
reload and refuse early finish.

The ordinary-commit bypass regression fails before implementation. Additional
coverage exercises snapshot restoration, identity retirement, stale commit
refusal, duplicate acknowledgement, wrong owner/namespace/expiry, shared resource
limits and shared content preservation. A digest-addressed reference also
caught a colon-parsing error in retirement before this feature was committed.
Full library checkpoints pass 3,401 macOS/3,455 Linux tests (41.37s/88.76s), with
five/19 explicit gates. Final 250 Pickle, 87 state-machine and 17 lease tests,
eight API-token/two compatibility/four authority integrations and strict Clippy
pass on both platforms.

This changes the durable and replicated contract to protocol 9/state 13 and
lease schema 5. HTTP admission, local transaction fencing, replicated image
writers, image-reference ownership checks and actual node cleanup remain open.
No public OCI request may claim this work is complete until those paths and the
physical cleanup cases pass.

## Peer repository identity prerequisite

Peer transfers no longer flatten nested repository names. The old workaround
predated nested OCI routing and would bypass a reserved lease namespace on the
receiving node. A real HTTP regression fails with an upload 404 before the fix,
then verifies upload, HEAD inventory and digest-checked download at the exact
nested path. All 17 replication, nine pull and 17 cluster integration cases
pass on macOS/Linux, with strict Clippy. No format changes. Receiver-side lease
admission and confirmation remain part of the integration work above.

## Current ownership queries

Registry workers can query the current leader for an active repository owner or
for their own ready-to-retire repositories. The service bearer and actual TLS
node certificate are both required; compatibility, revocation and node identity
are checked against a quorum-backed security view. Cleaning alone does not
allow metadata removal: the workload-retirement barrier must also be committed.
The inventory never returns another node's obligations or lease credentials.

Five integration cases pass on macOS/Linux (20.03s), covering TLS forwarding,
leader replacement, refusal by an isolated old leader, lost quorum, exact-node
inventory, revoked/foreign identities and oversized/stalled request bodies.
Eight route-authorisation audits and strict Clippy pass on both platforms.
This supplies discovery for the cleanup worker; HTTP lease admission and actual
storage retirement remain open. No durable format changes.

## Exact local repository generation

The local and replicated catalogue now retain repository-to-lease-generation
ownership independently of manifest rows. Claims and retirement refuse a different
generation or unowned existing metadata. Retiring empty, already-cleaned storage
is idempotent, while a later owner cannot be erased by an old cleanup retry.
Raft validates every repository generation before final lease removal. State
advances to 14 (protocol 9, lease schema 5); development clusters remain fresh-only.

The reload regression fails before the fix. All 44 catalogue-type and 87 Raft
state-machine tests, two actual-binary compatibility tests and strict Clippy pass
on macOS/Linux. The generic snapshot-copy fixture now creates an ordinary
repository and asserts that both copies existed before retirement; its old
reserved name had silently refused the setup. HTTP admission must next persist
these generation records before upload creation.

## HTTP admission and storage-worker integration

Direct OCI writes now require the exact authenticated lease owner and matching
namespace, recording the writer receipt and local generation before temporary
files. Internal blob replication may attach only to an already owned active
repository; it cannot publish leased manifests. Every subsequent request checks
admission again, and final publication is conditional on an active lease.
Standalone publication holds the lease operation guard through persistence.

The node's supervised registry reaper obtains current receipts, waits for local
writers, removes partial uploads, persists repository metadata removal and then
acknowledges the exact receipt. Failed cleanup retains ownership. Each repository
has a deadline, so an unavailable writer does not starve later obligations.
Owned upload creation covers cancellation before session registration; blocking
blob/catalogue commits retain their repository guards after caller cancellation.

The unleased-upload regression fails before implementation (202 instead of 403).
Coverage includes exact owner/namespace refusal, cancelled creation, late commits,
workload-retirement ordering, failed file removal, failed metadata persistence,
shared ordinary content and actual TLS claim/publication/confirmation through the
leader. The TLS fixture exposed a definitive lease refusal being flattened to
503; the internal endpoint now returns a structured refusal and the worker
preserves 403. The generic repository-copy fixture uses an ordinary namespace,
while the new lease tests exercise the reserved namespace explicitly.

Protocol advances to 10; state remains 14 and lease schema 5. P2P local writers,
conditional healer publication, ordinary app/job image-reference admission and
the physical multi-node qualification remain open. C34 is not complete.

Qualification passes 3,411 native library tests (five explicit gates, 43.38s),
followed by all 261 final Pickle tests and strict Clippy. The full final Linux
library run passes 3,466 tests with 19 explicit gates. Both platforms pass the
17 lease tests, 17 cluster/five integrity/five TLS authority/two upload/two binary
compatibility integrations and strict all-target/all-feature Clippy. These do
not close the separately listed P2P, workload-reference or crash qualifications.

## Workload image-reference ownership completed

Ordinary workloads and different leases cannot depend on disposable repositories.
HTTP preflight checks every main, init and job image before any part of a mixed
manifest applies. Raft repeats the application check with its committed observation
time. Only the same active application lease may use its registered repositories;
ordinary images remain available to all otherwise-authorised workloads.

The HTTP-200 and unleased-Raft-write regressions fail before the fix. All three
ownership cases, 124 Bun API/89 state-machine/17 lease tests and strict Clippy pass
on macOS/Linux. Formats remain protocol 10/state 14, lease schema 5. P2P/healer
ownership and physical multi-node registry qualification remain open.

The next transfer step must also replace stale full-holder-set publication.
A healer currently copies location sets from an earlier catalogue snapshot;
writing those sets after GC or retirement can resurrect obsolete holders.
Storage nodes should confirm their own digest-verified copy while excluding local
GC, then add only themselves through a conditional authoritative operation. A
replicator's earlier HEAD response alone is insufficient evidence across an
already-approved deletion. Keep direct peer pulls owned through cancellation and
retain failed temporary-file deletion in the upload-session tracker. Supply a
bounded authoritative per-repository catalogue read for workers without a local
Raft catalogue. Qualify cached-copy and concurrent GC/retirement cases before
closing this path.

Manifest reads now enforce repository retirement for digest URLs as well as tags.
The failing-first shared-content regression passes on macOS and Linux alongside
all 261 Pickle tests, 24 registry integrations and strict Clippy. Resolving fresh
workers' catalogue reads through current authority remains a separate open item.

The catalogue-read fix now routes repository metadata and quota usage through the
current authenticated leader. Fresh workers and followers no longer turn a
committed image into a cache miss; missing quorum refuses. Responses are scoped
to the requested repository and bounded. Protocol advances to 11, state remains
14. The original real-TLS fresh-worker regression fails before this change.

The remaining copy operation also needs a per-node GC generation in durable
state. A copy proposal can time out locally but still commit later, after its
local guard has gone and collection has deleted the bytes. A node must query the
repository and its GC generation coherently, verify bytes while excluding local
collection, then propose only its own holder identity with that generation.
Actual GC approval advances the generation; a delayed older copy must refuse.
Check overflow before any mutation and qualify deferred copy versus GC, lease
retirement and node decommission. This is planned work, not implemented proof.
The public Bun image list also still needs current catalogue authority.

The public image-list fix is complete: a committed image is visible without a
local catalogue projection, workers/followers use current authority, missing
routes refuse, and user authentication remains enforced. The initial empty-list
regression fails first. All 263 Pickle/125 Bun API tests, five real TLS authority
cases, two compatibility checks and strict Clippy pass on macOS/Linux. Current
formats are protocol 12/state 14, lease schema 5.

Runtime P2P and healer downloads now use owned, repository-admitted upload tasks.
Caller cancellation cannot strand an untracked temporary file; failed deletion
remains fenced for retry. Already-admitted image pulls share their read guard
with children so queued cleanup cannot force recursive lock acquisition. Cached
blobs still require ownership and digest verification. The unowned-file regression
fails first; all 269 Pickle tests, 29 registry integration tests and strict Clippy
pass on macOS/Linux. Formats remain protocol 12/state 14, lease schema 5.

Conditional publication remains open. When implementing it, hold the local
catalogue/GC guard before requesting GC approval, through physical deletion, in
an owned transaction. A deletion approved before acquiring that guard must not
wait behind and then invalidate a newer copy proof. Also fence delayed manifest
publication, not only the new copy operation: its proposal currently follows the
local persistence task. Query the publishing node's GC generation while excluding
local collection, and validate that generation when Raft applies the publication.

## Publication and collection generation fence

GC now acquires the local catalogue guard before requesting approval and keeps
it through physical deletion in an owned task. Manifest publication similarly
retains ownership through its authoritative response. Dropping a caller cannot
release either operation early. A replicated per-node counter advances only for
non-empty approved deletions; exhaustion refuses without changing metadata.
Every manifest proposal carries the generation observed under that guard. A
delayed proposal across collection returns a typed retryable refusal, even after
a snapshot restores the leader. A proposal timeout never authorises deletion.

The delayed-publication and counter-exhaustion regressions fail first. The full
library checkpoints pass 3,428 macOS and 3,482 Linux tests, followed by all 31
registry/compatibility integration cases, a further publication-cancellation
regression and strict all-target/all-feature Clippy on both platforms. Tests
retain GC/publication ownership after aborting their callers and exercise real
TLS stale refusal followed by ordinary and leased publication at the new
generation. Protocol 13/state 15, lease schema 5. Receiving-node copy confirmation
and physical node-death qualification remain open.

## Storage-node copy confirmation completed

The failing-first whole-holder-list regression proves that the old Raft operation
accepted claims without storage evidence. That operation now always refuses.
Healers and direct peer consumers ask each receiving node to confirm its exact
repository/digest after verifying all local files under owned catalogue and
repository guards. Receivers propose only their own TLS-bound identity. Raft
unions it with current holdings and rechecks GC generation, active exact lease,
writer receipt, existing metadata and identity retirement. Tags remain unchanged.

The internal endpoint requires the service credential, with only the explicit
ordinary standalone bootstrap exception. Receipts have a bounded body/deadline
and must identify the requested receiver and content. Cached HEAD responses
cannot replace confirmation. Clustered receivers do not replay authoritative
remote tags into their local projections.

All 275 Pickle, 94 state-machine and 17 lease tests pass on macOS/Linux, alongside
31 registry/compatibility integrations and strict all-target/all-feature Clippy.
Coverage includes corrupt/missing blobs, cancellation, durable standalone writes,
wrong/stalled/oversized receipts, cleanup/expiry/decommission fences and actual
TLS receiving-node publication. Protocol 14/state 16, lease schema 5. The older
standalone healer fixtures now explicitly seed metadata and check each authority's
own proof; the TLS cluster fixture proves their union at a common leader.
Physical registry cleanup qualification and the runnable-image catalogue remain
open.

## Await asynchronous storage acknowledgements

The C30 physical catalogue observed successful registry assertions followed by
HTTP 500 during cleanup: the coordinator tried to finish the lease before its
registry writers acknowledged retirement. Raft correctly refused. The helper now
returns the existing CleanupPending result after recording workload retirement
and before attempting final deletion. Admission is already fenced, so receipt
sets can only shrink. Transport/storage errors retain their original handling.

A real Raft two-writer regression fails before the fix, then proves zero/one
acknowledgements retain Pending and the second permits completion. Six authority
integrations, 17 lease tests and strict all-target/all-feature Clippy pass on
macOS/Linux. No format change. The C30 fixture and complete physical registry
qualification remain separate commits.

# Persist distinct batch execution identities

Issue: #535.

First reproduce the pull watcher completing a new batch from an older named
job while the new deploy is unacknowledged. Allocate opaque random runtime names
on the leader, independent of the submitted name's length. Persist them in
batch records, dispatch those names, and match every pull/callback outcome by
canonical instance identity and namespace. Keep submitted names as labels and
return their runtime mapping for observability.

The authenticated internal request carries an exact execution-to-label map.
Every configured execution must have one valid logical name in the same
namespace, and unexpected entries are refused. A typed internal agent command
passes that map to the deploy worker. Public Config has no alias field.

Preflight the entire group in the agent loop and publish one atomic checkpoint
containing all new owned Preparing generations. No worker starts and HTTP must
not return 202 until the whole admission is durable. Carry each exact prepared
generation to its worker. Keep checkpoint IO in spawn_blocking under a bounded
timeout. An error or timeout retains the existing uncertain-store fence and
returns an explicit 503; exact retries preserve the original unknown attempt.
A regression prevents checkpoint publication, then removes the filesystem
obstruction and verifies that retry still refuses without launching either job.

Record each logical label durably before runtime launch. Checkpoint recovery
validates and preserves it. The supervisor continues to key runtimes and
adoption by execution identity. API status presents the logical name while
retaining its existing opaque instance ID. A runner resolves raw log paths
against its local durable labels before checking caller scope, even when it
has no leader-side batch tracker. Forwarded LogRecord rows retain namespace
and instance ID but use the logical label as their app name, so scoped shared
queries work across repeated executions and recovery.

The durable record also carries explicit batch-execution ownership. Identical
internal dispatch retries reuse the exact stored attempt and terminal result,
including after recovery. They never create another generation under the same
execution identity. Refuse a changed spec or logical label for that execution;
don't infer ownership from a naming prefix. Public Config cannot set the marker
or take over an existing batch-owned execution, including with explicit job
rerun acknowledgement after terminal success or checkpoint recovery.
Use real completion callbacks and a launch sentinel to prove that a repeated
service request ran only once, then repeat across checkpoint recovery.

Add raw HTTP tests for invalid internal maps, concurrent identical labels,
stale watcher evidence, runner status/log scope, and public alias injection.
Add checkpoint round-trip and invalid-label tests through serialized records.
Run all regressions against unchanged production before implementing. Update
Chapter 8, then run portable CI and the cluster gate.

Coordinate with #550: resource fingerprints keyed by logical name and namespace
must refuse ambiguous evidence when distinct executions carry different specs.

The record, index and internal report semantics change wire and durable state.
Refresh the actual integrated train before implementation and increment both
relevant format counters once, preserving earlier fields. The parent coordinates
the order so parallel children never claim the same generation.

If a logical label equals another execution's opaque name, refuse a string-only log path with 400. The existing instance selector disambiguates: authorize the selected exact namespace/logical label and retain its opaque runtime selector. Raw and structured-store tests select both executions under each reader scope and prove that only the authorized execution's sentinel returns. Never infer ownership from a prefix.

Retirement moves a positively retired batch execution into a separate compact
RetiredBatchExecution proof inventory in the SAME atomic checkpoint. Active
RecordedJob always has its full spec. The proof binds namespace, execution,
batch id, logical label, normalized spec digest and exact generation, retaining
an observed exit code if one exists. Positive retirement without observed exit
remains Unknown and refuses delayed dispatch. Checkpoint publication snapshots
both inventories; active/proof collisions are invalid, and an uncertain move
keeps the original execution fenced. Log aliases remain available from proofs.
Preflight both inventories plus future active-transition headroom before the
uncertain IO fence. The 16 MiB bound is finite; refuse new admission gracefully
when full and retain exact retries. Never prune replay proof without a durable
fence.

A compact global execution-ownership index lives in the SAME durable Raft state
and outlives terminal tracker pruning. Its namespace/execution key binds batch
id, original logical label and the same normalized specification digest used
by node ownership. Full node specs and outcomes stay in the node ledger.
New batch registration checks the whole index's explicit entry and byte limits
before any registration or dispatch, preserving existing active transitions.
The proposed limits are 131,072 identities and 32 MiB of deterministic encoded
index bytes, independently enforced; both ceilings refuse whole registrations.
Registration cannot take an already committed app identity. Ordinary AppSpec
and leased TestLeaseAppSpec entries atomically refuse indexed identities, so
concurrent apply, GitOps and reconciliation cannot bypass an API precheck.
The real HTTP regression completes a batch, prunes its terminal tracker record,
and refuses public app reuse on another worker with no local job ledger. Repeat
through an actual two-node leadership transfer and durable snapshot recovery.
State-machine controls cover both app/batch commit orders, leased writes,
namespace isolation, another batch borrowing an identity and finite capacity.

Ownership history is never pruned into reuse. Once the finite global or node
proof inventory is full, new admissions require a fresh cluster or a future
explicit durable retirement-fence design; existing executions remain usable.
The global limit permits the 100,000-job scheduler example across sufficient
nodes, but that example describes allocation complexity, not unlimited retained
execution throughput.

Node retirement controls include uncertain atomic proof publication followed by
restoration/recovery of the old active inventory, rejection of an active/proof
collision during recovery, and explicit retirement with positive absence but
unknown exit. The last case must keep its unknown result and refuse delayed
retry before and after recovery; it never fabricates success or failure.

New clustered runner admission also checks the live committed allocation, its
assigned worker and the shared normalized submission digest. The retained
index alone cannot authorize first creation after tracker pruning or on a
different worker. An exact locally owned replay may retain its original result
without the live tracker. The raw HTTP changed-spec control mutates a command
before the first local record and requires 409, no checkpoint and no launch.

Automatic retries retain the execution generation but advance restart_count.
Bind observed exit evidence to that exact generation and attempt; clear old
evidence durably in claim_job_retry before creating the next runtime. A nonzero
exit awaiting retry is not terminal. Current-attempt positive process exit proves success, or failure after the
retry budget is exhausted. Object/resource absence is a separate requirement
for compact retirement.
Retiring an unknown later attempt cannot preserve an earlier attempt's exit.
The real HTTP control makes attempt one fail, blocks attempt two, and sends a
delayed identical dispatch. Neither watcher may report completion during the
blocked retry; after exit zero and checkpoint recovery, replay preserves that
final outcome without a third launch. The existing runtime state sweep already
binds observations to created_at and restart_count; retain that guard.

Tests-first evidence on qualified validator base 9cf9eeb2 (protocol 36/state 51):
the grouped unchanged-production run completed 51 tests, with 14 passing and
37 failing; the preserved log is /tmp/runtime-535-grouped-baseline.log.
The first retry test also hit a cleanup runtime-flavour error after its intended
premature-failure assertion. Correcting that fixture to the harness-required
multi-thread runtime produced a clean failure at the same assertion, retained
in /tmp/runtime-535-retry-baseline.log. Initial JSON expression and command
conversion compile errors were fixture repairs, not behaviour evidence.
Four further raw HTTP regressions each observed old acceptance (202 instead
of 400) of schedule or run_before on public and internal batch routes, in
/tmp/runtime-535-declarative-baseline.log. These declarations are unsupported
by the batch worker and must be refused before registration, ownership or run.

This child depends on #530 workload admission and #551 live namespace
validation; both child PRs must enter the train before #535 integration.
The qualified file-backed ProcessGrill rescan repair 3335b71c is inherited for
fixed verification; original #530 and shared capture failure evidence remains.

The first fixed focus run passed 87/91 controls and retained four failures in
/tmp/runtime-535-focused.log. Two fixtures needed the new internal acknowledgement
and exit-evidence contract; uncertain checkpoint errors needed explicit wording.
The real retry control passed its blocked-second-attempt assertions but exposed
an early callback before the final exit checkpoint, then recovery refused the
unknown attempt. Owned status now waits for current durable terminal evidence.
A separate refinement baseline passed the Process exit control and failed the
retained OCI object completion control (None rather than exit zero), preserved
in /tmp/runtime-535-retained-object-baseline.log. Completion requires the durable
current process exit under the existing retry policy. Compact retirement still
independently requires positive absence of the runtime object/resources.

The next focus run passed all 222 batch and state-machine controls, retained in
/tmp/runtime-535-focused2.log. A further real HTTP remote-selector baseline
returned 400 rather than 200 on a council node holding committed ownership and
shared log rows but no execution ledger (/tmp/runtime-535-remote-log-baseline.log).
The ordinary local-owner control passed before the resolver change in
/tmp/runtime-535-ordinary-selector-control.log. An explicit canonical selector
now uses retained committed ownership when no local status owns that identity,
then authorises the original label and namespace. Unknown identities and local
ordinary collisions remain refused; ambiguous string-only paths remain 400.

The third grouped focus passed 236/236 on integrated namespace/log-store/GitOps
prerequisites (/tmp/runtime-535-focused3.log). Two unchanged admission controls
then timed out at 60 seconds because the new owned-identity query preceded the
Scope and leased-image refusal (/tmp/runtime-535-existing-admission-control.log).
These introduced failures are retained, not classified as flakes. The lookup
now follows all ordinary Role, field, lease, Scope, Deploy, HostExec and Admin
checks. Positive admission fixtures answer only the new read-only command;
their original mutation assertions remain. Three further raw API controls all
failed their finite outer bound before the new metadata deadline, retained in
/tmp/runtime-535-metadata-bound-baseline2.log. Initial duplicate-counter merge
resolution was a compilation repair, not behaviour evidence. Legacy ask_agent
has no deadline; only newly introduced ownership/alias metadata queries and
owned batch admission now bound queueing plus reply to five seconds, refusing
unavailable results before writes or launches. Scoped caller controls remain.

Normally inherited actual SQL metrics train e359c07d (protocol 36/state 52).
This child advances protocol to 37 and state to 53, preserving prior format
comments and requiring a fresh cluster across the new execution boundary.

The fourth broad focus retained four introduced failures (399/403 passing) in
/tmp/runtime-535-focused4.log: intrinsic malformed log input reached metadata
first, and positive admission relay fixtures sampled their asynchronous
mutation forwarding too early. The unchanged denied controls remained intact.
Field and namespace denial now precede metadata; positive controls await the
actual mutation. A remote selector's second metadata phase also exceeded its
outer deadline, retained in /tmp/runtime-535-remote-bound-baseline.log. Alias
resolution now has one overall five-second deadline, covering both read-only
phases and the council ownership read. Admission/allocation ownership reads
have the same overall bound. Legacy IPC remains unchanged. Logical app scope
still follows alias resolution where node-owned metadata must supply the label.
All 404 focused controls then passed on actual integrated train 4d0ee050,
retained in /tmp/runtime-535-focused5.log, with protocol 37/state 53.

Three further actual recovery controls passed Process retirement and failed
Apple/Runc retained-object retirement, in
/tmp/runtime-535-retained-recovery-baseline.log. Stopped/adopt(false) proved
process absence under legacy recovery but could not prove OCI object absence.
Owned OCI recovery now retains that distinction; it keeps full active replay
records until inspection positively establishes resource absence. The
ordinary job recovery contract is unchanged. Retained OCI objects can consume
more of the finite node inventory than compact Process proofs.

A lost HTTP admission acknowledgement is distinct from metadata timeout or
checkpoint publication failure. Metadata timeout writes and launches nothing.
A checkpoint failure starts no worker and keeps its uncertain fence. Once an
owned mutating admission is already publishing, losing its HTTP acknowledgement
may leave that original attempt running; HTTP returns 503 rather than claiming
success. Publication is never undone on reply cancellation. An identical retry
refers to the same owned attempt and cannot launch a second execution.

The recovery correction passed seven owned/ordinary controls in
/tmp/runtime-535-retained-recovery-fixed.log. Extending the same controls through
finish_app_stop then reproduced two retained OCI false-compaction failures,
with the Process control still passing, in
/tmp/runtime-535-retained-stop-baseline.log. Confirming an owned OCI process stop
now preserves the same resource-absence distinction as recovery.

The first full portable run after both Clippy matrices passed completed
5,525 tests: 5,517 passed and eight introduced failures remained, retained in
/tmp/runtime-535-ci2.log. The failures exposed a no-op inventory retirement
checking uncertainty before equality, missing inline-await justifications,
and fake actor/schema fixtures that did not implement the new internal command
or checkpoint shape. Denied permission requests and assertions were preserved;
ordinary read-only fixture replies and the positive batch acknowledgement were
updated. No failure in this receipt is classified as a flake.

Three further permanent durable-exit consistency controls all failed before
correction, retained in /tmp/runtime-535-exit-validation-baseline.log. Persist
and raw recovery accepted a phase exit conflicting with current observed exit,
and the completion predicate returned fabricated success. Publication/recovery
now reject that conflict, and the terminal predicate withholds it independently.

The combined repaired focus passed all 509 API, batch, state-machine, format,
Process recovery and owned retirement controls in 100.818 seconds, retained in
/tmp/runtime-535-focused6.log. This includes every introduced full-CI failure
and all three malformed durable-exit consistency controls. Final full portable
and required cluster gates follow on this source.

The third full portable run passed all 5,528 tests in 577.387 seconds, both
Clippy matrices, two doctests, all 52 CI-script controls and the ignored-owner
check (/tmp/runtime-535-ci3.log). The required real cluster gate passed all
42 cases in 313.474 seconds (/tmp/runtime-535-cluster.log).

Subsequent review found that schema 3 recovery accepted an omitted
batch_execution field as an ordinary null owner. A permanent raw checkpoint
control deleted that field from a valid persisted batch record and failed;
the explicit-null ordinary recovery control passed on the same unchanged
deserializer (/tmp/runtime-535-ownership-presence-baseline.log). Active records
now require the field through a presence-required deserializer, while explicit
null remains valid for ordinary jobs. The Process recovery fixture explicitly
encodes null as every schema 3 writer does. Post-change focused and full gates
follow; the preceding green receipts do not cover this final recovery change.

The post-change ownership-presence focus passed all 91 checkpoint, Process
recovery and real batch controls in 83.908 seconds
(/tmp/runtime-535-ownership-presence-fixed.log), including both raw schema 3
presence controls. Final portable CI and cluster checks use this required-field
source with protocol 37/state 53.

The final behavioral portable run completed 5,530 cases: 5,529 passed and the
README command-reference control failed after prose was added inside its
generated list (/tmp/runtime-535-ci4.log). No production or recovery control
failed. Restored the generated command text and moved the contract description
to ordinary README prose; the focused command-reference and remaining CI gates
follow. This introduced documentation failure is retained, not a flake.

The corrected command-reference control passed 1/1
(/tmp/runtime-535-readme-fixed.log); the remaining doctest, 52 CI-script and
ignored-owner gates passed (/tmp/runtime-535-final-remaining.log). The final
required-field cluster source passed all 42 real cases in 318.754 seconds
(/tmp/runtime-535-cluster-final.log). The parent then requested normal
inheritance of qualified backend/DNS train a41b42b4 and meaningful combined
focus before publication; protocol 37/state 53 stay unchanged.

Normal inheritance of a41b42b4 merged cleanly; restoring the validated work
needed no conflict repair. The combined integrated focus passed all 647 API,
batch, checkpoint, Raft, recovery, compatibility, reporting and backend/DNS
controls in 115.380 seconds (/tmp/runtime-535-integrated-focused.log).
Formatting and diff checks passed on this integrated source.

Subsequent review produced two more real-route baselines. The mixed healthy and
node-policy-invalid groups failed both controls: predictable host policy or
Process resource rejection happened after the group checkpoint, leaving two
Unknown owners and instances (/tmp/runtime-535-policy-admission-baseline.log).
The shared read-only Supervisor job gate now checks the complete policy before
any ownership publication; the same helper remains in actual job deployment.

Four SSE/WebSocket controls failed because batch follow used ordinary app
placements: unplaced workers supplied no line, while same-label placements
supplied an unrelated sentinel (/tmp/runtime-535-follow-owner-baseline2.log).
Six additional controls failed by accepting streams with missing, unadvertised
or pruned allocations (/tmp/runtime-535-follow-unavailable-baseline.log).
Original denied logical scopes remained 403. After Scope/Logs grants, selected
batch follow now proves the exact retained owner and live allocation, including
batch ID, namespace, execution, logical label and accepted-spec digest, then
routes only to that worker using the existing trusted service transport. The
same five-second overall deadline covers alias and allocation metadata. Pruned
ownership alone cannot select a remote worker; logical follow requires an
explicit instance. Ordinary placement routing remains available to ordinary
identities. All 12 new controls passed in 0.354 seconds
(/tmp/runtime-535-policy-follow-fixed.log). Broader final checks follow these
behavioral changes; earlier green receipts do not cover them.

The broader policy/follow focus passed all 901 controls in 177.013 seconds
(/tmp/runtime-535-policy-follow-focused.log). Parent review then identified a
separate local WebSocket boundary: local=true skipped the owned route lookup
but still entered ordinary cluster fan-out. Four real-route controls admitted
an actual local Process capture or selected an execution without local capture,
with a same-label remote placement in both cases. The two SSE controls passed;
both WebSocket controls failed by returning the unrelated remote sentinel
(/tmp/runtime-535-ws-local-baseline.log). WebSocket now bypasses cluster spawning
for local=true, matching SSE. Original scope-denied controls remain unchanged.
Final focused, both lint matrices, full portable and cluster qualification
follow this last behavior change.

All 16 final real policy, allocation-routing and local-follow controls passed
in 0.426 seconds (/tmp/runtime-535-policy-follow-final.log), including actual
local capture and zero-peer-contact empty local follow. Full portable CI and
required cluster qualification now use this source.

The next make ci stopped in Clippy on two new test assertions using
get(...).is_some()/is_none() instead of contains_key
(/tmp/runtime-535-ci5.log), before portable tests ran. Converted those assertions
to the equivalent map idiom; full CI resumes with the same expected behavior.

The final integrated full portable run executed all 5,550 selected tests with
zero retries or exclusions: 5,549 passed and the unchanged Apple fixture
stalled_inspection_times_out_and_reaps_the_cli failed at apple.rs:673 after
2.147 seconds because its shell PID file did not exist
(/tmp/runtime-535-ci6.log, 591.470 seconds). Both Clippy matrices and formatting
passed. The same source/line and failure shape are already tracked by #555;
this change does not modify Apple runtime code, and the cause remains unknown.
No isolated retry or full-CI pass is claimed. Remaining gates and the real
cluster qualification follow separately because make stopped at the portable
failure.

The remaining final gates passed: two doctests, all 52 CI-script tests and
ignored-test ownership (/tmp/runtime-535-remaining-final.log). The final real
cluster gate passed all 42 cases in 304.390 seconds
(/tmp/runtime-535-cluster-final2.log). No production changes followed these
runs; final formatting/diff checks and the parent review precede publication.
Exact-head remote CI must pass before merge.

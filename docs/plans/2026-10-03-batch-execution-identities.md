# Persist distinct batch execution identities

Issue: #535.

First reproduce the pull watcher completing a new batch from an older named
job while the new deploy is unacknowledged. Allocate opaque UUID runtime names
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
exit awaiting retry is not terminal. Current-attempt exit plus positive runtime
absence can prove success, or failure after the retry budget is exhausted.
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

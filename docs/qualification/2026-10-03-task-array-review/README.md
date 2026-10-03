# PR #266: task-array rebase and design review

Reviewed on 3 October 2026, rebased onto main `41dfc9ba35d377a36359cd86a7b76382301cfc1e`.
The original PR head was `160bd16f7ea4e402fc5e775cd442cefe3780285c`.
Compatibility is now protocol 35 / state 50 (main was 34 / 49).
This review changes integration and records evidence; it does not implement the
architectural corrections below.

## Recommendation

Keep the array/chunk control-plane model, but modify the executor and recovery
protocol before finishing the feature. Do correctness and resource accounting
before more views or headline benchmarks. This is a useful specialised path for
many copies of the same executable. It does not increase the throughput of the
existing heterogeneous `/v1/batch` or ordinary independent-job path.

100 million jobs/day means approximately 1,158 completed jobs/second sustained.
Capacity also depends on task duration, CPU, memory, isolation and failure rate.
A short simulated million-task run cannot establish the daily production target.

## What the implementation does

An array stores one unchanged `JobSpec` template plus a count. Workers expand
`{index}` in arguments and set per-task environment variables. The default chunk
is 1,024 tasks, so one million tasks becomes 977 chunks. Compact queued/done range
sets and grants replace per-task objects in Raft. The leader polls each node over
direct HTTP once per second and combines results and new grants into at most one
Raft `Sync` entry per active array per tick. Silent holders are requeued after
30 seconds. Each node expands its chunks locally, retries and times out tasks,
and writes terminal outcomes into a CRC-framed local ledger.

This amortises placement and consensus overhead. Cost still grows with nodes,
arrays, chunks, failure ranges and retained outcomes. The direct HTTP fan-out
also differs from the hierarchical completion reporting in the whitepaper.

## Correctness findings

The five probes in [probes.rs](probes.rs) deliberately assert the current defects;
they are evidence, not tests asserting desired behaviour. [probe-output.txt](probe-output.txt)
records their successful reproduction. Instructions to rerun are in the source.

1. **Capacity is per array, not per node.** `TaskArrayNode::open` creates a new
   full-capacity `TaskPool` for each array. A node configured for two slots ran
   four tasks with two arrays. CPU/memory declarations and existing application
   reservations do not govern this executor. Use one node-wide budget with
   resource eligibility, application reservations and cross-array fairness.
2. **The group-commit interval does not bound replay of completed work.**
   `run_chunk` calls `ledger.append` only after the entire chunk finishes. The
   probe completed 12 tasks while the ledger still contained zero records after
   150 ms, despite a 10 ms group-commit interval. Feed terminal task outcomes to
   the writer as they finish; acknowledge retirement only after durability.
   Separately, code inspection shows an append error is logged but the completed
   chunk is still reported. That needs a defined storage-failure transition.
3. **A torn tail is tolerated but not repaired before append.** Reopening a
   ledger with a torn block and appending a valid record made later replay fail
   with `Corrupt { offset: 30 }`. Recover the valid boundary and truncate the
   incomplete suffix before accepting new records. Check write/fsync failures.
4. **Delayed sync can replace newer assignments with older grants.** After a
   worker finished attempt 2, a delayed attempt-1 assignment executed the same
   ten tasks again and reported attempt 1. Leader-side result fencing does not
   fence execution. Sync needs monotonic applied-state/leadership provenance,
   rejection of stale requests, and explicit grant lifetime semantics. Retries
   still require idempotent task effects; do not promise exactly-once execution.
5. **Failed-only results can contradict the leader.** A superseded node's
   failure survived merging even though the leader counted success and zero
   failures. Successful replacement rows are filtered out before the merge;
   records also lack the grant provenance needed to identify the accepted run.
   Define authoritative result selection before filtering or fetching logs.

## Other trade-offs and limits

- This is currently a raw host-process executor. It rejects image and script
  templates and refuses nodes with `mount_isolation`, including the Linux
  default. `ProcessRunner` does not use the existing runtime owner, resource
  containment or process-group retirement. Define and implement that execution
  contract before treating quickstart or production execution as supported.
- Ledgers and failed output are local, not replicated. Permanent worker loss can
  lose detailed outcomes of chunks already retired in Raft. Choose explicitly
  whether users need best-effort node-local results or durable archived results.
- Successful output is discarded; failed output retains only a bounded head and
  tail. There is no complete per-attempt history. These are sensible optional
  cost controls if exposed as an honest retention contract.
- Chunk prefetch and the extra tick before grants reach workers trade control
  traffic for refill latency and idle slots, especially with tiny tasks. Measure
  and tune rather than assuming the simulated executor rate transfers to HTTP
  dispatch and fork/exec.
- Results replay an entire ledger even for a small response limit and query
  nodes serially. Retention is bounded by 20 completed arrays / one hour and is
  pruned on registration; age is measured from submission. Check long-running
  arrays, pagination, memory bounds and cleanup under continuous submission.

## Qualification needed

First define representative workloads and the execution/result guarantees. Then
measure the existing job path and the corrected array path using real executables
on stated hardware with concurrent applications and multiple arrays. Count unique
accepted/completed task indices separately from execution attempts. Exercise
leader changes, delayed syncs, partitions, worker restart/loss, disk failures and
ledger truncation. Demonstrate at least 1,158 unique completions/second with
headroom over 24 hours, bounded memory/disk and honest result-loss reporting.
Heterogeneous jobs need a separately designed batched durable manifest/queue;
the same executor and grant protocol can be reused without discarding arrays.

## Rebase validation

- `cargo check --all-targets --offline`: passed.
- Formatting and clippy for all targets with and without default features: passed.
- `make ci` with four test processes and no fail-fast: formatting and both
  clippy configurations passed; 5,417 portable tests passed, two unchanged
  quickstart tests failed, 76 skipped. The portable CI run is not green.
- `make test-doc test-ci-scripts check-ignored`: passed separately after
  nextest stopped `make ci`; both doctests, all 52 script tests and ownership
  checks passed.
- Focused quickstart check, serial, five iterations: nine of ten test
  executions passed, one failed. This is evidence of the flake, not a clean CI
  result. Tracked by #517 in the existing 0.1.5 milestone.
- Complete `make test-cluster` gate: all 43 tests passed, one skipped, 268.59 s.
- Focused three-node task-array gate: passed; 100,000 simulated tasks, one node lost,
  27 Raft entries, 27.63 seconds. Real networking and Raft, `FakeRunner` execution.
- Five focused defect probes: reproduced all five findings above.
- Real VM throughput, Linux process isolation and a 24-hour throughput soak were
  not run. These results do not qualify the 100-million/day claim.

The first portable run after fixing the rebased compatibility assertion hit both
unchanged quickstart runner tests tracked by [#517](https://github.com/reliaburger/reliaburger/issues/517).
The create script missed a 200 ms watchdog: one test omitted the initial create
command; the other never wrote its console marker. The full rerun uses four
nextest test processes, no retries and no fail-fast. This is separate from the
five task-array defects above; the underlying macOS startup delay is unconfirmed.

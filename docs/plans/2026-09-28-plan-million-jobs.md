# Plan: a million jobs (task arrays at scale)

Status: in progress on `feat/million-jobs`, for **0.2.0** ("A million jobs").
Task arrays are wired into Raft, the API and `relish` (M5). The wiring bumped
the compatibility generations to protocol 47 and state 64, so 0.2.0 needs a
fresh cluster: the maintainer decided on 28 September 2026 that there's no
backwards compatibility before 1.0.0 (see [Compatibility](#compatibility)).

The October [resource-aware plan](2026-10-04-plan-delegated-jobs.md) now owns
implementation and qualification. This file retains the September design and
library history. It originally superseded
[`2026-09-25-plan-task-arrays.md`](2026-09-25-plan-task-arrays.md) (merged in
from `plans/million-jobs` unchanged), whose findings it keeps and whose
100,000-job tour step becomes the fallback headline below.

## Where to pick up

*Keep this section current. Another agent resumes from here.*

**3 October review:** keep the array/chunk concept, but resource accounting,
durable outcomes and stale-grant fencing need correction before finishing the
feature. Five defects reproduced; see the [review and qualification evidence](../qualification/2026-10-03-task-array-review/README.md).
The maintainer authorised implementation on 4 October. The
[resource-aware delegated jobs plan](2026-10-04-plan-delegated-jobs.md) now owns
implementation and qualification; the library history below is retained.

- **Branch:** `feat/million-jobs`, rebased onto `main` at `41dfc9ba` on
  3 October 2026 (the original base was `087d882f`). Draft PR #266, "Million jobs:
  task arrays at scale (0.2.0)", labelled `full-ci`.
- **Done (library, M1-M4):** `src/meat/index_set.rs`, `task_array.rs`,
  `task_array_state.rs`, `latency_histogram.rs`; `src/bun/task_executor.rs`,
  `task_ledger.rs`; `tests/suite/task_array_million.rs` (100k default,
  `RELIABURGER_TASK_ARRAY_TASKS=1000000` for the full run: 30.6 s debug);
  `benches/task_arrays.rs` (`make bench-task-arrays`, release numbers still to
  record).
- **Done (wiring, M5), five commits `M5.1`-`M5.5`:**
  - Raft: one variant, `RaftRequest::TaskArray(Box<TaskArrayWrite>)`
    (`Register`, `Sync`, `Cancel`, `Requeue`), applied by
    `TaskArrays::apply` in `src/meat/task_array_store.rs`;
    `DesiredState::task_arrays`; `CouncilResponse::TaskArrayRegistered`. Ids
    come from `batch_state`'s counter. Finished arrays are pruned after an
    hour or past 20; at most 64 run at once. Compatibility 35/50.
  - Node: `src/bun/task_array_node.rs` (`TaskArrayNode::sync`, results,
    failed-task output; resume from the ledger via
    `TaskPool::resume_chunk`; zero slots plus a reason for a binary off the
    allowlist or a node with `mount_isolation`).
  - Leader: `src/bun/task_array_leader.rs` (`TaskArrayService`, the loop
    spawned from `router_with_upgrade`, one `Sync` entry per array per tick,
    requeue for holders silent 30 s or refusing; standalone in memory).
  - API: `src/bun/task_array_api.rs` (`POST /v1/batch/array`,
    `POST /v1/batch/{id}/cancel`, `GET /v1/batch/{id}/results`,
    `GET /v1/batch/{id}/tasks/{index}/logs`, node-to-node
    `POST /v1/batch/array/sync` and two `local` reads; `GET /v1/batch/{id}`
    answers arrays with `"kind": "array"`). `router_with_upgrade` gained a
    last parameter, `task_arrays: Option<Arc<TaskArrayService>>`; `bun`
    passes a process-runner executor under `<data>/task-arrays/`.
  - CLI: `relish run --batch NAME --count N --exec PATH -- ARGS`,
    `relish batch cancel|results|logs`, arrays in `relish batch-status`.
  - Tests: unit tests in every new module; portable suite
    `tests/suite/task_arrays.rs` (100,000 fake tasks through a single-node
    council in 51 Raft entries; real processes; failures, results, logs,
    cancel, refusal); gated `tests/cluster_task_arrays.rs` (three wired
    nodes, follower killed mid-run) in `make test-cluster`.
  - Docs: manual `01_deploy-an-app.md` "Task arrays"; book Chapter 12
    "Wiring it in"; `docs/roadmap.md`; README; `docs/testing.md`.
- **September next steps (superseded by the October plan):**
  1. Before the rebase, CI on PR #266 was green with `full-ci` (run
     36486408185, commit `348db40f`: lint, portable Linux and macOS, privileged Linux, minimum
     Rust, benchmarks, acceptance, and the multi-node cluster job, which
     runs `cluster_task_arrays`). Keep `full-ci` on while the wiring moves.
  2. After the soak (about 03:30 BST): record `make bench-task-arrays`
     release numbers here; M0 on a quickstart VM (fork/exec rate of
     `/usr/bin/true` at 1x/2x/4x/8x vCPU concurrency, fsync latency, Raft
     commit rate).
  3. M6 views: `batch-status --watch` and `--cost`, rate and latency
     histograms in the summary (record start lag and run time into
     `LatencyHistogram` from `TaskPool`, add them to `ArrayProgress`, merge
     on the leader), Ketchup sampling (first 100 successes, 1 in 1,000
     after, every failure), Mayo counters, TUI view, dashboard card.
  4. M7: mount isolation for tasks (today a node with
     `[process_workloads] mount_isolation = true`, the Linux default, refuses
     arrays with a reason) and host processes on quickstart nodes; an
     `rb-task` demo binary under `examples/million-jobs/`.
  5. Known limits worth a look before M8: grants reach a node one tick
     after they're written (the leader could re-sync nodes that just got
     grants within the same tick); each array gets its own pool per node,
     so two arrays on one node can use twice its CPUs; results and logs
     fan out to nodes one at a time (3 s timeout each); gossip-dead nodes
     wait out the 30 s silence rather than being requeued at once.
- **Historical local build constraints during the September release soak:**
  `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=$HOME/.cache/rb-target-millionjobs`;
  The following applied during that soak, not to the October rebase review:
  run only targeted tests (`cargo test --lib task_array`,
  `cargo test --test suite task_arrays`) and leave the full suite to CI. No
  Lima VMs, quickstart clusters, in-process multi-node clusters or
  `qualify-*.sh` until the soak finishes. Delete that target directory when
  the branch is done.
- **Don't** touch `StateReport` or `ReportingMessage` (bincode); progress
  travels over the sync route.

## Contents

1. [Goals](#goals)
2. [The demo](#the-demo)
3. [Why this is hard on Kubernetes](#why-this-is-hard-on-kubernetes)
4. [Where we are today](#where-we-are-today)
5. [Design](#design)
6. [Compatibility](#compatibility)
7. [Phases](#phases)
8. [Benchmarks](#benchmarks)
9. [Risks](#risks)
10. [Progress checklist](#progress-checklist)

## Goals

1. **One request, a million tasks.** A task array is a job template plus a
   count. Submitting 1,000,000 tasks is one API call of a few kilobytes, not a
   million objects.
2. **Each task is individually real.** Every index is its own process, with
   its own exit code, retries, timeout and (sampled) log lines, all tracked by
   the orchestrator rather than by user code draining a queue.
3. **Constant control-plane cost per unit time.** Raft writes, leader memory
   and reporting bytes scale with the number of *nodes* and *chunks*, never
   with the number of tasks. No per-task Raft entry, ever.
4. **Honest numbers.** Every published figure comes from a benchmark in the
   repository that anyone can rerun, with the machine, variance and failure
   rate recorded next to it.
5. **A development release.** 0.2.0 is below 1.0.0, so it doesn't keep
   compatibility with 0.1.x: the wiring bumps `protocol` and `state`, old
   peers and old state are refused, and upgrading means a fresh cluster. No
   migration, no feature gate.

Non-goals for this plan: DAG workflows (that's Argo's job and a later plan),
gang scheduling, GPU task arrays, cross-array dependencies, and exactly-once
execution (we promise at-least-once and say so).

## The demo

### What the audience sees

A three-node cluster (the headline runs on three cloud VMs; the laptop version
runs on the three quickstart VMs). One terminal, four commands:

```sh
# 1. One request. 1,000,000 tasks. Each index is its own process.
relish run --batch squares --count 1000000 --chunk 1024 \
    --exec /usr/local/bin/rb-task -- square '{index}'

# 2. Live view: counters, rate, latency percentiles, per-node table.
relish batch-status 7 --watch

# 3. Any single task is still addressable.
relish batch logs 7 --index 424242
relish batch results 7 --failed

# 4. What did it cost the control plane?
relish batch-status 7 --cost
```

`rb-task square N` is a tiny demo binary shipped with the examples (`examples/
million-jobs/`): it prints `N*N`, and it exits 1 on its first attempt when
`N % 100 == 42`, so one task in a hundred fails once and succeeds on retry.
That gives the retry counter and the per-index attempt history real numbers
without faking anything.

The `--watch` screen, refreshed every second:

```text
batch 7  squares  1,000,000 tasks  1024/chunk  977 chunks          02:41 elapsed
  succeeded  998,112   failed 0   running 96   queued 1,792   retried 10,000
  rate       6,212/s (1 s)  6,180/s (10 s)     eta 00:01
  start lag  p50 3 ms   p90 9 ms   p99 41 ms
  run time   p50 1.1 ms p90 1.9 ms p99 7.4 ms
  node      running  done       rate/s   chunks held
  n1        32       333,901    2,071    2
  n2        32       332,640    2,062    2
  n3        32       331,571    2,049    2
  top exit codes: 1 x 10,000 (all retried)   sample: 42, 142, 242
```

`--cost` prints the numbers that make the Kubernetes comparison concrete:
Raft entries written for this batch, bytes of Raft state it occupies, leader
RSS delta, reporting bytes per node per second, and ledger bytes on disk.

### Target numbers

| Quantity | Headline target | Laptop target | Measured by |
|---|---|---|---|
| Tasks | 1,000,000 | 1,000,000 (fallback 100,000) | the batch itself |
| Cluster | 3 nodes x 16 vCPU Linux | 3 quickstart VMs x 4 vCPU on Apple silicon | `relish bench` environment record |
| Wall time, submit to last result | at most 3 min (at least 5,600 tasks/s) | at most 10 min (at least 1,700/s) | `batch-status` elapsed |
| p99 start lag (queued to spawned, once a slot is free) | under 1 s | under 1 s | executor histogram |
| Raft entries for the whole batch | at most 3 per second of runtime, plus 1 | same | `--cost` |
| Raft state held by one 1M-task array | at most 256 KiB JSON | same | unit test and `--cost` |
| Leader RSS growth during the batch | at most 64 MiB | at most 64 MiB | `--cost` |
| Leader CPU for batch control | at most 1 core average | at most 0.5 core | Mayo |
| Executor overhead per node beyond the tasks | at most 0.5 core | at most 0.5 core | Mayo |
| Submission request | under 4 KiB | same | integration test |
| Node ledger on disk per 1M tasks | at most 16 MiB, deleted after retirement; measured bound since #678: at most 50 bytes per task (measured ledger 22.4 + result index 25.0 on APFS, about 45 MiB per 1M), deleted an hour after retirement on a leader timer | same | unit test and `make bench-task-arrays` |
| Log volume retained | at most 50 MB for the whole run | same | Ketchup |

The laptop numbers are gated: the headline says "a million" only if M0 and the
M4 prototype show the laptop sustaining 1,700 tasks/s with p99 start lag under
a second. Otherwise the tour uses the original plan's claim, "100,000 jobs in
about a minute", which needs about 2,000/s for 50 s. Record the variance and
the thermal state, because a throttling laptop moves the number.

### The sentence we want to be able to say

> A million jobs, each its own process, individually retried and logged, on
> three machines in three minutes, with the control plane writing a few Raft
> entries a second.

Not "Kubernetes can't run a million jobs" (it can, in waves, with enough
etcd), and not "a million tasks packed into a thousand workers" (a work queue
does that on Kubernetes too).

## Why this is hard on Kubernetes

Sources checked on 28 September 2026. Re-check them before anything is
published.

- **One Pod per task.** An Indexed Job creates one Pod per completion index
  ([Jobs](https://kubernetes.io/docs/concepts/workloads/controllers/job/)). A
  Pod's life is several API writes persisted in etcd: create, bind, kubelet
  status updates, the Job controller's finalizer removal, then deletion by the
  Job or the terminated-pod garbage collector. Call it six to eight etcd
  writes and a few kilobytes of object per task, so a million tasks is
  millions of etcd writes.
- **Indexed Job limits.** With `completionMode: Indexed`, `parallelism` must
  be at most 10^5, and when `backoffLimitPerIndex` is used with more than 10^5
  completions, `maxFailedIndexes` must be set and be at most 10^4
  ([Job API reference](https://kubernetes.io/docs/reference/kubernetes-api/workload-resources/job-v1/),
  [KEP-3850](https://github.com/kubernetes/enhancements/tree/master/keps/sig-apps/3850-backoff-limits-per-index-for-indexed-jobs),
  [v1.33 GA post](https://kubernetes.io/blog/2025/05/13/kubernetes-v1-33-jobs-backoff-limit-per-index-goes-ga/)).
- **Cluster scale envelope.** At most 110 Pods per node and 150,000 Pods in
  total ([Considerations for large clusters](https://kubernetes.io/docs/setup/best-practices/cluster-large/)).
  A million tasks must run in waves that are created, finished and
  garbage-collected.
- **Start latency.** The upstream SLO is p99 pod startup at most 5 s per
  cluster-day, excluding image pulls and init containers
  ([pod startup SLO](https://github.com/kubernetes/community/blob/master/sig-scalability/slos/pod_startup_latency.md)).
  A sandbox per task costs on the order of a second of node work, which is
  longer than the tasks in this demo.
- **Controller throughput.** The Job controller syncs 5 Jobs concurrently by
  default and talks to the API server through a client-side rate limiter
  ([kube-controller-manager flags](https://kubernetes.io/docs/reference/command-line-tools-reference/kube-controller-manager/):
  `--concurrent-job-syncs`, `--kube-api-qps`, `--kube-api-burst`,
  `--terminated-pod-gc-threshold`). Quote the defaults from that page, not
  from memory.
- **Argo Workflows** keeps a workflow's node status inside the Workflow
  object in etcd, which must stay under about 1 MB; large workflows compress
  it and then offload it to a SQL database
  ([Offloading large workflows](https://argo-workflows.readthedocs.io/en/latest/offloading-large-workflows/)).
  Each step is still a Pod.
- **Volcano and Kueue** add queueing, fair sharing and gang scheduling on top
  of Pods ([Volcano](https://volcano.sh/en/docs/), [Kueue](https://kueue.sigs.k8s.io/docs/overview/)).
  They decide *when* Pods run; each task is still a Pod.
- **Armada** (G-Research) moves the queue out of etcd and fans jobs out over
  many clusters for very large batch volumes
  ([Armada](https://armadaproject.io/)). It's the right comparison for
  throughput and the wrong one for simplicity: it's another control plane
  beside Kubernetes.
- **The work-queue pattern** (a few long-lived workers draining Redis or
  similar, [Kubernetes docs](https://kubernetes.io/docs/tasks/job/fine-parallel-processing-work-queue/))
  scales fine, but the orchestrator no longer knows about individual tasks:
  retries, logs and results become application code.

So the fair claim is about *per-task tracking at constant control-plane
cost*. Reliaburger stores the template once and the progress as ranges, so the
thing that grows with a million tasks is a node-local file of fourteen-byte
records, not a database of objects.

## Where we are today

The 25 September code reading still holds (see the superseded plan for file
references): `POST /v1/batch` carries one full `JobSpec` per job under axum's
2 MiB body limit (about 8-10k jobs); `schedule_batch` has no queue and marks
the overflow `Unschedulable`; there are quadratic `find`s on submit, report and
both watchers; `BatchRegister` stores every job and each finished job is its
own `BatchJobUpdate` Raft write; each job goes through the full deploy path
with an owner helper, two log files and a checkpoint rewrite; host-process
jobs can't run on runc-only quickstart nodes. Throughput estimate: 10-30
jobs/s as containers, 60-150/s as processes.

The existing batch path stays as it is. Task arrays are a second, parallel
path for homogeneous work; heterogeneous `relish batch` files keep today's
per-job behaviour.

Two facts about formats that shape the compatibility section:

- Raft log entries and snapshots are **JSON** (`src/council/durable_log.rs`,
  `src/council/state_machine.rs`). The comment above `UpgradeUpdate` in
  `src/council/types.rs` still says "the log is bincode-encoded", which is
  stale; fix it in M5 (wiring).
- The reporting tree (`StateReport`, `ReportingMessage`) is **bincode**
  (`src/reporting/transport.rs`), so it's positional: any new field or
  variant there is incompatible.

## Design

### Vocabulary

- **Task array** (or array batch): one job template, a `count`, and policy.
- **Task**: one index in `0..count`. Each task is one process.
- **Chunk**: a fixed-size contiguous range of indices, `chunk_size` long
  (default 1,024; the last chunk may be shorter). The chunk is the unit of
  allocation, of Raft bookkeeping and of retirement. A 1M array with the
  default is 977 chunks.
- **Grant**: the leader's decision "node N runs chunk C, attempt A".
- **Ledger**: a node's append-only record of task outcomes for its chunks.

### Data model (library, M1)

`src/meat/index_set.rs`:

- `IndexRangeSet`: a sorted set of disjoint, non-adjacent half-open `u32`
  ranges. Insert, insert range, remove range, contains, union, length (as
  `u64`), range iteration. Serialised as `[[start, end], ...]`. It's what
  stores completed chunks, failed indices, queued chunks and per-node grants:
  a million completed tasks in order is one range, and the usual shape of
  failures (sparse) costs eight bytes per failure. A fixed-size bitmap would
  be 122 KiB for a million indices regardless of content; ranges are 8 bytes
  for the common case and never worse than twice the number of set members.

`src/meat/task_array.rs`:

- `TaskArraySpec { count, chunk_size, max_attempts, max_failed_indexes,
  task_timeout_secs, per_node_concurrency }`, validated with a
  `TaskArraySpecError` (thiserror). Limits: `count` 1 to 16,777,216 (2^24);
  `chunk_size` 1 to 65,536, and `count / chunk_size` at most 65,536 chunks;
  `max_attempts` 1 to 10 (default 3); `task_timeout_secs` 1 to 86,400
  (default 600). The template is today's `JobSpec` carried *beside* the
  array spec, never extended, because `JobSpec` has
  `#[serde(deny_unknown_fields)]`.
- `ChunkId(u32)`, `chunk_count`, `chunk_range(chunk) -> Range<u32>`,
  `chunk_of(index)`.
- Template expansion: `expand_argv(template, index) -> Vec<String>` replaces
  every `{index}` in `exec` arguments and `command`, and `task_env(batch,
  index, attempt)` adds `RELIABURGER_BATCH_ID`, `RELIABURGER_TASK_INDEX`,
  `RELIABURGER_TASK_COUNT` and `RELIABURGER_TASK_ATTEMPT`. No other template
  language: a task that needs more derives it from the index.

### Leader: chunk table and grants (library, M2)

`TaskArrayState` is a deterministic state machine, serialisable to JSON, which
becomes one entry in `DesiredState` in M5. It holds:

- `queued: IndexRangeSet` of chunk ids not yet granted,
- `grants: BTreeMap<NodeId, IndexRangeSet>` of chunks held per node,
- `attempt: BTreeMap<u32, u8>` only for chunks re-granted after a node loss
  (sparse; absent means attempt 1),
- `done: IndexRangeSet` of retired chunks,
- `failed_indices: IndexRangeSet` capped at 10,000 ranges, with
  `failed_overflow: u64` counting the rest,
- `succeeded`, `failed`, `retried` totals, `status` (`Running`, `Succeeded`,
  `CompletedWithFailures`, `Failed`, `Cancelled`), submission time from the
  request (never `now()` inside `apply`).

Operations, each a pure function the Raft `apply` will call:

- `grant(node, chunks)`: move chunks from `queued` to that node's grants;
  refuse chunks that aren't queued.
- `complete(node, chunk, attempt, outcome)`: accept only if the chunk is
  currently granted to that node *at that attempt*. That fences stale
  completions from a node the leader already gave up on. A duplicate
  completion of a done chunk is an idempotent no-op; everything else is a
  typed error, like the existing batch transition table.
- `requeue_node(node)`: return a lost node's unfinished grants to `queued`
  with `attempt + 1`.
- `cancel()`: clear `queued`, mark `Cancelled`; grants drain as nodes stop.
- `summary()`: counts, derived without scanning tasks.

The grant *policy* is a separate pure function, `plan_grants(state,
node_slots) -> Vec<(NodeId, Vec<ChunkId>)>`. Each node reports how many task
slots it has (its per-array concurrency) and how many chunks it holds; the
leader tops every node up to `depth = max(2, ceil(2 * slots / chunk_size))`
chunks, lowest chunk ids first. That's pull-based load balancing: a fast node
finishes chunks sooner and asks for more, so there's no up-front partition to
go wrong (scheduler-meat 5.2's weighted partition has a straggler tail by
construction). No resources means no capacity limit, but concurrency still
caps what runs at once.

Tail behaviour: in v1, no speculative duplicates. When the queue is empty the
batch waits for the slowest chunk. Speculative re-execution of stragglers is a
follow-up, since at-least-once already allows it.

### Node: the executor (library, M3)

`src/bun/task_executor.rs` and `src/bun/task_ledger.rs`:

- **Worker pool.** One tokio task per array per node, running at most
  `concurrency` tasks at once through a `JoinSet` window (the same shape as
  the P2P download executor in Chapter 12). Concurrency defaults to the
  node's CPU count and is capped by `per_node_concurrency`.
- **Runner seam.** A `TaskRunner` trait with two implementations: the real
  `ProcessRunner` (spawns with `tokio::process::Command`, no owner helper, no
  checkpoint, `kill_on_drop`, stdout and stderr piped and truncated to a
  bounded head and tail) and `FakeRunner` for tests and in-process benchmarks
  (outcome computed from the index, optional simulated duration). Two
  implementations exist from day one, so the trait isn't premature.
- **Retries** happen inside the pool: exponential backoff from 100 ms up to
  5 s, jittered deterministically by index, up to `max_attempts`. A timeout,
  a signal or a non-zero exit is a failed attempt. Spawn errors (missing
  binary) are fatal for the task without retry, because retrying can't fix
  them.
- **Ledger.** One append-only file per array under
  `<data>/task-arrays/<batch>/ledger`, fixed 14-byte records
  (`index: u32, attempts: u8, outcome: u8, exit_code: i32, run_ms: u32`) in
  framed blocks with a CRC32 each. Group commit: the writer fsyncs every 100 ms
  or 4,096 records, whichever comes first, and a chunk is reported complete
  only after its last record is durable. On restart the node replays the
  ledger, and any task in a held chunk without a terminal record runs again.
  That's the at-least-once rule, and it's written down in the manual.
- **Retirement.** When the leader's sync acknowledges a chunk as done in Raft,
  the node forgets it; when the whole array is terminal and acknowledged, the
  node deletes the ledger directory. Per-task results survive until then and
  are fetchable through `relish batch results`.
- **Cancellation.** Stop admitting, send SIGTERM to running tasks, SIGKILL
  after a 10 s grace, report partial chunks as cancelled.
- **Counters** are plain integers plus a mergeable latency histogram
  (`src/meat/latency_histogram.rs`: log-linear buckets, 4 per power of two,
  128 `u64` buckets in microseconds, merged by addition, percentiles within
  25%, serialised sparsely). No new crate needed.

### Leader and node talk once a second (wiring, M5)

One RPC per node per active array per second, leader to node, carrying
everything: `POST /v1/batch/array/{id}/sync` (system principal only) with body
`{ new_grants, acknowledged_chunks, cancel }`, answered with
`{ slots, held_chunks, finished_chunks: [{chunk, attempt, succeeded,
failed_indices, retried}], counters, histograms }`.

Why pull rather than push through the reporting tree:

- The reporting tree is bincode, so adding progress there is a protocol
  change; a new HTTP route is additive.
- The leader owns the cadence, so a restarted leader resumes by simply
  starting to sync again: no callback URL, and none of the credential
  exfiltration class Chapter 12 closed.
- It's O(nodes) requests per second. At 200 nodes that's 200 small requests
  a second, well within what axum does.

The leader then writes **at most one Raft entry per second per array**
combining new grants and finished chunks (`TaskArraySync`), plus
`TaskArrayRegister` at submit and `TaskArrayCancel`/`TaskArrayRequeue` when
they happen. That's the "few entries a second" in the demo sentence.

A node that doesn't answer sync for 30 s, or that gossip marks dead, gets
`TaskArrayRequeue`. When it comes back, its stale completions are fenced by
the attempt number.

### Views, logs, metrics, results (M6)

- `relish run --batch NAME --count N [--chunk K] [--max-attempts A]
  [--max-failed F] [--timeout S] --exec PATH -- ARGS...` submits an array.
  The existing `relish batch FILE` gains `count` in the TOML `[batch]` table
  only if it stays additive; otherwise arrays are CLI and API only in v1.
- `relish batch-status ID --watch` renders the screen above from
  `GET /v1/batch/{id}` (additive JSON fields on the summary: `array`,
  `rate`, `histograms`, `per_node`). `--cost` reads a small cost block.
- `relish batch logs ID --index I` asks the node that ran the index (the
  chunk's grant history says which) for that task's captured head and tail.
- `relish batch results ID [--failed]` streams `index, attempts, exit_code,
  run_ms` from the nodes' ledgers.
- **Log sampling.** Ketchup gets every failed attempt's captured output, the
  first 100 successful tasks, then one in 1,000, each with a `task_index`
  field. Everything else stays in the per-task head and tail in the node's
  ledger directory until retirement. That's how 1M tasks stay under the 50 MB
  budget.
- **Metrics.** Mayo counters labelled by `batch` and `node` only (never by
  task): started, succeeded, failed, retried, running, plus the run-time and
  start-lag histograms. The series count is fixed per array per node.
- A TUI batch view and a dashboard card come last and read the same summary.

### How this avoids per-object overhead

| Per task on Kubernetes | Per task here |
|---|---|
| A Pod object in etcd, several writes over its life | nothing in Raft; an index inside a range |
| A scheduler decision | nothing; the chunk was granted once per 1,024 tasks |
| A sandbox, a pause container, cgroups, a network namespace | one `fork`/`exec` from an existing worker |
| Kubelet status updates to the API server | a counter increment |
| Job controller status patch and finalizer removal | a 14-byte ledger record, group-committed |
| Pod garbage collection | ledger deleted once per array |
| Log files per container | a head and tail in memory, sampled into Ketchup |

## Compatibility

Decision (maintainer, 28 September 2026): there's no backwards
compatibility before 1.0.0. 0.2.0 is a development release, so there's no
finalisation gate and no migration. Any incompatible change bumps `protocol`
or `state` in `src/compatibility.rs` (the policy is in `CLAUDE.md` and
`docs/releasing.md`, draft PR #254), nodes refuse 0.1.x peers and 0.1.x
state, and upgrading means a fresh cluster. The table below records what
changes, so the bump is deliberate and the tests know where to look.

### Every format change, and how it's handled

| Change | Where | Encoding | Handling |
|---|---|---|---|
| `IndexRangeSet`, `TaskArraySpec`, `TaskArrayState`, ledger | new library types | JSON / local file | library only until M5 |
| `RaftRequest::TaskArray(TaskArrayWrite)`, one variant wrapping `Register`, `Sync`, `Cancel` and `Requeue` (`src/meat/task_array_store.rs`) | `src/council/types.rs` | JSON, externally tagged | new variant: **protocol and state bump** (done: protocol 35, state 50) |
| `CouncilResponse::TaskArrayRegistered { batch_id }` | same | JSON | same bump |
| `DesiredState::task_arrays` | `src/council/types.rs` | JSON snapshot | same bump |
| `JobStatus`, `BatchRecord`, `BatchJobUpdate` | `src/meat/batch_tracker.rs` | JSON | **unchanged**; arrays never reuse them |
| `BatchSummary` gains optional `array`, `rate`, `histograms`, `per_node` | `src/meat/batch_tracker.rs`, HTTP | JSON | new optional fields; covered by the bump |
| `POST /v1/batch/array`, `POST /v1/batch/array/{id}/sync`, `POST /v1/batch/{id}/cancel`, `GET /v1/batch/{id}/results`, `GET /v1/batch/{id}/tasks/{index}/logs` | `src/bun/api.rs` | JSON over HTTP | new routes, added to the `authz.rs` matrix |
| `JobSpec` | `src/config/job.rs` | TOML/JSON, `deny_unknown_fields` | **unchanged**; array fields live in a wrapper |
| `StateReport`, `ReportingMessage` | `src/reporting/types.rs` | **bincode** | **unchanged**; progress travels over sync instead |
| Mayo series `reliaburger_task_array_*` | metrics | Prometheus text | new series |
| Ketchup `task_index` field | logs | Arrow/Parquet column | nullable column: check whether the archive reader tolerates it before M6; if not, carry the index in the message instead |
| Node files under `<data>/task-arrays/` | local disk | new ledger format | covered by the state bump |

Test fixtures that model a compatible peer keep taking their pair from
`compatibility::CURRENT`, and M5 adds a test that a 0.1.x peer and 0.1.x
state are refused.

## Phases

Each step writes its failing tests first, then the code, then ticks the
checklist in the same commit.

| Phase | What | Needs a cluster? | Rough effort |
|---|---|---|---|
| M0 | Measure: fork/exec rate of a trivial binary inside a quickstart VM at 1x/2x/4x/8x vCPU concurrency; fsync latency of the VM disk; Raft commits per second | yes, after the soak | 2 days |
| M1 | Data model: `IndexRangeSet`, `TaskArraySpec`, chunk maths, template expansion | no | 2-3 days |
| M2 | Leader state machine and grant policy (`TaskArrayState`, `plan_grants`), requeue and fencing | no | 3-4 days |
| M3 | Node executor: runner seam, worker pool, retries, timeouts, cancellation, ledger with group commit, replay | no (process runner tests are local-only processes) | 1-1.5 weeks |
| M4 | In-process benchmarks and the million-task acceptance test | no | 2-3 days |
| M5 | Wiring: compatibility bump, Raft variants, API routes, leader sync loop, CLI submit, status, cancel | CI cluster suites | 1.5-2 weeks |
| M6 | Views, sampled logs, results, Mayo metrics, TUI view, dashboard card | CI | 1-1.5 weeks |
| M7 | Host processes on quickstart nodes (the old P1), with an allowlist of exactly one demo binary | Lima | 3-5 days |
| M8 | Real-cluster benchmarks (quickstart, then 3 cloud VMs), the tour step, manual, book, whitepaper Q8 and scheduler-meat 5.2 rewrite | yes | 1 week |

About six to eight weeks for one engineer, a week less than before because
there's no finalisation gate to build. M1-M4 (about three weeks) are done;
M5 onwards is about four to five and a half weeks, plus M0.

### Tests-first steps for M1-M4

**M1.1 `IndexRangeSet`.** Unit tests: empty set; insert merges neighbours on
both sides; insert inside an existing range is a no-op; `insert_range` across
several ranges coalesces; `remove_range` splits a range; `contains` at range
edges; `len` of the full `u32` domain doesn't overflow; serde round trip and
the exact JSON shape; a hand-built unsorted or overlapping JSON is refused on
deserialise. Property test against a `BTreeSet<u32>` model for random insert
and remove sequences, checking the invariant (sorted, disjoint, non-adjacent,
non-empty ranges) after every step.

**M1.2 `TaskArraySpec` and chunk maths.** Validation accepts the limits and
refuses zero count, zero chunk, too many chunks, attempts out of range;
`chunk_count` rounds up; `chunk_range` of the last chunk is short;
`chunk_of(index)` inverts `chunk_range`; property test that chunk ranges
partition `0..count`.

**M1.3 Template expansion.** `{index}` replaced in every argument, several
times in one argument, and left alone when absent; the environment block has
the four variables; a submission for 1M tasks serialises to under 4 KiB.

**M2.1 `TaskArrayState` transitions.** Register gives all chunks queued;
grant moves chunks and refuses non-queued ones; complete with the right node
and attempt retires the chunk and adds the counts; duplicate completion is a
no-op; completion from the wrong node or a stale attempt is refused;
`requeue_node` returns only unfinished chunks and bumps their attempt; cancel
empties the queue; terminal status is computed from counts and
`max_failed_indexes`; failed-index overflow is counted, not stored.

**M2.2 `plan_grants`.** Tops nodes up to depth; lowest chunks first; never
grants the same chunk twice; a node with zero slots gets nothing; with an
empty queue it grants nothing; deterministic for the same input; property
test that after any sequence of plan/grant/complete/requeue every chunk is in
exactly one of queued, granted or done.

**M2.3 Size bounds.** A 1M-task array's JSON after full completion with 1%
sparse failures (10,000 failed indices) is at most 256 KiB; the number of
state transitions for a simulated run with one sync per second is at most the
number of seconds plus one.

**M3.1 Runner seam.** `FakeRunner` outcomes by index; `ProcessRunner` runs
`/bin/sh -c 'exit 3'` and `true` (portable, host-only processes, no runtime),
kills on timeout, truncates output to the head and tail bound.

**M3.2 Pool.** At most `concurrency` tasks run at once (the fake runner
records the high-water mark); retries up to `max_attempts` with the backoff
schedule (paused tokio clock, driven manually, per the start_paused rule);
spawn errors don't retry; cancellation stops admission and kills in-flight
tasks; chunk completion is emitted once, after the last task of the chunk.

**M3.3 Ledger.** Records round-trip; a torn final block is ignored on replay;
a corrupt CRC in the middle stops replay with an error rather than skipping;
group commit fsyncs at most once per interval (counted through a seam);
replay reports which tasks of a held chunk need rerunning.

**M4.1 In-process acceptance.** `tests/suite/task_array_million.rs` (a module of the portable suite binary) drives the M2
state machine and three M3 executors with `FakeRunner` through 1,000,000 tasks
with 1% first-attempt failures and one simulated node loss mid-run, and
asserts: every index is accounted for exactly once in the final counts, the
number of state transitions stays within the per-second bound, and the final
state is within the size bound. It runs in a few seconds in a debug build, so
it belongs in the portable suite. (M2.3 already runs the leader half of this, with no executors, as a unit test.)

**M4.2 Criterion benches.** See below.

## Benchmarks

**In-process (M4, no cluster, `make bench-task-arrays`):** a
`benches/task_arrays.rs` Criterion suite:

- `index_set/insert_sequential_1m` and `index_set/insert_sparse_10k`;
- `state/plan_and_complete_1m` with 3 and 200 nodes (control-plane cost of a
  whole 1M-task run, no processes);
- `state/json_encode_1m` (snapshot cost);
- `executor/fake_64k` (pool overhead per task with a zero-cost runner:
  this is the ceiling the process runner can't beat);
- `executor/process_true_256` at concurrency 1, 4 and 16, spawning
  `/usr/bin/true` (the fork/exec floor on the machine running the bench; it's
  a benchmark, not a test, so it's never part of `make test`).

Each prints tasks per second and bytes per task, so the headline can be
predicted before any cluster exists.

**Real clusters (M8, after the soak and after M5):** a
`relish bench batch-throughput` suite in `src/testkit/bench/suites.rs` that
submits a 100k and a 1M array against a leased namespace, records wall time,
rate over time, start-lag percentiles and the `--cost` block, and writes them
to the usual bench report. Run it on three quickstart VMs (laptop, mains
power, record `pmset -g therm` before and after) and on three 16 vCPU Linux
VMs. For the comparison, run the same work as a Kubernetes Indexed Job
(`completions: 100000`, `parallelism` at the per-node pod limit) on a kind or
k3s cluster of the same size, and publish both scripts.

## Risks

- **Fork/exec inside Apple's Virtualization framework** is the unknown the
  laptop headline rests on. M0 measures it before anyone writes a tour step.
- **Leader CPU** for syncing many arrays at once. Mitigation: one sync loop
  for all arrays, not one per array; the RPC carries every active array for
  that node.
- **At-least-once** is a change from the owner-helper model's adoption across
  restarts. It has to be stated plainly in the manual and in `relish run
  --batch --help`.
- **A fresh cluster for 0.2.0.** 0.1.x clusters can't roll to 0.2.0. That's
  the pre-1.0 policy, and the release notes and manual must say so plainly.
- **Security of host processes.** Process workloads have no mount isolation.
  The demo allowlists exactly one binary, never a shell, and says what it
  trades away.
- **Straggler tail.** Without speculation, the last chunk sets the wall time.
  Small chunks near the end (split the last few chunks) are a cheap fix if M8
  shows a long tail.
- **Ketchup schema change** for `task_index` may not be additive for archived
  Parquet readers; the fallback is to carry the index in the message.

## Progress checklist

### M0: measure (after the soak)

- [ ] fork/exec rate of a trivial binary in a quickstart VM at 1x/2x/4x/8x vCPU
- [ ] VM disk fsync latency and Raft commit rate
- [ ] decide the laptop headline (1M or 100k)

### M1: data model

- [x] M1.1 `IndexRangeSet` with unit and property tests
- [x] M1.2 `TaskArraySpec` validation and chunk maths
- [x] M1.3 template expansion and task environment

### M2: leader state machine

- [x] M2.1 `TaskArrayState` transitions, fencing, requeue, cancel
- [x] M2.2 `plan_grants` policy with the partition property test
- [x] M2.3 size and write-count bounds

### M3: node executor

- [x] M3.1 `TaskRunner` seam: `FakeRunner` and `ProcessRunner`
- [x] M3.2 worker pool: concurrency, retries, timeouts, cancellation
- [x] M3.3 ledger with group commit and replay

### M4: in-process proof

- [x] M4.1 `tests/suite/task_array_million.rs` acceptance (100k in the portable suite; 1M passed locally in 30.6 s debug, 32,700 tasks/s with `FakeRunner`)
- [x] M4.2 `benches/task_arrays.rs` and a `make bench-task-arrays` target (smoke-tested with `cargo test --bench task_arrays`; release numbers not yet recorded, see below)

### M5: wiring

- [x] compatibility decision recorded (28 September 2026): no gate, bump and start fresh
- [x] bump `protocol` and `state` in `src/compatibility.rs` (34/49 to 35/50); test that peers and state from before are refused
- [x] `RaftRequest::TaskArray(TaskArrayWrite)` and `DesiredState::task_arrays` (`src/meat/task_array_store.rs`; ids share the batch counter)
- [x] node side of the sync (`src/bun/task_array_node.rs`): runs held chunks through `TaskPool` with the ledger, fences by attempt, resumes from the ledger after a restart (`TaskPool::resume_chunk`), refuses binaries off the allowlist and nodes with `mount_isolation` (M7 decides), keeps failed tasks' output, deletes an array's files once the cluster stops listing it
- [x] `POST /v1/batch/array`, `POST /v1/batch/array/sync`, `POST /v1/batch/{id}/cancel`, `GET /v1/batch/{id}` for arrays (`"kind": "array"`, per-node slots, counters and refusal reasons), `GET /v1/batch/{id}/results`, `GET /v1/batch/{id}/tasks/{index}/logs`, plus the two node-local reads (`src/bun/task_array_api.rs`, authz matrix rows, scope checks on submit, cancel, results and logs)
- [x] leader sync loop with requeue on silence (`src/bun/task_array_leader.rs`: one `Sync` entry per array per tick, grants planned against the state with that tick's results applied; standalone nodes use an in-memory copy). Portable suite `tests/suite/task_arrays.rs`: 100,000 fake tasks through a single-node council in 51 Raft entries; real processes standalone and with failures, results and logs; cancel; refusal reasons
- [x] `relish run --batch --count` (with `--chunk`, `--max-attempts`, `--max-failed`, `--timeout`, `--concurrency`, `--env`, args after `--`; `--help` states at-least-once), `relish batch cancel`, arrays in `relish batch-status` (counts, chunks, failed indices, per-node slots or refusal); `relish batch FILE` keeps working beside the new subcommands
- [x] multi-node in-process cluster test: `tests/cluster_task_arrays.rs` in `make test-cluster` (three wired nodes, 100,000 fake tasks submitted through a follower, a follower killed mid-run and requeued after a 3 s silence timeout). Passed in CI run 36486408185 (`full-ci`, "multi-node cluster" job, 28 September); also passed locally on 3 October 2026 (100,000 tasks, 27 Raft entries, 27.63 s)

### M6: views and data

- [ ] `batch-status --watch` and `--cost`
- [x] `relish batch logs --index`, `relish batch results [--failed] [--limit]` (landed with M5: failed tasks' output kept per node; results merged from every node's ledger)
- [ ] Ketchup sampling, Mayo counters
- [ ] TUI batch view, dashboard card

### M7: host processes on quickstart nodes

- [ ] per-workload runtime choice or hybrid runtime, with the isolation note

### M8: real numbers and docs

- [ ] `relish bench batch-throughput`
- [ ] quickstart and cloud runs recorded under `docs/qualification/`
- [ ] Kubernetes Indexed Job comparison run, scripts published
- [ ] tour step, manual chapter, book section, whitepaper Q8 and
      scheduler-meat 5.2 rewritten to match what shipped

# Plan: task arrays and a 100,000-job tour step

Status: proposed, after 0.1.0. Nothing here blocks the release.

## Why

The five-minute tour ([`docs/manual/08_five-minute-tour.md`](../manual/08_five-minute-tour.md))
never shows jobs or processes that run straight on the host, and those are
where Reliaburger can do something Kubernetes's object model makes expensive:
very many short tasks, each tracked individually.

On Kubernetes, running each task as its own Pod means several API writes
(create, bind, status updates, delete) persisted in etcd, a scheduler decision
and a sandbox start, typically about a second each (the published startup SLO
is p99 of 5 s excluding image pulls). There are at most 110 pods per node, and
the large-cluster guidance stops at about 150,000 pods. Kubernetes does have
answers: Indexed Jobs, Kueue and Volcano for queuing, Argo for workflows,
Armada for very large batch volumes, and above all the work-queue pattern (a
few workers draining Redis). All of them either create one Pod per task or
hand task tracking to user code. Check these figures against the current
upstream documentation before publishing any comparison.

So the claim we want to be able to make is precise:

> 100,000 jobs, each its own process, retried and logged individually by the
> orchestrator, in about a minute on your laptop.

Not "Kubernetes can't run a million jobs", which is false as stated, and not
"a million tasks packed into a thousand processes", which a work queue does on
Kubernetes too.

## Where we are

A code reading on 25 September 2026 (not yet measured) found the current
batch path won't get there:

- **Submission.** `relish batch` sends one `[job.X]` table per job with its
  full spec ([`src/config/job.rs`](../../src/config/job.rs)); there's no count
  or index. `POST /v1/batch` uses axum's default 2 MiB body limit
  ([`src/bun/api.rs`](../../src/bun/api.rs)), which is roughly 8-10k jobs.
- **Scheduling.** `schedule_batch` has no queue: whatever doesn't fit now is
  `Unschedulable`, which is terminal
  ([`src/meat/batch.rs`](../../src/meat/batch.rs)). Jobs with no resource
  request stop at 10,000 per node.
- **Quadratic paths.** `batch_submit_handler` matches assignments to jobs with
  nested `find`s; `BatchRecord::report` searches linearly on every report; the
  node's `run_jobs_and_watch` and the leader's `watch_batch` scan all pending
  jobs every tick, the latter with one HTTP request per pending remote job
  ([`src/bun/batch.rs`](../../src/bun/batch.rs),
  [`src/meat/batch_tracker.rs`](../../src/meat/batch_tracker.rs)).
- **Raft.** `BatchRegister` holds every per-job record in one entry, each
  finished job is its own `BatchJobUpdate` write, and each report clones the
  whole desired state. That contradicts the whitepaper's Q8 ("the Raft log
  records only batch-level decisions") and
  [`docs/design/scheduler-meat.md`](../design/scheduler-meat.md) §5.2.
- **Node.** Every job state change rewrites and fsyncs the node's job
  checkpoint, which validates every recorded job; the checkpoint is capped at
  16 MiB and batch jobs are never retired, so a node stops admitting jobs after
  roughly 40-50k in its lifetime. Each process job also gets a durable owner
  helper (a bun re-exec) and two log files
  ([`src/bun/agent.rs`](../../src/bun/agent.rs),
  [`src/bun/jobs.rs`](../../src/bun/jobs.rs),
  [`src/grill/process_owner.rs`](../../src/grill/process_owner.rs)).
- **Host processes on quickstart nodes.** Quickstart runs
  `bun --runtime runc` ([`src/relish/quickstart/provision.rs`](../../src/relish/quickstart/provision.rs)),
  and a node has one runtime, so an `exec`/`script` job's `proc-grill:host`
  root ([`src/grill/oci.rs`](../../src/grill/oci.rs)) looks like an image
  reference to runc. Host-process jobs almost certainly can't run on a
  quickstart node today; confirm on a cluster.
- **Views.** `relish batch-status` already aggregates counts, but `relish
  status` and the TUI jobs view list every instance and re-poll everything
  every 5 s, and the web dashboard has no job views.

Estimated throughput today: 10-30 jobs/s as runc containers, 60-150/s as
processes (where available).

## Design

**Task arrays.** A batch gets `count` and an `{index}` placeholder in its
command, so 100,000 tasks are one small request:

```sh
relish run --batch hello --count 100000 --exec /usr/bin/printf -- 'task %s\n' '{index}'
```

The leader assigns index ranges to nodes and queues what doesn't fit instead of
refusing it. Raft records the batch spec, the per-node range allocations and
one completion per chunk (say 1,000 tasks), plus failed indices as compact
ranges. Heterogeneous batches keep today's per-job path.

**A node executor for short tasks.** A bounded worker pool spawns each task
directly: no owner helper, no per-task checkpoint record. Durability is a
per-chunk append-only log with group commit (one fsync about every 100 ms).
That gives at-least-once execution after a crash, which we document, and it
deliberately gives up adoption across an upgrade for tasks this short. Retries
happen in the pool. Output goes to Ketchup with a `task_index` column, from a
shared log per chunk.

**Counters, not records.** Each node keeps atomic counters and a mergeable
latency sketch (HDR histogram or DDSketch) per batch, and adds a small
`batch_progress` entry to its `StateReport` every second
([`src/reporting/types.rs`](../../src/reporting/types.rs)). The leader's
aggregator merges them. Mayo exports the same counters labelled by batch and
node, never by task, so the series count stays fixed.

**Live views.**

- `relish batch-status ID --watch`: pending, running, succeeded, failed,
  retried; tasks per second over 1 s and 10 s; success and retry rates; p50,
  p90 and p99 for queue-to-start and run time; a per-node table; the top exit
  codes with a sample of failing indices.
- A TUI batch view on the existing stream channel
  ([`src/relish/tui/`](../../src/relish/tui)), fed by a server-sent events
  endpoint that pushes a delta every second.
- `relish status` groups by app and batch by default, with `--all` for every
  instance. The TUI jobs list gets filtering and a failed-only mode.
- A batch card in the web dashboard.

**Host processes on quickstart nodes.** Let a node run process workloads next
to runc, either through a hybrid runtime or a per-workload runtime choice.
Mount isolation for process workloads isn't implemented, so the demo
allowlists exactly one binary (never a shell) and says what it's trading away.

## Phases

| Phase | Work | Rough effort |
|---|---|---|
| P0 | Measure first: a `relish bench batch-throughput` suite covering raw fork/exec rate of `/usr/bin/printf` in a Lima VM (bare, and at 2x/4x/8x vCPU concurrency), today's batch path at 1k/5k/10k jobs, and Raft commits per second on Lima's disk | 2-3 days |
| P1 | Host processes on quickstart nodes, with the isolation decision | 3-5 days |
| P2 | Task-array batches: count and index, range allocation, queuing, chunk-level Raft writes, no quadratic scans, per-node progress instead of per-job polling | 1.5-2 weeks |
| P3 | The fast node executor: worker pool, group-commit log, in-pool retries, chunked Ketchup output | 1-2 weeks |
| P4 | Counters and views: `batch_progress`, aggregation, SSE, `batch-status --watch`, TUI batch view, dashboard card, Mayo metrics | 1-1.5 weeks |
| P5 | Tour step, manual, book chapter, and the whitepaper Q8 / scheduler-meat §5.2 fix | 2-3 days |

About five to seven weeks for one engineer.

## The tour step

```sh
relish run --batch hello --count 100000 --exec /usr/bin/printf -- 'task %s\n' '{index}'
relish batch-status 7 --watch
relish logs hello --index 4242
relish          # press b for the live batch view
```

Some tasks should fail on purpose (for example, one in a hundred exits 1), so
the retry and success rates show real numbers. A two-line note says what the
same run means on Kubernetes.

## What decides the headline

Publish "100,000 jobs in about a minute" only if P0 plus a prototype sustains
at least 2,000 tasks per second across the three quickstart VMs. Move to "a
million jobs in under five minutes" only at 5,000 per second or more with p99
scheduling lag under a second. Measure on a laptop that isn't throttling, and
record the variance, because thermal limits will move the number.

## Risks

- Fork/exec cost inside Apple's Virtualization framework is the unknown the
  whole headline rests on; P0 exists to measure it.
- Leader CPU for aggregation at thousands of reports per second.
- Crash semantics: at-least-once for short tasks is a real change from the
  owner-helper model and needs to be stated plainly.
- Running an unisolated binary as root in a demo sends the wrong security
  message unless it's scoped and explained.

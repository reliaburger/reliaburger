# Batch jobs

Development preview for 0.2.0. Build the PR #266 binaries and use a fresh
cluster: protocol 48 and state 65 change the control messages, snapshots and
worker ledgers. Production submissions require a council so definitions and
identities survive restart; standalone in-memory execution is a test harness.
The published 0.1.5 binaries do not have these commands.

## Submit compact work

A task array is a template and a count. The cluster assigns chunks to eligible
nodes; each node starts tasks as CPU and memory become available. A chunk of
1,000 tasks does not reserve 1,000 simultaneous executions. Applications and
batch attempts share the same node resource accounting. Existing app commitments
remain reserved while an app starts, runs or retires.

For an image in the cluster's registry:

```sh
relish run --batch render --count 10000 --image renderer:v1 --cpu 250m-500m --memory 256Mi-512Mi -- /renderer --frame {index}
```

CPU and memory use the same request–limit syntax as apps. Requests govern local
packing; rootful Linux containers enforce limits. Omitted values mean 1 CPU and
64 MiB, including the limits. A profile that cannot run is shown as refused in
status. Images are bound to a digest at submission and must pass the cluster's Pickle
and upstream trust policies, including required cosign signatures.

Every task receives `RELIABURGER_TASK_INDEX`, `RELIABURGER_TASK_COUNT`, `RELIABURGER_BATCH_ID` and `RELIABURGER_TASK_ATTEMPT`.
Its identity is the array ID and index; retries keep that identity. The job image and command apply to
all indexes; build separate profiles when resources or commands differ.

## Mix resource profiles

The homepage burger example contains real CPU and memory work in the same image
as the web service. After building and applying `burger/burger.toml`, submit
`burger/jobs.toml`:

```sh
relish --output json batch submit burger/jobs.toml
relish batch watch 1
relish batch results 2 --failed --limit 20
relish manual batch
```

Use the actual parent ID returned by submission in place of `1`. Watch shows
child array IDs; use the small profile's ID in place of `2` for task detail.
The example has 1,000 small tasks requesting 100m CPU and 32 MiB each, and 64
large tasks requesting 1 CPU and 128 MiB each. The node packs actual attempts
beside the web service, respecting both resource dimensions. Submission commits
all profiles atomically; cancelling the parent cancels all profiles.

A TOML manifest has `name`, `namespace` and `[[cohort]]` entries. Each cohort
has a unique DNS-label `name`, `count`, optional array policies and a `[cohort.template]`
job specification. There are at most 16 profiles and 16,777,216 total indexes
per submission. Names and namespaces are DNS labels; each template is at most
16 KiB. Use `relish batch submit burger/jobs.toml --dry-run` for local validation.
The cluster admits at most 64 active profiles. Reduce chunk
size for expensive tasks and straggler tails; larger chunks amortise dispatch
and consensus overhead. Chunk size defaults to 1,024 and is capped at 65,536.

## Observe summaries and inspect selected tasks

`relish batch watch ID` polls once per second. Status and the dashboard show
counts, backlog, retries, resource profiles and accepted success rates. Rates
count unique outcomes accepted by the cluster, rather than attempts started.
They begin as unknown and reset after leadership changes or stale samples.
Accepted counts arrive when chunks are acknowledged, so a one-second rate can
alternate between zero and a burst. Use a sustained interval for capacity planning.
Duration histograms merge across workers; watch displays p50, p95 and p99 bucket
upper bounds for final attempts. They exclude queueing and earlier retries.
`relish --output json batch-status ID` returns the machine-readable summary.
`GET /v1/batch/summaries` lists retained parent summaries and standalone arrays,
filtered to the caller's scope. Views and metric labels do not enumerate tasks.

```sh
relish batch results 2 --index 42
relish batch results 2 --failed --limit 20 --after 4095
relish batch logs 2 --index 42
relish batch cancel 1
```

Result pages contain at most 1,000 rows, examine at most 4,096 indexes and
contact at most eight workers. A page can end earlier when tiny chunks span
more workers. An
empty failure page can still contain `next_after`: continue with that cursor.
Only the worker and grant whose completion was accepted can supply an outcome
or failed-task output. Unreachable workers are explicit; successful tasks keep
no output. Control reports keep exact failure counts and at most 256 index
ranges per chunk; indexed detail remains available beyond that preview.
Failure output retains a bounded head and tail. Never interpret
missing detail as success. A missing or corrupt result index is an explicit
error; rebuilding it is worker recovery, rather than an unbounded page read.
Use the child array ID for results and logs. Each profile index has a 1 MiB
cache budget; more detail traffic may require additional disk reads.

Detail remains on the worker disk. Later submissions prune detail older than
one hour **after completion** and keep the newest 20 terminal submissions.
This is pruning on registration, rather than a background expiry timer; detail
can remain readable longer while no new work is submitted. For a mixed manifest,
the clock starts when its last profile finishes; profiles are pruned together.
Permanent worker or disk loss can lose
individual results even when accepted aggregate counts remain replicated.
Export needed results before retention expires. Cancelling counts never-started and interrupted
tasks as not run, separately from terminal failures and drains active runtime owners before releasing
capacity. An abandoned attempt quarantines its capacity until startup recovery;
a dropped future cannot free an uncertain execution's request. Retries release
capacity during backoff.

## Execution and capacity limits

Execution is at least once. A lost worker or a crash before a durable outcome
can rerun a task, so external effects need a stable business idempotency key.
Array ID and index identify a task within this cluster history; backup rollback
or a new cluster can reuse those IDs. Grant fencing prevents accepting an obsolete result; it cannot undo
an external effect. Submission timeouts have an uncertain outcome: check the
retained summary list before submitting again, since a new submission creates
new task identities. Workers repair an incomplete ledger tail before appending,
commit incremental outcomes in groups and acknowledge chunks only after
outcomes and their result index are durable. Storage failure cancels local work
and refuses further grants until the node is repaired and restarted.

After council disaster recovery advances the recovery epoch, workers with old
array directories stop their attempts and persist a refusal. Old results and
grant fences are preserved. Archive that worker data and re-enrol with fresh
worker data before resuming batch execution; a restart alone cannot clear the
refusal. Fresh workers can join the recovered epoch. Ordinary leader failover
within the same epoch continues to reconcile existing results.

Image tasks require the rootful Linux owned runtime. Runtime slots reuse cached
image layers and bounded instance identities per namespace; each attempt still has its own
container launch and fresh temporary scratch space. The image root is read
only. Container tasks have a 1 MiB limit per regular output or scratch file
(`RLIMIT_FSIZE`), so logging cannot fill the disk without bound. GPU jobs, encrypted task environment, scripts, cron and dependency hooks
are refused. Host tasks require the owned process runtime, an absolute allowlisted
binary and disabled mount isolation. That backend cannot enforce CPU/memory
limits, so explicit resource ranges are refused. Prefer image tasks for shared
clusters.

With eBPF enabled, image tasks inherit their namespace before starting. They
may reach services in that namespace; cross-namespace services are refused.
Batch templates currently have no explicit cross-namespace grant or egress
allowlist fields. Without eBPF, namespace network enforcement is unavailable,
as it is for ordinary apps. The worker caches at most 256 namespace bindings,
evicts only idle bindings, and journals them for cleanup after old executors
retire at startup. Losing the source binding stops its original runtime owner.

Local packing is non-preemptive. Existing applications have reserved requests;
new deployments and rolling replacements also need free capacity. There is no
DRF tenant fairness or automatic app pre-emption of running jobs. Batch admission uses FIFO waiting so smaller tasks cannot repeatedly overtake
a larger waiting request. This can leave capacity idle behind a large request. Reserve service capacity and allow
rollout headroom when planning a mixed cluster.

100 million unique successes per day is about 1,158/s sustained. Provision for
bursts, retries, average task duration and the tightest CPU/memory dimension;
container startup, images, output, storage and network cost also matter. This
feature does not qualify that target by itself. A simulator benchmark and this
short demo are not a 24-hour throughput or reliability qualification.

Apps without explicit resource requests reserve zero, matching the existing
app scheduler. Set requests for services sharing a node with batch work, and
leave capacity for rollouts. Admission protects requests; it cannot guarantee
latency when workloads burst up to their limits.

Namespace app quotas currently govern ordinary app placement, not delegated
array resource usage. Scope checks still apply to submission and reads. Use
admission limits and dedicated capacity for mutually untrusted batch tenants
until tenant resource allocation is added.

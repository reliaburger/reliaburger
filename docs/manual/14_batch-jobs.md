# Batch jobs

Development preview for 0.2.0. Build matching development binaries and start a
fresh cluster: protocol 49 and state 66 change control messages and durable
state. Published 0.1.5 binaries don't have this lifecycle. A council replicates
cluster definitions and runs; standalone Bun persists the same state in private
`job-state/jobs.json` before acknowledging admission.

## One lifecycle, several triggers

A definition stores reusable work and policies. A run captures its revision and
trigger identity. Tasks and attempts belong to that run. An ordinary job has one
task; arrays add indexes to the same placement, resource admission, runtime,
result, retry and cancellation machinery.

```toml
[job.migrate]
image = "database-tools:v1"
command = ["/migrate"]
run_before = ["app.api"]

[app.api]
image = "api:v2"

[job.cleanup]
image = "database-tools:v1"
command = ["/cleanup"]
schedule = "0 3 * * *"
```

`relish apply jobs.toml` records a durable deployment operation. Required
`run_before` jobs must have accepted successful outcomes before applications are
published. Hook dependencies name applications in this apply and namespace;
arbitrary task dependency graphs aren't supported. Ordinary jobs are admitted
when application publication succeeds. A disconnected client or a new leader
can resume the operation from the same accepted run identities.

Ordinary TOML jobs get four attempts for known failures; deployment hooks get one
attempt, so a failed migration is not repeated automatically. Ordinary jobs have no implicit
runtime deadline; hooks have a 600-second deadline. An uncertain attempt outcome
becomes `Unknown`, retains ownership and prevents automatic replay, even after
worker restart or disappearance. A successful launch isn't evidence of successful
work. Explicit bulk submissions retain at-least-once replay, including a
one-element array. This is a retry policy on the common engine.

Cron uses five fields in UTC. Registration starts with the next matching minute.
The leader atomically claims the occurrence and admits its run. Default overlap
is `forbid`; JSON definitions can select `allow`. Both skip missed minutes, with
no catch-up queue. Skipped overlapping occurrences advance the cursor too.
Registration and observation cursors survive leader changes, result pruning and
clock rollback. A conservative definition with unknown ownership skips new
occurrences even when overlap is allowed.

```sh
relish jobs
relish jobs --definitions
relish --output json batch-status 42
relish batch cancel 42
relish stop cleanup
```

`jobs` shows bounded run summaries, backlog and accepted rates; `--definitions`
includes dormant schedules and revisions. Stop disables future occurrences and
cancels retained runs. Deleting a definition doesn't erase uncertain ownership.
Deployment inventory and cancellation use `/v1/deploys/operations` and
`/v1/deploys/cancel/OPERATION_ID`. Cancellation is durable; capacity and gates
remain held until workers positively retire their launches.

Inspect an `Unknown` summary's owner and grant digest before deciding to repeat
work:

```sh
relish batch replay 42 --node worker-2 --grant-digest DIGEST --acknowledge-side-effects
```

Replay acknowledges that external effects may happen again. The digest identifies
those exact unknown grants; repeating an old acknowledgement can't authorise a
later unknown attempt. The cluster service principal cannot make this decision.

Singleton status and logs use `run-ID`, a stable identifier independent of the
worker's reusable runtime slot. `relish status` reports a live singleton's PID;
indexed batches stay in summary views. Select `--instance run-ID` when several
retained runs share a logical name. Live logs follow the owning worker and
completed singleton output remains bounded by the task-output retention policy.
Apps, reusable job definitions and retained job runs cannot share the same name
and namespace across workload kinds. Delete the definition and wait for its run
receipts to be pruned before reusing that identity for an app.

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

`--count` defaults to one. Add `--schedule "0 3 * * *"` to register an array
schedule without an immediate run. Every task receives `RELIABURGER_TASK_INDEX`, `RELIABURGER_TASK_COUNT`, `RELIABURGER_BATCH_ID` and `RELIABURGER_TASK_ATTEMPT`.
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
or retained task output. Unreachable workers are explicit. Singletons keep bounded successful output as
well as failure output; larger arrays discard successful output. Control reports keep exact failure counts and at most 256 index
ranges per chunk; indexed detail remains available beyond that preview.
Retained output keeps a bounded head and tail. Singleton live stdout/stderr also
uses ordinary log capture under its logical name, namespace and `run-ID` selector. Never interpret
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

Explicit bulk execution is at least once. Ordinary jobs and hooks use
acknowledged replay for unknown outcomes. A lost worker or a crash before a durable outcome
can rerun a task, so external effects need a stable business idempotency key.
Array ID and index identify a task within this cluster history; backup rollback
or a new cluster can reuse those IDs. Grant fencing prevents accepting an obsolete result; it cannot undo
an external effect. Submission timeouts have an uncertain outcome. Reuse the same `Idempotency-Key`
for array, finite group or apply admission; the JSON definition endpoint uses
`request_id`. Changed content under a retained identity is refused. Dedupe lasts
while that receipt is retained; a new request creates new identities. Workers repair an incomplete ledger tail before appending,
commit incremental outcomes in groups and acknowledge chunks only after
outcomes and their result index are durable. Standalone admission reserves sparse progress within its 64 MiB store before
accepting work. Storage failure cancels local work
and refuses further grants until the node is repaired and restarted.

After council disaster recovery advances the recovery epoch, workers with old
array directories stop their attempts and persist a refusal. Old results and
grant fences are preserved. Archive that worker data and re-enrol with fresh
worker data before resuming batch execution; a restart alone cannot clear the
refusal. Fresh workers can join the recovered epoch. Ordinary leader failover
within the same epoch continues to reconcile existing results.

Arrays containing multiple image tasks require the rootful Linux owned runtime.
Singletons use the configured owned container or process runtime, preserving its
supported limits. Unsupported explicit CPU/memory limits are refused on process
and rootless runtimes. Slots reuse cached image layers and bounded instance
identities; every attempt gets a fresh launch. Singleton container roots are
writable. Larger arrays use a read-only root, temporary scratch and a 1 MiB limit
per regular output or scratch file (`RLIMIT_FSIZE`).

On Linux, Bun's `auto` and `mixed` modes select the backend per workload.
Jobs select `runtime = "runc"` (the default), `runtime = "process"`, or
`runtime = "shared-runc"`. Process jobs use `exec` or `script` and refuse
`image`. Container jobs require `image` and refuse host `exec`/`script`.
`relish run` exposes the same choice with `--runtime`.

On Linux, `bun --runtime mixed` enables both owned backends. `--runtime auto`
can detect both when runc is available. Explicit `--runtime runc` is
container-only and refuses host jobs; `--runtime process` is host-only.
Host commands still need the executable allowlist and
`mount_isolation = false`. Explicit host CPU/memory limits remain refused;
container capabilities do not enforce limits or isolation on host commands.

For example, after allowing `/usr/bin/printf` on a process-enabled node:

```sh
relish run --runtime process --batch hello --count 1000 \
  --exec /usr/bin/printf -- 'task %s\n' '{index}'
```

Omitting `--runtime process` refuses the host command. A process job's default
request reserves one CPU and 64 MiB beside applications. This is admission
accounting; host commands have no hard CPU or memory limit. The raw VM baseline
bypasses this reservation as well as durable task ownership and outcomes.


## Reusable containers

For short image commands, select `runtime = "shared-runc"` explicitly
in the job or profile template. The default remains `runc`.

```toml
[job.prepare-record]
image = "registry.example.com/dataset-tools@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
runtime = "shared-runc"
command = ["/usr/local/bin/prepare-record", "{index}"]
cpu = "100m-500m"
memory = "64Mi-256Mi"
```

Image admission still pins the image and verifies its trust policy. A worker
keeps at most 32 compatible containers. The compatibility key includes the
pinned image, namespace, live credentials and resource profile. Each container
runs one command at a time, as a separate process. Jobs, arrays, schedules and
hooks keep their common run identities, retries and durable results.

The command is born in its task cgroup, with independent CPU and memory limits.
It receives its own mount and IPC namespaces, empty environment before explicit
image/job variables, and fresh 16 MiB `/tmp` and `/dev/shm` filesystems. Its root
is read-only; regular files have a 1 MiB size limit. No command receives the
helper's control descriptors, delegation identity or capabilities. The verified PID-1 helper kills and reaps every descendant; Bun then proves
the task cgroup empty before another command can use the slot.

PID and network namespaces remain shared with the executor. Use fresh
containers when that stronger separation matters. Reuse does not keep your
Python interpreter, model or application process alive between commands.
Resident inference workers are a separate feature (#641).

An idle container retains its profile request plus 10 millicores and 8 MiB for
the helper. It yields to waiting requests and expires after one idle second.
A timeout, cancellation, lost policy or uncertain receipt retires the whole
container before its resources can be used again. Waiting for a compatible
slot or resource admission does not consume the command's attempt timeout;
cold preparation counts after the full profile is reserved. Abandoned ownership remains
quarantined until recovery retires the original generation. Host execution,
rootless runtimes and GPU requests cannot select this mode. Kubernetes export
refuses it because an ordinary Job cannot preserve this contract.

Summaries include `runtime` and `idle_executor_reservation`. Host jobs report
`process`. On rootful Linux runc, memory-limited image jobs disable swap
in both fresh and reused execution. A job exceeding its RAM limit can be OOM-killed; it cannot silently
spill into swap and consume unaccounted disk I/O. Choose the memory range for
the actual working set, including model weights.

Compare accepted-success rates over the whole run and service
latency alongside launch rates; container reuse alone does not qualify the
100 million jobs/day target.

Authorised scripts and encrypted environment values use the common path. Workers
decrypt with live keys for the job's namespace. Decrypted configuration stays
execution-local; captured output uses normal scoped retention. Host jobs require the owned process runtime, an absolute
allowlisted binary (or `/bin/sh` for scripts) and disabled mount isolation.
That backend cannot enforce explicit resource ranges. GPU jobs remain refused;
GPU placement is tracked separately in #359. Test-lease workloads retain their
lease-aware admission; unowned lease namespaces and images are refused here.

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

## API admission shapes

`POST /v1/jobs/runs` accepts `name`, optional `namespace`, a `definition` and a
manual `request_id`. A definition has `template`, optional `tasks` (count one by
default), optional `cron` (`expression`, `overlap`, `missed`) and
`replay_unknown` (false by default). `tasks.task_timeout_secs = 0` means no
deadline. JSON task-policy defaults remain three attempts and 600 seconds;
set them explicitly when submitting a side-effecting job.

`POST /v1/batch` admits up to 64 distinct named singleton jobs atomically,
returning a parent ID and each `run-ID`. `assigned` reports accepted jobs,
which may still wait for a worker; use summaries for queued or refused work.
Group repeated commands/resources into compact manifests rather than sending a
separate definition per index.

`POST /v1/batch/array` and `/v1/batch/manifest` admit compact repeated work.
`GET /v1/jobs/definitions` lists scoped definitions without secret templates.
`POST /v1/jobs/definitions/NAME/NAMESPACE/disable` disables future occurrences.
`POST /v1/jobs/runs/ID/replay` requires `node`, `grant_digest` and
`acknowledged=true`, under a user credential with current workload permissions.

`relish run --batch` exposes the same choice as manifests:

```sh
relish run --batch frames --image registry.example/frames@sha256:DIGEST \
  --runtime shared-runc --count 100000 --cpu 100m --memory 32Mi \
  -- /app/frame '{index}'
```

Replace `DIGEST` with the image's full SHA-256 digest. The default is
`runc`. Host `--exec` commands cannot select container reuse.

The watch summary names the execution mode and separates verified active
commands from other in-flight attempts (waiting for resources, preparing or
settling). Reusable executors count a command from its authenticated start
receipt until positive descendant cleanup. Counts reflect the last node sync;
unsupported backends show `unknown`, rather than treating a runtime launcher
or a resource waiter as a running command. A mixed summary is unknown if any
executing profile cannot supply that receipt. Detail remains indexed and bounded.

Recent accepted-success rates include their observation interval: a completed
chunk can produce a reporting burst. The since-admission average includes the
whole admitted run and freezes at its accepted terminal timestamp. It uses UTC
timestamps at one-second precision; sub-second runs have no such rate. Throughput
qualification separately times submission through accepted completion with a
monotonic clock, including work before admission acknowledgement.

To measure and record an actual submission beside a live application:

```sh
python3 scripts/demo/measure-jobs.py jobs.toml \
  --relish 'relish --endpoint https://node.example:9443' \
  --service-url https://app.example/health \
  --warmth warm-images-cold-executors --output job-measurement
```

The command writes the submitted manifest, raw summary/service samples, selected
indexed outcomes, a report and an asciinema recording with the actual pauses.
The warm/cold label is an operator declaration: record cache preparation beside
the evidence. `--observe-dir` takes node-local storage directories and reports
incomplete observations when its bounded traversal is exhausted. This harness
measures full dispatch; use the same workload, limits and concurrency for the
separate bare-process, direct-container and direct-durable baselines. The report
never qualifies 100m/day from a short recording.

`--observe-pid PID` records Linux RSS and peak RSS for at most eight original
process identities. A changed start time or unavailable process produces an
incomplete observation. These are selected processes, not total node or
container memory; record cgroup working sets separately when qualifying limits.
Storage observations also become incomplete when atomic renames race the scan.

The three-tier landing-page recorder adds a separate million-process VM
baseline, then submits 1,000 fresh containers, 10,000 shared containers and
10,000 host jobs. Run it only on a task-owned Linux measurement node with
matching optimised `bun`, `relish` and `job-throughput` binaries:

```sh
python3 scripts/demo/record-job-tiers.py \
  --binaries /absolute/immutable-binaries --relish /absolute/immutable-binaries/relish \
  --image registry.example/busybox@sha256:DIGEST \
  --host-binary /absolute/matching-unpacked-rootfs/bin/busybox \
  --service-url http://application/health --observe-pid BUN_PID \
  --observe-dir /absolute/node-data --output /absolute/new-recording
```

The recorder verifies that the host binary matches the pinned-image baseline.
Provide CLI credentials privately. Enable `mixed` on Bun, allowlist that binary,
and set `mount_isolation = false` for these host jobs. Give the baseline its
own node subnet identity, as with the direct driver below. The casts retain
actual pauses, including warm-up and indexed verification. Per-tier reports
time submission through accepted completion. The host tier's default request
can reduce concurrency below the requested 27; it doesn't bypass admission.
The recorder uses the default maximum of three attempts and reports retries
separately from unique accepted successes. Failed diagnostics stay in the raw
evidence. Chapter jumps and labelled faster playback use the original cast;
measured durations always refer to real elapsed time.

For a continuous one-hour run after the demo, submit one bounded batch at a
time beside the same service:

```sh
python3 scripts/demo/qualify-jobs.py /absolute/new-recording/process.toml \
  --seconds 3600 --window 1 \
  --relish /absolute/immutable-binaries/relish --app-url http://application/health \
  --observe-pid BUN_PID --observe-dir /absolute/node-data \
  --output /absolute/new-hour-measurement
```

The runner counts unique accepted outcomes, starts another batch when one
finishes, and cancels its own remaining work at the cutoff. It reports service
latency, selected process memory and bounded storage observations. Keep other
benchmarks, builds and test traffic stopped. A complete hour can still miss the
rate target; it never passes the separate 24-hour qualification. To observe an
already-running submission without creating or cancelling work, use
`--existing-batch BATCH_ID`; that read-only mode subtracts earlier accepted
results and leaves the submission running afterwards. An idle remainder of the
window isn't continuous throughput evidence.

For the direct baselines, build the rootful Linux example with the same profile
as Bun and run each mode in a fresh directory on the qualification node:

```sh
cargo build --release --features ebpf --example job-throughput --bin bun --bin relish
sudo target/release/examples/job-throughput --path durable-reused \
  --root /tmp/job-baseline --bun "$PWD/target/release/bun" \
  --image registry.example.com/busybox@sha256:DIGEST \
  --count 10000 --concurrency 27 --service-url http://app.example/health
```

Use `bare`, `fresh`, `reused`, `durable-fresh` and `durable-reused` with the same
image, command, count and resource inputs. This driver uses `/bin/busybox true`
from that pinned image, including the same unpacked binary in its process
floor. Bare processes have no container limits or ownership/durability contract.
Direct runtime and durable worker modes omit Bun's live namespace supervision;
the public cluster includes it. Disclose those differences when interpreting
the gap. Run independent runtime allocators on separate qualification nodes or
with distinct node subnet identities; they cannot share one node address pool.
The directory and its ancestors must be traversable by the container user
namespace. Every original runtime owner is positively retired after timing.

For a separately provisioned task-owned persistent cluster, the recovery probe
uses a mixed-profile reusable manifest and a running application:

```sh
python3 scripts/demo/verify-job-recovery.py mixed-jobs.toml \
  --bun /opt/qualification/bun --relish /opt/qualification/relish \
  --config /opt/qualification/node.toml --pid-file /opt/qualification/bun.pid \
  --node-name worker-1 --service-name web --service-url http://app.example/health \
  --output recovery-proof
```

Set the ordinary `RELIABURGER_ENDPOINT`, `RELIABURGER_CA_CERT` and
`RELIABURGER_TOKEN` environment variables. The probe checks the original binary
and configuration before using a Linux PID handle to kill Bun during partially
accepted completion. It starts the same binary/configuration, updates the PID
file, verifies the application's original PID, resumed accepted outcomes and
selected indexes, and rejects a stale control version over verified node TLS.
The replacement remains running. Preserve its binary and ownership journals
until normal workload retirement. This proves a single-node crash; separate
multi-node and sustained fault cases remain necessary.


## Measured development results

The standalone landing-page recording measures a raw VM baseline and three
public job tiers on a four-vCPU, 8 GiB Ubuntu 24.04 aarch64 VM, with matching
optimised development binaries and pinned BusyBox `true` commands:

| Path | Completed work | Elapsed | Rate | Accepted retries |
|---|---:|---:|---:|---:|
| Raw VM processes | 1,000,000 exit statuses | 63.87s | 15656.6/s | Not applicable |
| Fresh containers | 1,000 accepted successes | 256.01s | 3.9/s | 0 |
| Shared containers | 10,000 accepted successes | 21.76s | 459.5/s | 0 |
| Owned host jobs | 10,000 accepted successes | 200.56s | 49.9/s | 0 |

All 21,000 public jobs completed. The separate million-process baseline counts
exit statuses; it omits admission, limits, durable ownership and task ledgers.
The whole rate gap cannot be called scheduling overhead. Real pauses remain in
the cast; chapter jumps and labelled playback speeds only change the view.
Container profiles request 100m CPU and 32 MiB with a one-core CPU limit. Host
jobs retain the default one-CPU/64-MiB reservation without enforced resource
limits, so admission can reduce actual concurrency below the requested 27.
The normal maximum of three attempts applies, and accepted retries remain
visible. The image is warm and shared executors start cold.

These results don't qualify 100m/day. Initial startup and inventory failures
remain in the diagnostics, and bounded storage scans cannot prove a global
bound when incomplete. Read the [raw evidence and reproduction](../qualification/2026-10-09-job-runtime-revision/README.md)
for the one-hour run, resource observations and remaining 24-hour, headroom,
fault and metadata-collection work. [Historical measurements and recovery proofs](../qualification/2026-10-08-job-measurements/README.md)
retain the earlier 50,000-job recording, real Bun crash and three-worker loss
experiments, including mixed profiles and the original application's PID.

### Continuous one-hour host run

The separate host runner completed 3600.00 seconds of continuous
public dispatch with one active 10,000-job batch at a time. It observed
**182,000 unique accepted successes (50.6/s)**,
0 terminal failures and 0 accepted retries.
The original service passed 3,490 probes with
0 failures; p95 latency was
0.99 ms and the maximum was
12.49 ms. Its original process start time and
durable generation still matched after the window. The runner cancelled only
its own remaining batch at the cutoff, with no cleanup failure.

Selected Bun RSS was 191.0 MiB at the first observation and
202.6 MiB at the last; its recorded peak reached 229.8 MiB. This
isn't total node/container memory. 117 bounded storage observations
were incomplete, so this run cannot prove a global storage bound. The node
logged 4 transient inventory-publication timeouts;
the raw warnings remain beside the samples. No timeout was counted as a
terminal task failure, and service probes continued, but that doesn't make
control-loop responsiveness fully qualified.

The achieved host rate remains below the 100m/day target. Both daily throughput
and overall qualification remain false. A completed hour doesn't substitute
for matched current-binary baselines, sustained container/cluster scaling,
headroom, faults and bounded metadata collection. Those remain in #640.

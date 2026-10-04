# Plan: resource-aware delegated jobs

Date: 4 October 2026. Owner: PR #266, release 0.2.0.
This supersedes the next-step order in the September million-jobs plan.
The maintainer has authorised the implementation, whitepaper, manual and
homepage demo following the 3 October review and the Mesos design discussion.

## Contract

A batch is queued work, not simultaneous execution. Every task has a stable
identity and resource request. Meat shares CPU and memory between applications
and batch workloads; Bun executes only within the granted budget. Arrays are
compact homogeneous manifests. Heterogeneous submissions group templates and
resource profiles without losing the identity of their constituent tasks.

Raft stores definitions, grants and accepted aggregate outcomes. Worker ledgers
record terminal outcomes incrementally with group commit. Completed chunks are
retransmitted until accepted; stale control messages cannot replace newer
assignments. Permanent worker loss and result retention must be explicit in
responses. Execution is at-least-once; effects require idempotency.

Default operator views show rates, backlog, latency distributions, capacity and
failure groups. Task detail is bounded, filtered and addressable. Metrics labels
must not grow with task IDs. Reports and queries have bounded memory, deadlines
and work independent of the full historical task count.

## Capacity planning

Size the cluster from the workload, rather than its task count. At 1,157.407
successes/s, a one-second mean runtime means about 1,158 concurrent tasks; a
one-minute mean means about 69,445. For each resource dimension, sum each
profile's arrival rate multiplied by its mean attempt duration and request,
then add application/system reservations, retries and burst headroom. Check
measured CPU-seconds, memory working sets and limits as well as declared
requests: request accounting alone cannot guarantee application latency.

Control-plane batching removes one scheduling transaction per task. It does not
remove image transfer, container startup/retirement, output capture, fsync or
index costs. Measure warm and cold launches, durable completion throughput,
straggler tails and service latency independently. The cheapest route to the
daily target depends on task size and cluster hardware; tenant fairness,
pre-emption and pooled task execution need separate design if measurement
shows they are necessary.

## Implementation order and proof

1. Convert the five review probes into regression tests. Repair torn-tail
   recovery, incremental durability, shared concurrency, stale assignments and
   authoritative result selection. Exercise storage failure before retirement.
2. Introduce CPU/memory request profiles and a shared node execution budget.
   Subtract application commitments; admit mixed profiles against the same
   ledger. Test two arrays, mixed task sizes, cancellation, retries, a changing
   application allocation and recovery without overcommit.
3. Support owned runtime execution on normal Linux quickstart nodes. Keep the
   host-binary allowlist and mount-isolation refusal for unisolated execution.
   Reuse the existing runtime lifecycle, image cache and resource enforcement;
   verify cancellation retires the process tree before releasing resources.
   Bind image tasks to their network namespace before launch; prove cross-tenant
   refusal and retirement after a lost source binding.
4. Add mixed-profile manifests and bounded admission, with durable ownership
   recorded before dispatch. Test validation, follower forwarding, scoped
   authorisation, partial failures, retries and result identities.
5. Wire mergeable duration histograms and timestamped summary rates; distinguish
   unique accepted terminal tasks from attempt counters. Add watch, indexed
   detail and cursor pagination. Test resets, duplicate reports, stale samples,
   filters and bounds. Expose the summaries through CLI, API and dashboard.
6. Add the architecture section to the whitepaper and scheduler design, a
   compiled manual page and book walkthrough. Update the homepage's tour,
   executable tour script and example workload together, with real batch work
   beside the existing application. Keep recordings honest about which binary
   and workload they demonstrate.
7. Run portable CI and matching cluster/runtime/demo gates. Record real-process
   throughput with workload, hardware, resources, retries and storage costs.
   Provide a reproducible sustained gate for 100 million unique successes/day
   (1,158/s plus headroom), concurrent applications, multiple profiles, failover,
   delayed messages and disk faults. A simulation or burst is not a 24-hour
   qualification; publish measured limits rather than claiming an unrun result.

## Design choices

Resource eligibility and ownership remain central; local packing avoids a global
placement transaction per task. Chunk size controls dispatch overhead, not
concurrency. CPU and memory requests drive placement; supported runtime limits
are enforced during execution. Fairness starts with bounded shared allocation
and reservations, rather than adding a public external-framework API.

The executor owns only valid grants. Control requests carry monotonic provenance;
workers persist fences. On restart, reconcile before assigning ambiguous work.
Detailed outcomes use explicit accepted-run provenance instead of inferring a
winner from a capped failure set. Incremental outcome writes do not block every
new task on a separate fsync; one bounded writer commits and acknowledges groups.

## Progress

The correctness, shared resource budget, owned runtime path, mixed-profile
manifests, histogram/rate summaries, indexed detail and dashboard are implemented.
The whitepaper, scheduler design, book and compiled manual describe the contract;
the matching burger manifest and executable homepage preview perform real work.
FIFO admission prevents continual overtaking by small tasks, with head-of-line
utilisation costs. Production definitions require a council; volatile standalone
state is confined to the test harness. An abandoned execution quarantines its
request until recovery instead of releasing uncertain capacity.

Portable, cluster and real Linux runtime gates have run. The actual Linux
homepage workload passed with all 1,064 successes while its service kept serving;
the separate recording discloses its measured elapsed time. Results and measured
limits are recorded in the
[qualification report](../qualification/2026-10-04-delegated-jobs/README.md).
The sustained harness records unique accepted successes alongside a serving app;
no 24-hour 100m/day result has been established.


## High-volume container jobs on the landing page

The demo should establish a built-in high-throughput execution capability and
measure its overhead, rather than optimise for a particular round task count.
Useful initial targets are 500,000 distinct accepted successes in 60 seconds
or one million in 120 seconds: both require about 8,333/s. Choose the volume
and hardware after measuring the full path. Neither target is an established
result. A short demo also cannot qualify 100 million successes over 24 hours.

Give this feature its own asciinema recording and a prominent section near the
landing page introduction, before the installation walkthrough. Lead with the
verified count, real elapsed time, workload and hardware, then show how to
reproduce it. Keep the existing general tour separate. The current 1,064-task
recording proves mixed-profile execution beside a serving app; retain its
honest caption until a high-throughput container run can replace it. Do not
relabel that recording or accelerate its playback into a throughput claim.

### Fair measurements

Use tiny deterministic work that depends on each task index. Each command must
actually execute, exit and produce its own durable terminal outcome. A million
iterations inside one submitted job would measure that job's inner loop. Verify
unique accepted task identities and inspect selected indexed outcomes; count
retries separately. Use release builds, pinned images and bounded output.

Measure the following stages on the same VM, workload and concurrency:

| Stage | What it establishes |
| --- | --- |
| Bare child processes | Process-launch throughput and the minimum execution cost without container isolation or durable accounting. |
| Direct container executor | The cost of the same child launches with the proposed container isolation, per-task limits and cleanup, without cluster dispatch. |
| Direct executor with ledger/index | The extra cost of durable per-task outcomes with the same group-commit and index settings as the complete system. |
| Full Reliaburger submission | Dispatch, ownership, resource admission and accepted council completions on top of that execution and storage path. |

Use the direct durable executor as the matched baseline for the complete path.
Report the overall added elapsed time and throughput ratio. Use profiling and
phase timings before attributing that whole difference specifically to the
scheduler; resource admission, ownership and completion reporting also cost
work. The host-process result alone cannot quantify container scheduling
overhead. Keep the service running and reservations identical for the matched
runs; repeat full-size runs and report their spread. Do not extrapolate from a
short launch burst or subtract timings collected on different machines.

Start the full-path clock before submission and stop only once the council
accepts all successes after worker ledger/index durability. Report image pull,
executor warm-up and cold-start timing separately, including a cold end-to-end
run. Publish wall time, build revision, VM resources, concurrency, resource
requests/limits, retries, peak memory, disk growth and service probe latency.
The recorded run must correspond to the report. Playback speed or editorial
cuts must never substitute for the measured elapsed time.

The first full primitive measurement launched one million tiny child processes
in 66.360 seconds on the existing 4-vCPU VM, without the orchestrator or durable
outcomes. A 20,000-process sample's faster 23,110/s rate did not survive the full
run, which averaged 15,069/s. That makes a longer demo worth investigating,
without establishing container or orchestrator throughput. Reproducible
evidence is in the
[qualification record](../qualification/2026-10-04-delegated-jobs/README.md#process-launch-feasibility-for-the-million-job-demo).

### Execution contract

The recommended path to investigate is a bounded pool of long-lived owned
container executors, with a separate child process for each command task. This
must be a built-in batch execution mode, with no separately installed framework,
user-managed queue or application worker protocol. Compatible executors share
only a namespace/trust boundary, pinned image, credentials, mounts, security
settings and resource profile. Chunk size still controls dispatch rather than
per-task resource admission; mixed requests use distinct compatible profiles
and are packed against the same application/batch capacity budget.

A task running inside a reusable container and a fresh OCI container per task
have different isolation contracts. State that distinction in the manifest,
manual and recording. Preserve the existing fresh-container path for workloads
requiring it; do not silently substitute shared container isolation. Benchmark
that path separately with its stronger lifecycle cost. A large count of
container-backed tasks must not be described as that many fresh containers.

Build and initialise the executor context once, then retain per-index admission,
exit status, timeout, cancellation and retry identity. Prove child-tree
retirement and clean scratch/environment before another task uses the slot.
Enforce supported per-task limits independently; an executor-wide cgroup alone
cannot provide every child with an independent limit. Refuse incompatible
specifications rather than silently weakening them. Account for idle executor
memory. Uncertain executor ownership keeps resources quarantined; restart
fences and retires the old executor before replaying uncommitted work. Keep the
existing durable outcomes and accepted-owner/grant protocol.

An explicit persistent worker protocol could remove process startup as well,
but changes the command contract: crashes can interrupt several tasks, state
can leak between tasks, and killing one task may require retiring the worker.
Keep this opt-in and outside the initial process-per-task container demo.

The Mesos idea to borrow is a reusable executor that manages many tasks, rather
than equating every task with a new container. Mesos also permits custom
executors without a one-to-one task/process relationship; its shared-resource
and coupled-failure semantics should not replace our independent retry contract
without an explicit decision. See the
[framework guide](https://mesos.apache.org/documentation/latest/app-framework-development-guide/)
and [workload isolation guide](https://mesos.apache.org/documentation/latest/running-workloads/).
Executor reuse remains planned work. The current reusable identity slots still
recreate the runtime per attempt; their measured performance does not establish
small execution or scheduling overhead.

### Recording and operator tooling

The main recording should show one manifest submission, the baseline result,
a live accepted-success count, whole-run average rate, recent rate, backlog,
active tasks, retries and remaining time when the estimate is meaningful.
Use summaries rather than scrolling task lists. Show the image and execution
mode, then retrieve one indexed outcome and a bounded failed-results page.
Publish underlying evidence and a reproducible benchmark command beside the
recording. Rates must distinguish accepted completions from local execution
and expose the freshness of chunk-based reports instead of presenting a
completion burst as a sustained rate.

Keep the initial workload small enough to reveal orchestration overhead.
Follow with a short mixed-resource example and a separate restart/retry example
to demonstrate that this capability also handles ordinary heavier work and
failures. Keep faults outside the headline throughput interval and report them
separately. The main run should retain service probes and capacity reservations
so the demo does not depend on starving applications.

The Kubernetes comparison should describe the architectural distinction:
Reliaburger delegates execution and batches global lifecycle accounting as a
built-in capability. Kubernetes already has Indexed Jobs and automatic finished
Job cleanup; native Indexed Jobs still track task completion through Pods.
Its API server also requests etcd compaction every five minutes by default.
Avoid suggesting that batch always requires a third-party framework or manual
compaction, or that Kubernetes cannot run large batches. A numerical performance
comparison needs a matched Kubernetes benchmark and stated execution semantics.
See the official [Jobs documentation](https://kubernetes.io/docs/concepts/workloads/controllers/job/)
and [API server compaction option](https://kubernetes.io/docs/reference/command-line-tools-reference/kube-apiserver/#options).

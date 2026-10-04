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

Portable, cluster and real Linux runtime gates have run. Results and measured
limits are recorded in the
[qualification report](../qualification/2026-10-04-delegated-jobs/README.md).
The sustained harness records unique accepted successes alongside a serving app;
no 24-hour 100m/day result has been established.

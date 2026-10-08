# Reusable container executors and measured job throughput

Date: 7 October 2026. Updated: 8 October 2026. Issues:
[#639](https://github.com/reliaburger/reliaburger/issues/639) and
[#640](https://github.com/reliaburger/reliaburger/issues/640), milestone 0.2.0.
Branch: `codex/reusable-job-executors`, based on main `5cfb9b71`.

## Scope and execution contract

Build on the common singleton, batch, cron and deployment-hook lifecycle from
#642. Keep fresh containers as the default. Add an explicit reusable-container
mode that retains a bounded owned container, starts a separate command process
for each task, and amortises image/runtime setup. A resident AI model process is
#641; GPU execution remains #359. Neither is implied by reusing a container.

Group executors by pinned image, namespace, live credentials, security settings
and resource profile. Reserve the command request plus the helper's independent
CPU and memory request in the same ledger used by applications. Enforce command
CPU, memory and process limits separately from the helper. Reuse only after a
matching cleanup receipt and positive proof that the task cgroup is empty.
Timeout, cancellation or uncertain ownership retires the original container;
resource capacity remains quarantined until positive retirement.

Run host commands and image workloads on the same Bun using the existing owned
process and runc backends. Persist the original backend before creation. Recovery
must consult that owner; missing evidence never authorises a second execution.
Host commands retain their existing allowlist and isolation restrictions, and do
not claim the container backend's resource or isolation capabilities.

## Implementation and verification sequence

1. Expose execution mode in manifests, CLI and summaries. Implement bounded
   authenticated helper control, per-command resource limits, scratch cleanup
   and original-owner retirement. Document the weaker shared PID/network
   boundary of reusable containers and refusal of unsupported configurations.
2. Audit admission fairness, helper overhead and compatible warm capacity.
   Revalidate credentials after slot/resource waiting, at the command boundary.
   Write regressions before each correction. Keep application reservations,
   cancellation and ownership fences authoritative throughout these transitions.
3. Run portable CI, real rootful/rootless Linux gates and the matching cluster
   and upgrade gates. Add combined real-runtime cluster coverage for restart,
   worker loss, stale grants, partial completion, mixed profiles and concurrent
   applications. Record failures and repairs in the qualification document.
4. Measure identical commands through bare processes, direct fresh/reused
   containers, direct durable worker execution and full public dispatch. Record
   cold/warm behaviour, full accepted completion time, retries, failures,
   resource usage, storage growth and concurrent service latency. Do not compare
   different workloads or execution contracts as scheduling overhead.
5. Select the recording volume from achieved results. Put a standalone measured
   asciinema demonstration before the landing-page installation walkthrough.
   Show one submission, accepted progress/rates, backlog, actual command counts,
   failures/retries, service probes and bounded indexed result queries. Publish
   reproducible commands, hardware and raw evidence beside the recording.
6. Qualify sustained execution before claiming 100 million unique accepted
   successes per day: approximately 1,158/s plus headroom for a real 24-hour
   run, concurrent applications, mixed profiles, recovery, delayed messages and
   disk faults. If this hardware misses the target, publish the bottleneck and
   leave #640 unqualified rather than extrapolating a short burst.

## Current evidence and remaining work

The local implementation and regression fixes are in the draft snapshot. The
common real Linux API case accepts 1,000 reused-container commands without
retries. The real Linux resource/cleanup case proves an OOM kill followed by
successful reuse and all eight queued short commands succeeding on one attempt.
Those fixtures prove correctness, not the throughput target. The kernel cleanup
bug, upstream fix and workaround are explained in Chapter 12.

Admission fairness, advertised helper/warm capacity and queued credential
revocation still need correction and regression coverage before #639 is complete.
Existing fake-runner cluster tests exercise real coordination but do not replace
combined reusable-container runtime/cluster evidence. Historical namespace
metadata growth needs measured bounds and retirement-authorised collection;
a live pool bound alone is insufficient. Final full gates remain required.

The four-path comparison, headline landing-page recording and sustained
qualification have not been delivered. #640 remains open and unqualified. See
[development evidence](../qualification/2026-10-07-reusable-job-executors.md)
for the actual environment, test results, failures and measurement limits.

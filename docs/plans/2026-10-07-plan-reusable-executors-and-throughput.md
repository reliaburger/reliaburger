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

The bounded executor and mixed-runtime implementation is complete locally.
Admission fairness, helper-inclusive advertised capacity, compatible warm slots
and queued credential revocation have failing-then-passing regressions. Real
Linux tests prove scratch/child-tree cleanup, actual OOM evidence and subsequent
reuse, timeout, cancellation and one-attempt queueing. Chapter 12 documents the
kernel cleanup bug and the startup/source-authority and inventory fixes found
by public runtime experiments.

Persistent single-node SIGKILL recovery accepted all 96 commands across two
resource profiles, preserved the application's original PID and refused stale
TLS control. A three-VM worker-loss experiment accepted all 384 mixed-profile
commands beside the original application. The returned worker positively
retired its five old-boot executor owners before clearing delegated bindings.
Raw evidence and reproduction commands are checked into the qualification
record. Final portable CI and the complete 159-case Linux runtime gate passed.
Earlier matching rootless, cluster and upgrade gates passed; final pushed
GitHub CI remains required.

Matched debug and optimised execution paths now have actual measurements;
fresh paths expose startup failures and are retained as failed evidence.
The longer optimised process floor launched 100,000 commands successfully.
Full public retained-container dispatch accepted 50,000 successes without
failures or retries in 100.73 seconds on one four-vCPU, 8 GiB VM. The recording
uses achieved completion, not an extrapolated target. The recording and raw
evidence are published in this PR.

The proposed 500,000/minute or million/two-minute recording and the sustained
100m/day qualification remain unmet. #640 stays open: measure and reduce runtime
startup/cleanup and durability costs, establish 24-hour headroom, and cover
concurrent applications, delayed messages and disk faults. Historical namespace
metadata needs measured bounds and collection authorised by original retirement
receipts; a bounded live pool does not bound all retired journals. Resident AI
model workers remain #641, and GPU support remains #359.

See [development evidence](../qualification/2026-10-07-reusable-job-executors.md)
for actual hardware, raw reports, failures and the limits of each comparison.

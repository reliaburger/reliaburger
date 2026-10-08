# Reusable job executor development evidence

Date: 7 October 2026. Issues: #639 and #640. Branch:
`codex/reusable-job-executors`, based on main `5cfb9b71`.

This is development regression evidence. It does not qualify 100 million jobs
per day, the headline demo, or bounded storage across historical namespaces.
There is no 24-hour result and no matched four-path benchmark in this record.

## Environment

The actual runtime cases ran rootful runc 1.4.0 in the `reliaburger-test` Lima
VM: Ubuntu 24.04, Linux `6.8.0-139-generic`, aarch64, four virtual CPUs and
approximately 8 GiB of memory. Pinned BusyBox images were served by the local
verified test-image mirror. Compilation used the `ebpf` feature and the matching
Bun binary. This is a shared development host, not isolated benchmark hardware.

## Reproduced regressions

The two-command case first exposed the Linux `kill_seq` bug. Normal cleanup
wrote `cgroup.kill`, proved the reusable task group empty, then its next
`CLONE_INTO_CGROUP` child received SIGKILL. The [upstream fix](https://kernel.googlesource.com/pub/scm/linux/kernel/git/tip/tip/+/8e359920216689b3b79e0fe8961a77fe312a511f)
snapshots the destination group's counter. Normal reuse now uses authenticated
private PID-1 signalling and reaping, followed by Bun's empty-cgroup proof.
Whole-container retirement removes a killed task group before replacement.
Chapter 12 describes the failure, fix, deployment implications and regression.

The eight-caller resource-pressure regression created eight containers before
the fix. Atomically charging preparation and waiting for a compatible charged
slot reduced it to one container while retaining an application reservation.
The real Linux regression passed; this checks admission/reuse, not throughput.

The mixed-runtime recovery regression deleted the routing journal while an
original backend still ran. Before the fix, another backend could create the
same identity. All four host/image replacement combinations now refuse until
the original ownership is positively retired. Unavailable complete inventories
and contradictory state also refuse. All seven portable mixed-runtime cases
passed after this fix.

## Common public API evidence

The real common API case passed encrypted fresh-image admission and accepted
hook gating, an allowlisted host singleton on the same mixed node, and 1,000
reusable command jobs. It verified terminal accepted counts with no failures or
retries, the admitted isolation, command activity at completion and the first
and final indexed results. Its independently timed batch completed in 6.959
seconds from before submission through observed accepted completion (about
143.7 accepted successes/s).

The control plane is an in-process single-node Raft with `MemLogStore`, not a
persistent production-cluster benchmark. The owned runtime and worker ledger
are real. The fixture deliberately constrained the shared node budget to two cores and
128 MiB, with each reusable profile requesting and limiting 100 millicores and
32 MiB plus helper overhead. Only three retained profiles fit the memory budget.
Commands exercised fresh scratch and index-dependent work. This short shared-host
regression is not a matched throughput benchmark, process-launch floor, or
extrapolatable daily capacity result. Raw development output was captured in
`/tmp/639-linux-common-api.log` on the qualification VM.

## Subsequent serial Linux run

The complete five-case owned-runtime selection passed namespace isolation and
policy-loss retirement in both execution modes, mixed-runtime ownership and the
existing fresh-container packing/cancellation case. It found two failures:

- The common API run accepted 998/1,000 successes. Two concurrent cold starts
  failed with runc's `file exists` error while creating a helper-file bind
  mountpoint in the shared image. The helper now uses its already-populated
  private bootstrap-directory bind; the no-retry regression must pass after
  that change before claiming this case qualified.
- The memory-hog fixture exhausted its 20-second deadline under a 100-millicore
  limit before proving an OOM kill. The fixture now doubles a bounded string to
  force a large allocation directly rather than formatting a million rows.
  Require actual `memory.events` OOM evidence and positive subsequent reuse.

Raw failure evidence remains in `/tmp/639-linux-owned-final.log` on the VM.

The first revised layout put the executable inside the mode-0700 control
mount. Runc's initial stat occurs before process capabilities are installed and
failed with `permission denied`. The common API case reached its deadline;
subsequent fixtures encountered its still-owned routes. Those original owners
were positively retired through their exact runtime journals before deleting
only the identified fixtures. No guessed PID or manual route deletion was used.
The corrected layout exposes only the public static executable in a read-only
bootstrap-directory bind, retaining the control-directory uid and mode 0700.
`/tmp/639-linux-owned-mount-green.log` records that unsuccessful run; its name is
not evidence of success.

Portable CI passed after the missing-route fix: both Clippy configurations,
all 6,156 portable tests, two doctests, 329 script tests and ignored ownership.
Linux-only bootstrap changes still require the revised real runtime gate.

The separated bootstrap layout then passed the real encrypted common API case,
including 1,000 accepted reusable successes with zero failures or retries
(`/tmp/639-linux-bootstrap-common.log`). The real multi-node gate passed all
46 selected cases. These are development regressions, not the headline benchmark.

The revised allocation fixture subsequently allocated 128 MiB and exited zero:
the VM has 8 GiB of swap, and `memory.max` bounds RAM alone. The memory-limited
image-job contract now disables swap in both fresh and reused Linux runc paths.
The OCI assertion genuinely failed against the previous generator (no swap key).
Require the actual OOM counter and subsequent successful reuse before recording
the Linux resource case as passed. The queued short-command timeout regression
has not yet been reached because its preceding memory assertion failed.

## Qualification still required

Measure bare processes, direct containers, direct durable execution and complete
public dispatch with matched workloads and disclosed contracts. Record image
warmth, accepted elapsed time, service latency, retries/failures, resource usage,
RSS, disk growth and namespace churn. Fresh and reused paths need separate
results. Run real-runtime restart/loss, stale grants and partial-completion
cluster cases, then sustained fault and headroom qualification. Keep #640 open
until that evidence and the measured landing-page recording are delivered.

## Admission-time regression

With swap disabled, the real allocation command was OOM-killed,
`memory.events` recorded that kill, and a following clean command reused the
surviving helper successfully. The next assertion then exposed queueing being
charged against a command's timeout: eight 300 ms commands for one fitting
profile, each with a one-second timeout, completed only three successes. The
others timed out or retried (`/tmp/639-linux-swap-timeout-red.log`). The fix starts
the timer after compatible-slot/admission waiting, charging cold preparation
once its whole profile is reserved. The regression now permits one attempt
per command. The corrected real Linux case passed in 13.22 seconds on 8 October
(`/tmp/639-linux-timeout-green.log`): all eight queued commands succeeded on
their first attempt, the actual memory-limit kill was recorded, and subsequent
commands reused the surviving helper. This also exercises normal cleanup on the
affected kernel; a task cgroup killed during final retirement is never reused.

## Portable cluster and upgrade gates

The portable cluster gate passed all 46 cases in 388.263 seconds, and the
upgrade gate passed all 18 cases in 1,457.034 seconds
(`/private/tmp/639-cluster-upgrade.log` on the host). These ran before the latest
swap-limit and queue-timeout fixes. The task-array cluster fixtures use the fake
runner with real coordination; they do not qualify reused-container worker loss
or establish production throughput. Repeat the relevant final gates after the
remaining admission audit.

## Draft snapshot runtime gate

On 8 October, the complete five-case real rootful Linux owned-runtime selection
passed serially in 56.67 seconds (`/tmp/639-draft-linux.log` on the VM), after
the bootstrap-directory, swap-limit and queued-timeout fixes. It covers fresh
packing/cancellation, fresh/reused namespace isolation and policy-loss
retirement, common encrypted admission with 1,000 accepted reused commands,
reused scratch/environment/resource cleanup, and mixed runtime owner recovery.
This scoped selection does not replace the full Linux or rootless gate, the
combined real-runtime cluster failure cases, or throughput qualification.

The 8 October draft snapshot also passed `make ci`: both Clippy configurations,
6,157 portable tests, two doctests, 329 CI-script tests and ignored-test owner
checks (`/private/tmp/639-draft-ci.log`). Later admission corrections require
new focused regressions and a final CI run.

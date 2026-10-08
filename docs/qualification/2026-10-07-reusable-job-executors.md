# Reusable job executor development evidence

Date: 7 October 2026. Issues: #639 and #640. Branch:
`codex/reusable-job-executors`, based on main `5cfb9b71`.

Updated 8 October: the landing page now publishes 50,000 accepted container
successes in 100.73 seconds with no failures/retries. Optimised matched paths,
real persistent Bun restart and three-VM worker loss have raw evidence below.
Fresh paths retain startup failures and whole-directory storage observations
are incomplete. This remains development evidence, not 100 million jobs/day,
the proposed 500,000/minute recording target or bounded historical storage.
There is no 24-hour result; #640 stays open.

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

## Admission corrections after the draft

Two new portable regressions genuinely failed before correction: an immediate
executor reservation overtook a larger queued owner, and advertised capacity
was three profiles where only two tasks plus helpers fitted. Both now pass,
alongside all six budget cases and all 15 worker admission cases. The executor
reservation checks queue state and capacity atomically. Worker offers include
helper overhead, the pool bound and already-reserved compatible warm capacity.

The new real Linux case passed the warm-capacity and external-owner yield
assertions, then genuinely failed: a queued command executed with a secret
decrypted before its namespace key was retired
(`/tmp/639-admission-linux-red.log`). The test proves the old ciphertext can no
longer decrypt under live identities. The original fixture owner was positively
retired before removing its exact directory. Command-boundary revalidation passed the corrected real Linux case in 6.96
seconds (`/tmp/639-admission-linux-green.log`): the queued command was refused,
produced no output, and its full reservation returned after positive cleanup. This test uses real runc
and the worker ledger with an in-memory test council, not production Raft
durability or throughput evidence.

## Initial draft CI rootless regression

GitHub run `37719598978` passed ten rootless cases but failed the eleventh,
`rootless_cluster_refuses_before_ownership_recovery_for_every_selection`. The
new mixed runtime refused rootless clusters promptly but omitted the existing
standalone guidance and changed the diagnostic checked by the gate. The mixed
branch now uses the same detailed diagnostic as direct runc. Gate revalidation
is pending; the skipped later privileged gates are not recorded as passes.

The full six-case scoped rootful runtime selection passed serially after those
corrections in 63.94 seconds (`/tmp/639-admission-linux-all.log`). Portable
`make ci` passed both Clippy configurations, all 6,159 tests, two doctests,
329 script checks and ignored-test ownership
(`/private/tmp/639-admission-ci.log`). The full privileged Linux, combined
real-runtime cluster failures and throughput qualification remain outstanding.

## Final admission gates and first complete-path measurements (8 October)

After the admission corrections, all six scoped rootful runtime cases passed
serially in 63.94 seconds. The rootless owned-runtime suite passed 10/10 in
13.26 seconds. The additional real Bun rootless recovery case passed in
46.92 seconds on ext4 after freeing stopped test binary copies. Earlier
attempts exhausted the VM disk; placing the fixture on virtiofs instead exposed
a truncated fixture-runtime state reply. Those failures and originals are
retained in the local logs. The kernel user-namespace setting was restored.
The current portable cluster gate passed 46/46 in 378.066 seconds and the
upgrade gate 18/18 in 1,394.716 seconds.

An isolated single-node TLS cluster, using the real Bun process, persistent Raft,
owned mixed runtime and eBPF, accepted 10,000 reused BusyBox `true` commands in
42.286 seconds (236.48/s), with zero failures/retries and concurrent service
probes succeeding. Its recorder then failed indexed verification because it
queried the manifest parent rather than the cohort. A repeat exposed an atomic
rename race in storage sampling and cancelled through the public batch ID.
Both recorder failures have failing-then-passing regressions. The next complete
recorder verification accepted 10,000 commands in 45.552 seconds (219.53/s),
verified the selected cohort indexes and completed with successful service
probes. That repeat overlapped compilation: it is recorder correctness evidence,
not a quiet-host performance comparison or a headline volume qualification.

The environment remains the 4-vCPU, 8-GiB ARM Linux VM described above, with
debug Rust binaries and warm pinned image content. The workload is the minimum
BusyBox command, not AI inference or training. These rates miss both the daily
headroom target and the proposed demonstration volume. Matched quiet-host
baselines and bottleneck analysis are still required; #640 remains unqualified.

The first real Bun crash case killed the verified task-owned Bun after 32 of
96 mixed-profile commands had accepted successes, with two verified commands
active. The original application kept serving, but the replacement refused with
`kernel source entries have no original ownership`. Startup now validates the
delegated namespace journal separately from application firewall authority.
Source validation passed its boot/inode unit regression. The repaired real Bun
restart adopted the application with its original PID and accepted all 96
commands with zero failures and retries. Stale control version zero was refused
with no slots, and the first, middle and last outcomes of both resource cohorts
were verified through bounded public indexed queries. The TLS probe verified
the node DNS name, CA and SPIFFE identity. The initial probe incorrectly used
an IP address against a node-name certificate; correcting the probe did not
disable certificate verification. Raw proof is in the fixture
`/tmp/rb-639-demo-20261008/recovery-proof/report.json`; this is a single-node
persistent-Raft crash regression, not multi-node failover or daily qualification.
No ownership journals or kernel entries were removed to bypass the refusal.


## Matched debug execution comparison (8 October)

Five serial paths ran the same pinned BusyBox `/bin/busybox true`, with 1,000
commands, concurrency 27, warm images and cold executors, on the same four-vCPU
VM beside the original live application. Container commands requested 100m and
32 MiB, limited CPU to 1 CPU and disabled swap. The job budget was 3 CPU / 4 GiB.
Bare processes had no isolation, ownership journal, hard limits or task ledger;
the resource fields in their original report describe comparison inputs only.
The direct runner did not inject the live namespace-policy hook. Public Bun
adds that supervision. These differences prevent attributing the whole gap to
scheduling alone.

| Path | Verified successes | Failures / retries | Elapsed seconds | Successes/s |
|---|---:|---:|---:|---:|
| Bare processes | 1,000 | 0 / 0 | 0.073 | 13,681 |
| Direct fresh owned containers | 1,000 | 0 / 0 | 353.923 | 2.83 |
| Direct retained containers | 1,000 | 0 / 0 | 8.600 | 116.28 |
| Durable fresh worker | 997 | 3 / 0 | 359.411 | 2.77 |
| Durable retained worker | 1,000 | 0 / 0 | 9.544 | 104.78 |

The durable fresh run is **failed evidence**, not an all-success benchmark. Its
three bounded failure outputs reported `runc startup timed out` (indexes 170,
824 and 883). The harness positively retired its original runtime owners.
The retained paths both succeeded without retries. The short bare run needs a
longer repetition before treating its rate as a stable floor. Image preparation
is reported separately and excluded from direct-path timing; cold helper
creation is included. All recorded service probes returned 200. The reports
include every probe latency, rather than presenting a sparse sample as a
service latency guarantee.

Raw reports: [debug comparison](2026-10-08-job-measurements/debug-baselines/).
The matching driver is `examples/job-throughput.rs`. These debug development
results establish the benefit of retaining containers and expose fresh runtime
startup/durability costs. They do not qualify production binaries or a cluster
with 100 million accepted successes per day.

The public retained 1,000-command run completed in 20.699 seconds with zero
failures/retries and verified selected indexes. The matching public fresh run
reached its 600-second observation deadline without accepted chunk completion
and was cancelled through its public identity. It is failed evidence, preserved
with its raw samples in [public debug observations](2026-10-08-job-measurements/public-debug/).
Its log repeatedly recorded `consumer runtime inventory timed out` and roughly
500 ms application-loop snapshots. Inspection found inventory reads taking
mixed-runtime mutation locks and retaining detached work after their caller
timed out. The inventory lock regression and corrected real public run must
pass before using this as a performance comparison. The final portion also
overlapped compilation of that regression, so this is diagnostic evidence,
not a quiet-host rate qualification.

The corrected owned-inventory fixture genuinely failed against the old mixed
wrapper with `read-only inventory waited for runtime mutation`
(`/private/tmp/639-inventory-lock-red-owned.log`). All eight mixed-runtime
unit cases then passed, including that regression, after the snapshot change
(`/private/tmp/639-inventory-lock-green.log`). The first test-fixture attempt
had an empty mock inventory and failed its expected row count; it did not
establish lock contention. The corrected real public run is still required.


## Optimised serial comparison and published demonstration

After preserving immutable optimised binaries, the direct paths ran serially
with the same image, command, 1,000 indexes, resource contract and concurrency.
The three-node loss fixture was stopped through its public lifecycle before
these runs; no compilation overlapped them. They use the same four-vCPU / 8 GiB
VM beside the original application. The longer bare-process floor separately
verified all 100,000 commands in 6.267 seconds (15,955.37/s). It is not the same
volume as the matched 1,000-command rows below.

| Path | Verified successes | Failures / retries | Elapsed seconds | Successes/s |
|---|---:|---:|---:|---:|
| bare-1000 | 1,000 | 0 / 0 | 0.074 | 13553.68 |
| fresh | 998 | 2 / 0 | 188.713 | 5.29 |
| reused | 1,000 | 0 / 0 | 4.867 | 205.47 |
| durable-fresh | 996 | 4 / 0 | 187.934 | 5.30 |
| durable-reused | 1,000 | 0 / 0 | 5.245 | 190.66 |

Fresh and durable-fresh are **failed evidence**. The durable run preserves four
bounded outputs reporting runc startup timeouts. Retained containers completed
all commands with no retries. These timings include original ownership and
positive cleanup. Image preparation is separately reported and excluded; cold
executor setup is included. The direct paths omit the full Bun namespace-policy
hook. Differences in contracts prevent assigning the entire process/container
gap to scheduling. The short bare row remains a noisy observation; use the
longer repetition for the process-floor result.

The optimised public retained path accepted 1,000 successes in 13.049 seconds,
10,000 in 26.893 seconds and 50,000 in 100.730 seconds, all without failures or
retries. These public runs were serial after the direct fresh run. Their timer
starts before launching the submission command and ends at observed accepted
terminal completion; selected first/middle/last indexed outcomes are also
verified. All concurrent application probes returned HTTP 200. The 50,000 run
made 95 probes; its maximum observed latency was 12.538 ms. This sampling does
not prove uninterrupted availability or a latency SLO between observations.

The independently measured optimised public fresh path now completed its
1,000-index chunk rather than reaching the earlier observation deadline:
999 successes and one failure in 277.032 seconds. Index 115's bounded output
reported `runc startup timed out`. This is failed evidence; it overlapped the
separate three-VM loss proof and is diagnostic, not a quiet-host comparison.
Removing the inventory mutation lock resolved the reproduced read contention;
it did not resolve every fresh-container startup failure.

The landing page publishes the actual 50,000-job cast before installation.
Its event timestamps retain real pauses and its report states
`qualified_100m_per_day=false`. Bare-floor data appears separately; the recording
shows actual public submission, accepted summaries, execution mode, backlog,
verified activity, chunk-burst rates, failures/retries, service probes and
bounded indexed queries. It uses unreleased development binaries, not 0.1.6.

During that run, selected Bun RSS reached 225,705,984 bytes. This excludes other
owners and container working sets. Every whole-data-directory sample exhausted
the 4,096-entry observation budget, so all those storage observations are
explicitly incomplete. They cannot establish bounded disk growth. Raw samples,
workloads, reports and failure output are in
[optimised public evidence](2026-10-08-job-measurements/release-public/);
[direct reports](2026-10-08-job-measurements/release-baselines/) disclose each
path's execution contract.

The achieved 496.38 accepted successes/s is below both the suggested recording
rate (about 8,333/s) and 1,158/s daily target on this hardware. **#640 remains
open.** Resulting work is to diagnose and remove the fresh startup failures,
profile retained command startup/scratch cleanup and runtime journal/index
costs, qualify scaling and resource headroom on measured hardware, and prove
retirement-authorised collection across historical namespaces and Bun restarts.
Add real delayed-message and disk-fault cases, then run 24-hour accepted-success
qualification beside applications. Do not extrapolate this short burst into a
daily success claim. Resident model workers (#641) and GPU execution (#359)
remain separate; BusyBox launch rates are not AI model throughput.

## Real optimised runtime recovery

The permanent single-node recovery probe passed against the immutable optimised
Bun. It SIGKILLed Bun with partial accepted completion and active commands,
then restarted its exact original arguments. All 96 mixed-profile indexes were
accepted without failures or retries. It retained the application's original
PID, verified selected results and rejected stale control over CA/DNS/SPIFFE
verified TLS. Raw evidence: [single-node proof](2026-10-08-job-measurements/release-recovery/).

Three actual Linux VMs (each 2 vCPU / 2 GiB) then ran 256 small and 128 large
reusable jobs through persistent Raft, gossip, TLS and eBPF. Worker 3 stopped
through the public CLI after partial accepted completion and verified active
work. The remaining workers accepted all 384 successes, with zero terminal
failures. The original `hello` application kept PID 2629 on worker 1 and every
sampled request returned HTTP 200. Both cohorts' first, middle and last outcomes
were verified. On return, worker 3 had a new boot ID, five positively retired
old-boot executor intents and an empty delegated namespace journal. No original
ownership evidence was deleted. [Raw loss evidence and reproduction](2026-10-08-job-measurements/three-node-loss/)
record the at-least-once boundary and fixture hardware. This proves a real
worker loss, not every partition or delayed-message case and not sustained
throughput. Two earlier probe attempts did not stop a worker: one missed its
active window, and one supplied an unsupported `--node` option. Neither counts
as loss evidence; the successful driver uses the verified positional node.


## Final local gates (8 October)

The portable `make ci` after the final capability correction passed both
Clippy configurations, all 6,161 tests, two doctests, 337 script tests and
ignored-test ownership checks. The separate portable `make coverage` before
that report-only correction passed 6,160 tests and measured **88.17% line
coverage**, above the unchanged 78.65% floor. LCOV and HTML reports were saved
before cleaning only the task-owned instrumented workspace build artifacts;
no original coverage executable was live. Final Linux Clippy passed with all
features and with none. After the recorded fixture and capability corrections,
the complete provisioned `make test-linux` gate passed all 159 cases in
704.120 seconds, with retries disabled. This includes all six owned executor
cases and the public secrets, image-registry and workload-identity catalogues.
Final pushed GitHub checks remain required.

The first final Linux run passed 33 cases, then the standalone view-lease
routing case refused a connection. The earlier persistent recovery experiment
had intentionally left four original kernel links attached. All 291 original
runtime intents were positively retired and no matching owner process remained.
Calling `OnionEbpf::retire_owned_state` for that exact manifest retired only
those links and retained the journals; the unchanged routing case passed.
This is fixture contamination, not successful first-pass evidence.
The [original-owner proof and terminal manifest](2026-10-08-job-measurements/gate-fixture-retirement/)
record the exact retirement boundary.

The focused namespace adoption extension then correctly refused its two
synthetic service backends: this fixture had directly published them without
application discovery owners. The fixture now removes only those two known
backend keys after positive runtime retirement, before adoption. The product's
refusal of unknown discovery owners remains unchanged. The original failed
fixture and its ownership journal were preserved for diagnosis.

The second full Linux run passed 155 of 159 cases, including all six owned
executor cases, then the public secrets catalogue skipped its three probes.
The runtime classifier recognised `runc` but not `runc+process`. A portable
mixed-node capability regression failed before correction. Container capability
and runc-version reporting now recognise mixed nodes; host backend availability
still requires the executable allowlist. Legacy placeholder-image process
catalogue probes remain process-only, avoiding an image-to-host fallback.
The corrected catalogue and the three cases cancelled by fail-fast all passed
in the final complete 159-case Linux run. GitHub's final full gate remains
required.

The final Linux rebuild uses Ubuntu LLD 18.1.3 through a task-local `cc`
driver, retaining the same Rust toolchain, Cargo profiles, source, feature set
and complete runtime selection. The system compiler was not replaced. The
recorded optimised throughput binaries precede this final capability-reporting
correction; executor, admission, inventory and recovery execution code is
unchanged. Their actual binary hashes and timing reports remain the evidence,
not a new measurement attributed to the later report-only fix.


## Final GitHub evidence binding correction (8 October)

[Run 37748296010](https://github.com/reliaburger/reliaburger/actions/runs/37748296010)
passed all execution owners: Linux portable coverage (86.08% lines), macOS,
rootless adoption, strict OCI interruptions, the complete privileged Linux
gate, cluster, acceptance, both upgrade gates and standard registry clients.
Its final ignored-test evidence aggregation failed because three newly added
executor cases used `owned_task_arrays` in the reviewed binding table instead
of Cargo's exact `reliaburger::owned_task_arrays` binary ID. The uploaded
Linux owner's actual discovery and successful completion include all three
cases under the latter ID. The table now matches that observed identity; no
selector, test, ownership rule or evidence consumer changed. A local source
identity check reproduced all three refusals before correction and none after.
The next pushed run must still pass every selected owner and final aggregation;
this failed aggregate is retained as a failure, not counted as a qualified run.

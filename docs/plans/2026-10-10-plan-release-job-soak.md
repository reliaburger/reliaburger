# Jobs in the existing release soak

10 October 2026. V02 extension for the 0.2.0 machinery in #654 and its review
fixes in #669. The controller and release gate are implemented; staged fast
and final qualification are still required on the integrated candidate. The
90-minute fast and eight-hour final tiers, fault schedule and acceptance
durations are unchanged.

## Run jobs alongside the apps

Keep fresh-container, shared-container and native host arrays active while
V02 kills agents, powers off VMs, loses quorum, rotates certificates and walks
upgrades. These operations already consume the soak's time. Adding three
sequential hour-long benchmarks would test fewer interactions and extend it.
The raw VM baseline remains a performance reference outside the release gate;
it exercises none of the machinery we need to qualify.

Use a bounded controller against the staged candidate's public CLI/API. It
maintains two active common-path runs per runtime, one short and one long,
and records a submission intent before sending it. On restart or a lost reply,
resend the exact same definition and request ID through `POST /v1/jobs/runs`.
The server resolves this idempotently; a name alone is not a deduplication key.
Save its run IDs and sampling state under the existing evidence directory so
`--resume` continues the same campaign. A missing controller heartbeat or an
unreconciled submission fails coverage; an empty snapshot must never mean clean.

Choose small, explicit resource requests, memory limits and a campaign
concurrency budget per node, initially eight commands split 2/3/3 across fresh,
shared and host modes. The nine finite cron, publication and deploy-hook
fixtures each add at most one command per node; the eight-command figure is
not an aggregate limit over those extra runs. This is a conservative starting point to calibrate on
the release rig, not the demo's throughput setting. Include reusable helper
reservations in that budget. Retain all existing apps and their probes. The
controller may reduce load during recovery but must record the reduction and
meet minimum activity coverage for every runtime. Admission still decides what
fits; do not infer running capacity from a configured concurrency cap.

All job modes receive work until the last two-minute drain interval. Mix short
commands (`sleep 0.02`) and 20-second commands so faults can interrupt active
work. Each runtime has separate 32-MiB and 64-MiB runs with 25m requests and
one-core limits. Failure probes add a cold 8-MiB profile. Short runs contain 262144 tasks in 64-task chunks; long runs contain 512
20-second tasks. This bounds run turnover even on faster rigs. Two-second
controller polls between replacement runs allow the one-second idle eviction policy to
retire helpers; repeated commands within a run exercise warm reuse. Bound pending work and retained run counts;
never enqueue a whole day's backlog just to keep a small rig busy.

## Connect jobs to the existing faults

Before a scheduled agent kill or power-off, verify active attempts on the target
node and record their run/grant identities. Fresh runc reports null activity
through the public summary. Its long/cancel fixtures therefore commit a start
receipt with the kernel boot UUID and shell process start ticks. A bounded
private guest observation joins that receipt to the current generation's exact
run/index and populated cgroup; queued callers cannot earn overlap coverage.
Shared/host modes use their verified public activity counters. Use the existing preparation/settle
budget; if overlap cannot be established, record missing coverage rather than
waiting beyond that budget. After recovery, confirm both app health and job
progress. Repeated faults must cover every runtime, each node and leader versus
worker recovery across the run. Quorum loss and all-off specials exercise durable
state recovery while work is outstanding. Upgrade walks use compatible private
soak builds from the same candidate; a format generation change requires a
fresh cluster, not a mixed-generation upgrade test.

Reuse the existing deploy slots to run a small required `run_before` hook and
ordinary publication-triggered singletons with verifiable outcomes. These are
not a new `run_after` hook API. Add a minute-scheduled singleton
job so cron uses the same durable path under faults. Check the implementation's
occurrence identity, overlap and missed-occurrence policy, not an invented
exactly-once wall-clock guarantee. Singletons, schedules, hooks and arrays all
need activity evidence, even though they share an executor.

Add bounded targeted cases within existing wait periods: cancel active arrays,
known non-zero exits with retries exhausted, a timed-out command with a child,
a resource-limit failure, and a non-replayable command interrupted by a fault.
For the latter, require two observations of the same unknown-owner grant
fingerprint before the test controller explicitly acknowledges replay. Only
the known replay-safe long fixtures (sleep commands with an idempotent test-only start receipt, and named `true` cron/hooks) qualify
for this test-operator action. Record the decision count; ordinary service
recovery must not repeat non-replayable work on its own. Expected failures belong to named fixtures and cannot excuse
unrelated failures elsewhere.

## What makes the job gate pass

The asynchronous controller fetches bounded summaries for its six runs. Fresh
activity uses one bounded guest call per node, with a seven-second deadline;
these calls overlap rather than delaying fault injection. At every existing
30-second light check, copy its latest compact snapshot. At every five-minute heavy check and fault
settle, collect job diagnostics and executor ownership/resource inventories.
Use the same observation timestamps and fault windows as the app checker.

- **Accounting:** accepted success/failure/not-run plus queued/held counts
  conserve submitted indexes; accepted counters never regress; IDs and runtime/
  resource requests remain unchanged. Retries are reported separately. The
  audited cohort independently checks exact logical index coverage. Scalar
  summaries alone cannot independently prove every bulk task index executed.
- **Correctness:** have a bounded audited cohort whose commands write stable
  `(run, index)` identities to an independent verifier. Compare verified logical
  effects against accepted outcomes. An idempotent retry may execute again;
  record duplicate attempts separately. A successful counter alone cannot prove
  the command performed the intended work or prove exactly-once execution.
- **Recovery:** each mode makes accepted progress in healthy intervals and
  resumes within the existing settle deadline. Expected fault-window downtime
  does not fail throughput; unresolved ownership, silent stalls or missing
  receipts after settling do. Preserve app availability/data and the existing
  one-second agent-turn gate.
- **Cancellation and isolation:** cancelled work stops; descendants, sockets
  and leases drain; shared tasks do not retain prior task environment, output
  or temporary files. Host jobs explicitly run without container isolation.
- **Bounds:** active pools/grants/queues stay within configured limits. Track
  executor owners, processes/zombies, FDs, cgroups, namespaces, veths, network
  leases, run-store size, retained output and disk growth. Exercise collection
  and prove plateau/expiry where a bound is implemented. A storage collector
  still under development is a recorded release gap, not a passing bound.
- **Coverage:** require work and fault overlap evidence for each mode, plus
  singleton/cron/hooks, mixed profiles, cold/warm reuse, eviction, cancellation,
  deadlines and failure handling. Missing required evidence fails the gate even
  when the rest of the cluster looks healthy.

The current `leak_findings` checker assumes every runtime/network object belongs
to an app instance. Reusable executors violate that assumption legitimately.
Extend it to validate exact private job/executor ownership, current boot and
generation and canonical pool slots, and record enforced cgroup limits. Declared resource
profiles are also checked through the API. The current API does not expose
the live commitment ledger, so these inventories cannot independently prove
that every idle helper is still charged to it; admission/runtime gates remain
necessary for that check. Do not blanket-allow executor-shaped names or raise leak limits.
At deliberate drain checkpoints, wait within the existing settle budget for
idle retirement and require the job-owned inventory to return to its baseline;
other app-owned objects remain accounted for throughout.

Performance is supporting evidence: per-mode accepted counts, retries,
progress timestamps and complete disk/resource inventories. Compare regressions
only on the same rig/build configuration, with explicit tolerance after
calibration. No universal laptop rate and no extrapolated daily count determine
a reliability pass. Saturation benchmarking remains separate: do not run three unbounded
saturators beside the apps or use a synthetic rate as the reliability verdict.

## Keep the schedule and evidence bounded

Job sampling runs asynchronously with strict request timeouts. It must not add
serial work to `run_cycle` or extend the existing min-cycle/settle deadlines.
Drain, inventory validation and the controller shutdown fit within the existing
last settle and teardown; inability to drain within that budget is a failure.
The current harness can already overrun when recovery is slow. This extension
must introduce no new soak duration, recovery allowance or extra cycle.

Keep compact aggregate snapshots, coverage counts, rig/candidate/config
identities, run IDs and the small verifier ledger. Bundle raw details on failure
and cap them; never retain millions of successful task rows or duplicate JSON
snapshots. Record peak disk/RSS/FD usage and any unsupported check explicitly.
The final verdict must include job findings and coverage, not merely append a
pretty job table to an otherwise passing V02 record.

## Implementation order and validation

1. Extend `soak-workloads.toml`/candidate configuration with the cron and hook
   fixtures, mixed Bun runtime, pinned image and narrowly allowlisted host
   BusyBox. Record the unisolated host opt-in as a soak-only deviation. Preserve
   production defaults and existing workloads. Verify capabilities from the
   running candidate and fail missing required support for 0.2.0 qualification.
2. Add the resumable public job controller and audited cohort. Test lost
   submission replies, controller restart, backpressure, cancellation, fault
   windows and unavailable nodes before wiring it into the shell harness.
3. Extend `sustained_check.py` and its Python tests with accounting, progress,
   coverage, inventory ownership and drain checks. Replay synthetic snapshots
   with deliberately lost/duplicate outcomes, phantom capacity, orphaned owners,
   stale heartbeats and growing retained storage; each must fail appropriately.
4. Wire start/stop, bounded sampling and fault-overlap markers into
   `qualify-sustained.sh`; render compact job results in the existing record.
   Add harness tests proving timings/durations are unchanged and job failures
   propagate to the exit status. Keep `soak.yml` within its existing hosted
   runtime budget. Update the release runbook, testing guide and book together.
5. Run the normal fast tier against staged binaries, calibrate resource caps
   and diagnose failures, then run the existing final tier on the final candidate.
   Keep product bugs and harness bugs distinct. Never pass by disabling a mode,
   suppressing job failures or skipping an existing fault.

This plan covers reliability alongside apps. The separate full-speed hourly
measurements and storage/failure-summary follow-up (#668) remain useful inputs;
a passing mixed-workload soak does not itself qualify sustained 100m/day.

## Implemented entry points and qualification boundary

- `scripts/release/job_soak.py`: durable intents, bounded six-run campaign,
  independent authenticated effect ledger, targeted probes, fault markers and
  bounded drain. At most 256 submissions, 32 unresolved named fixtures and 4096
  verifier effect identities are retained by the controller/verifier. The
  candidate's own storage lifetime is measured separately.
- `scripts/release/job_soak_inventory.py`: complete bounded kernel scans and
  private journal proofs, boot/generation checks, cgroup limits and complete
  `du -skx` measurements. A truncated/unsafe/failed inventory cannot mean clean;
  less than 256 MiB free fails the job storage gate.
- `scripts/release/sustained/soak-jobs.toml`: all three modes as singleton,
  minute-scheduled and `run_before` jobs. The host executable is copied from
  the same pinned BusyBox image used by the container fixtures.
- `sustained_check.py`: job heartbeat/progress/accounting failures affect live
  findings, recovery cleanliness and the final exit status. Final coverage
  requires every runtime overlapping faults on every node, leader kill,
  follower kill and power-off, and positive drain before teardown.
- `job_soak_linux.py`: a disposable rootful Linux smoke fixture for real
  execution, audited effects, retries, timeouts, memory kills, cancellation,
  mixed profiles and exact owner retirement. Its smaller runs and isolated
  mount/network identity are test-only; it cannot emit a release pass.

Portable regression checks run through `make test-ci-scripts`. Run the real
smoke check with `sudo python3 scripts/release/job_soak_linux.py --bun
/absolute/path/to/bun` on rootful Linux with runc, cgroup v2 and BusyBox.
The normal staged fast/final runs still need an integrated candidate, including
#669's correctness fixes. Do not treat a local smoke result as that evidence.
The global retained-storage bound and 24-hour qualification remain #668.

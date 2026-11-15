# Where the host-job budget goes (20 Oct 2026)

Step 1 of the 0.2.3 throughput goal ([#1029](https://github.com/reliaburger/reliaburger/issues/1029)):
profile Bun while `relish bench --scenario jobs-host-processes` runs, and find
out why host jobs reach only about a third of the raw process rate.

**Answer: they mostly wait. Per-task cost is a smaller, second-order problem.**
At the default cap of 27 the VM's CPU is a third idle, and for 1.5 to 2 s of
every 3 s cycle no task runs at all. Three things cause it:

1. **Held chunks run in lockstep, so the node drains before it refills.**
   Every granted chunk gets its own dispatch loop, and all of them wait on the
   same fair slot semaphore. So 16 chunks advance together, none finishes
   early, and their receipts all reach the leader at the end. Then the node
   holds nothing runnable until the next sync brings a fresh grant. Inside a
   burst the node runs about 10,000 tasks/s. Averaged over the cycle it runs
   5,100/s.
2. **The learned grant depth is capped at 16 chunks** (16,000 tasks), so
   each burst is short. Raising the cap to 64 (experiment E1, below) took
   host jobs from 5,100/s to 6,766.7/s (+33%).
3. **Bun burns a core doing nothing.** The GitOps loop wakes on every Raft
   metrics change, which happens on every commit. Each time, it clones the
   whole `DesiredState`, including every task array's record, before it even
   checks whether GitOps is configured. That is 41% of Bun's CPU in a
   host-job window, and 64% of it when Bun runs no tasks at all. Stopping
   those wake-ups (experiment E2) cut Bun's CPU in a 62 s raw window from
   61 s to 0.7 s (medians). It took the raw baseline from 18,026.5/s to 22,074.3/s and host
   jobs to 7,066.7/s (steady 8,465/s).

Raising the cap from 27 to 32 changed nothing: 5,100.0/s in all five rounds,
both ways. Per-task overhead comes after the three causes above. The biggest
per-task items are fork-style process creation in the helper and the helper's
two round trips per task. The ledger, the leader tick, cgroups and fsyncs cost
almost nothing.

This is not the 0.2.0 rig's hardware. The
[0.2.0 throughput record](../2026-10-17-v0.2.0-job-throughput/README.md) ran
on an M2 Max. This record was taken on an **M1 Max** with the same VM shape,
so compare this record's numbers with each other, not with that table.

## The rig

| Item | Value |
|---|---|
| Host | Apple M1 Max, 10 cores, 64 GiB, macOS 15 (Darwin 24.6.0). **Not** the M2 Max of the 0.2.0 record |
| VM | `rb-bench-1029`, Lima 2.2.1 (VZ), 4 vCPU, 8 GiB, 60 GiB disk, `template:ubuntu-24.04`, made for this run and left stopped |
| Guest | Ubuntu 24.04.5 LTS aarch64, Linux 6.8.0-142-generic (as in the 0.2.0 record), runc 1.5.2, perf 6.8.12 |
| Source | `release-0-2-3` at `f68755d9` (crate 0.2.1) |
| Release-style build | Rust 1.98.0 (`build.yml`'s pin), `cargo build --locked --release --features ebpf --bin bun --bin relish`, stripped as `[profile.release]` says. `bun` SHA-256 `cab1d7cb…f83b`, `relish` `16bd17a7…0f29` |
| Profiling build | The same, plus `CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=line-tables-only RUSTFLAGS="-C force-frame-pointers=yes"`. `bun` `1a6b6cf4…98d7` |
| BusyBox | Ubuntu's `busybox-static` 1:1.36.1-6ubuntu3.1, copied to `/opt/bench/busybox`, SHA-256 `52151e7f…e933`. The 0.2.0 record extracted the pinned image's BusyBox; that differs, so raw and host rates here compare only with each other |
| Scenario | `relish bench` defaults: 60 s windows, 25m CPU request, one-core limit, 32 MiB, 1,000-task chunks, a 16-million-task array per window, beside a 500m BusyBox web app on a fresh single-node cluster |

Every session started on a fresh boot of the VM. A session that follows another
Bun on the same boot fails, because the old node's mounts survive
(`rm: cannot remove '/var/lib/reliaburger/volumes/.identity/default__web-0'`),
much like the bpffs pins the 0.2.0 record ran into.

### Host load

The coordinator asked for the 1-minute load average before and after every
round, and to discard any round above about 5. On this host that rule discards
every round, including runs with nothing else on the machine:

- The busy 4-vCPU VM alone adds about 4 to the load.
- With the VM idle, macOS background services (`mediaanalysisd`, Spotlight,
  Chrome) kept the load near 6 and used about two cores.

So a strict reading discards every window: 20 of 20 in the first baseline, 19
of 20 in the second, 29 of 31 in the cap session, and 11 of 11 in each
experiment. The window-edge loads ranged from 2.98 to 9.52.
[`host-busy.log`](host-busy.log) samples, every 10 s from 17:40 UTC, the load,
the VM's CPU, everything else's CPU and the number of `rustc`/`cargo`
processes. It shows **no compiler running during any rate round from 17:40
on**. [`host-load.log`](host-load.log) covers 16:32 to 18:32. I kept the
rounds and say so here, rather than report nothing. Three things back that:

- The first baseline overlapped another agent's builds. It and a repeat run on
  a quiet host (no compilers) gave **identical host-job results**: 226,000
  accepted tasks in four rounds, 290,000 in one, both times. Host jobs are
  bound by grant cycles, not by spare host CPU.
- Raw-process medians agree within 2.3% across the two baselines.
- Every rate session has five rounds and a range.

Pre-run 1-minute loads: profiling session 3.8 (it rose to 26 during that
session while another agent built; profiles are less sensitive than rates, and
its windows ran at the same rates as the later quiet ones); first baseline
2.6; repeat baseline 3.0; cap session 5.9 (macOS services, no compilers);
E1 3.9; E2 3.6.

## Rates

### The unchanged release script

[`scripts/release/job_bench_rounds.sh`](../../../scripts/release/job_bench_rounds.sh)
`--rounds 5`, release-style build, repeat run (`baseline/`):

| Scenario | Median (range of five) |
|---|---:|
| Raw VM processes | 17,716.3/s (14,683.2–18,398.7) |
| Fresh containers | 10.2/s (10.0–10.4) |
| Shared containers | 2,233.3/s (2,116.7–2,466.7) |
| Host jobs | 3,766.7/s (3,766.7–4,833.3) |

Every window completed, with no failures or retries, and cleanup was verified.
The first run, which overlapped another agent's builds, gave 18,125.7, 10.2,
2,383.3 and 3,766.7.

### Concurrency cap, with CPU accounting

[`tools/session.sh`](tools/session.sh) sets up the same cluster and then
alternates host and raw windows, five rounds per cap. Each window also records
`vmstat`, `/proc/stat`, Bun's own CPU, cgroup CPU and a mid-window
`relish batch-status` (`caps/`, and the per-window table in
[`caps/summary.txt`](caps/summary.txt)).

| Cap | Host jobs, median (range) | Steady 15–58 s | Host CPU busy | Raw, median (range) | Raw CPU busy |
|---:|---:|---:|---:|---:|---:|
| 16 | 4,583.3/s (4,300.0–4,866.7) | 4,777/s | 64% | 17,980.8/s (17,976.5–18,049.1) | 93% |
| 27 | 5,100.0/s (5,100.0–5,100.0) | 5,149/s | 67% | 18,026.5/s (17,947.8–18,169.0) | 94% |
| 32 | 5,100.0/s (5,100.0–5,100.0) | 5,149/s | 66% | 18,191.8/s (18,000.8–18,244.2) | 94% |

Two things in that table answer the issue's questions:

- **The cap isn't the limit.** Going from 27 to 32 changes host jobs by 0%
  and raw by 0.9%. Halving to 16 costs host jobs 10%.
- **The run is waiting, not CPU-bound.** Raw processes keep the VM 94% busy.
  Host jobs leave a third of it idle, at every cap.

The session's host windows (5,100/s) beat the release script's (3,766.7/s) on
the same binary. In the script, each host window follows container scenarios,
and a grant cycle there takes 4 to 5 s instead of 3.1 s
(`baseline/r2-jobs-host-processes.out` against `caps/c27-r2-host.out`). A
likely cause is the third finding: the GitOps loop clones a `DesiredState`
that the container scenarios have made bigger, under the state machine's read
lock. I didn't isolate it further.

**Medians move in quanta.** The bench counts *accepted* successes, and
receipts arrive one whole grant cycle at a time, so a window's count moves in
steps of 16,000 tasks (266.7/s). Every cap-27 and cap-32 window accepted
exactly 306,000 = 2,000 + 19 × 16,000. The release script's windows accepted
226,000 or 290,000, four cycles apart. Five-round medians can't see a change
smaller than one cycle until the lockstep is fixed. Read the steady column too.

## Where the time goes

### On-CPU

The profiling build, cap 27, `perf record -F 499 -a -g` for 20 s in the middle
of a window: 39,920 samples over 4 vCPUs. I put each sample in one category
with [`tools/categorise.py`](tools/categorise.py). Bun's samples are picked by
process ID; relish's tokio threads have the same thread name. Per-task numbers
divide the VM's CPU time by the 4,688 tasks/s that window ran. Bun's fixed
costs therefore show up amortised per task.

| Rank | Cost | Share of VM CPU | µs per task | Raw scenario |
|---:|---|---:|---:|---:|
| 1 | Idle: no runnable work (grant gaps) | 35.0% | 298 | 7.6% |
| 2 | Bun: GitOps loop, `DesiredState` clone and drop per Raft metrics change | 12.4% | 106 | 15.7% |
| 3 | Task process lifecycle (clone3, execve, exit and teardown, page faults, BusyBox itself) | 23.0% | 196 | 106 µs + relish's spawner |
| 4 | Helper: poll, pipes, `waitid`, `/proc` children scan and socket syscalls, plus its own code | 8.4% | 72 | — |
| 5 | Bun: per-task pool path (below) | 7.5% | 64 | — |
| 6 | Bun: runtime, timers, wakeups, other subsystems | 6.6% | 56 | 5.1% |
| 7 | Bun: Raft core and log | 3.3% | 29 | 3.8% |
| 8 | Bun: task ledger (group commit, fsync) | 0.07% | 0.6 | — |
| 9 | Bun: task-array leader tick and node sync | 0.01% | 0.1 | — |
| — | Other processes and kernel threads | 3.8% | 32 | 3.2% |

The task process lifecycle (rank 3) splits into clone3 32 µs, execve 49 µs,
exit and address-space teardown 35 µs, page faults 47 µs and BusyBox's own run
34 µs. The raw scenario pays 13 µs, 33 µs, 27 µs and 33 µs for the last four.
Its spawner, relish, uses `posix_spawn`, so creation shows up in relish (62 µs
per task with its runtime). The difference is mostly fork-style page faults:
the helper's `clone3` has no `CLONE_VM`.

Bun's per-task pool path (rank 5) splits into:

| Item | µs per task |
|---|---:|
| Helper protocol (socket reads and writes) | 15.1 |
| Rest of `ReusablePool::run`: slots lock (two or three times per task), the 100 ms policy timer created per attempt, `LiveAttempt`, logs | 14.8 |
| Chunk streaming and attempt bookkeeping (`resume_chunk_streaming`, `TaskAttempts`, invocation clone) | 11.9 |
| `wait_empty_task`'s `cgroup.events` read | 8.8 |
| Namespace policy check (two tokio mutexes, one of them the global kernel lock, plus a BPF map read) | 7.6 |
| OCI spec and host environment rebuilt per task | 4.9 |
| `OwnedRunner::run` outside the pool (template, admission) | 0.5 |

Per task, Bun's own path costs about 64 µs, and the helper and process about
270 µs. With the GitOps loop gone (E2) the whole VM spends 313 µs of CPU per
host task and 160 µs per raw one. That puts a CPU-bound ceiling for host jobs
on this 4-vCPU VM near 12,800/s.

Flamegraphs (hover a frame for its name and share; frames under 0.2% are
hidden):

- [`flamegraphs/host-oncpu-vm.svg`](flamegraphs/host-oncpu-vm.svg): the whole VM, host jobs, cap 27
- [`flamegraphs/host-oncpu-bun.svg`](flamegraphs/host-oncpu-bun.svg): Bun only
- [`flamegraphs/raw-oncpu-vm.svg`](flamegraphs/raw-oncpu-vm.svg): the whole VM, raw processes (Bun idle beside them)
- [`flamegraphs/host-cap32-oncpu-vm.svg`](flamegraphs/host-cap32-oncpu-vm.svg): host jobs at cap 32, for comparison

The C helper is stripped, so its user-space frames show as `[helper]`; its
kernel frames are named. perf printed no command name for many short-lived task
processes (`:PID`). Those are BusyBox before and after exec, and the
categoriser counts them so.

### Off-CPU and scheduling

`perf record -a -g -e sched:sched_switch` for 5 s, weighted by the time each
thread stayed switched out ([`tools/flame.py`](tools/flame.py) `fold-off`):

- [`flamegraphs/host-offcpu-helpers.svg`](flamegraphs/host-offcpu-helpers.svg).
  The 27 executor helpers spend **94% of their time off-CPU**. Of that, 63% is
  blocked in `read()` on the control socket, waiting for Bun's next command or
  cleanup request. Another 28% is in `ppoll`, waiting for the child, and 4.6%
  is in `execve`. Most of that `read()` time is the grant gap: the helper has
  nothing to do.
- [`flamegraphs/host-offcpu-bun.svg`](flamegraphs/host-offcpu-bun.svg). 97% of
  Bun's off-CPU time is idle blocking-pool threads parked on a condition
  variable. Async waits don't show in off-CPU stacks, because tokio parks the
  worker rather than the future, so the probe below is the evidence for
  Bun's side.

`perf sched latency` ([`profile/c27-perfsched-host.latency.txt`](profile/c27-perfsched-host.latency.txt))
shows run-queue delays too small to matter: helpers 0.28 ms on average (18.6 ms
worst), BusyBox 0.29 ms, tokio workers 0.07 ms.

### The grant cycle, seen from the node

A probe window polled `relish batch-status` every 0.15 s
([`caps/c27-probe-host.timeline.txt`](caps/c27-probe-host.timeline.txt)). The
node's counters update once per sync, about once a second. The cycle repeats
like this:

```
   t(s)   held  accepted  started   active
   2.37  16000     34000    44106       24    <- sync: 10,104 started since the last one
   3.49  16000     50000    50000        0    <- all 16 chunks done, receipts accepted
   5.39  16000     50000    60157       19    <- 1.9 s later, the next grant is running
   6.35  16000     66000    66000        0
   8.42  16000     66000    76192       16
```

`held` sits at 16,000 tasks, the 16-chunk cap. `active` drops to 0 at the end
of each cycle, and nothing starts for about 1.9 s. The `vmstat` and on-CPU
timelines show the same thing. Here's half-second buckets of the 20 s profile:
helper samples drop to zero for 1 to 2 s every 3 s, while idle climbs to 65%.

```
t(s)  all idle busybox helper DesiredState-clone RaftCore resume_chunk_streaming
1.00  997  350       0    173                 64       30                     90
1.50  999  671       0      0                116       50                      0
2.00  997  597       0     23                 85       45                      9
2.50  999   94       1    358                 49       11                    125
```

### Start lag and the pool handoff

The node's `start_lag_ms_counts` (from the probe's `batch-status`) put 76% of
attempts at ≤1 ms from slot to helper start, 90% at ≤2 ms, 99% at ≤8 ms and a
tail of 44 out of 28,423 at 65 to 128 ms. The commands' own run time
(`duration_final_attempt_ms`) is ≤1 ms for 85% of attempts. The handoff isn't
the bottleneck.

### What the issue asked about

| Item | Finding |
|---|---|
| Pool handoff and start lag | Fast: p50 ≤1 ms, p99 ≤8 ms. Not a limit |
| Process spawn | The helper uses `clone3(CLONE_INTO_CGROUP)` without `CLONE_VM`, so the child is a fork with copy-on-write faults (32 µs clone, 47 µs faults, 49 µs exec, 35 µs exit per task). The environment is built per task (5 µs) and the child sets up its fds and drops capabilities before `execvp` |
| Per-attempt ledger writes and fsyncs | Group commit (100 ms or 4,096 records) with one `sync_data` per commit: 0.07% of VM CPU, I/O wait 0.2–0.4%. Not a cost |
| Chunk receipts | **The main cost.** Held chunks run in lockstep and finish together, so receipts arrive once per cycle and the node drains to zero before the next grant |
| Agent loop and task-array tick | 0.01% of CPU. Its 1 s cadence matters only through the receipt-to-grant round trip above |
| cgroup and accounting work per attempt | No cgroup is created per attempt: the executor reuses its task cgroup through `CLONE_INTO_CGROUP`. Per attempt there's one `cgroup.events` read (9 µs) and the policy check (8 µs). Accounting reads run per tick |
| Cap of 27, CPU-bound or waiting | Waiting: 27 to 32 changes nothing, and a third of the CPU is idle at any cap |

## Experiments

Two throwaway builds (never committed), the same compiler and flags as the
release-style build, each measured on a fresh boot with
`tools/run.sh caps e1|e2`. That's one probe window, then five rounds of host
and raw at cap 27. Their patches are
[`tools/e1.patch`](tools/e1.patch) and [`tools/e2.patch`](tools/e2.patch).

| Build | Host jobs, median (range) | Steady | Host CPU busy | Burst (per sync) | Raw, median (range) | Bun CPU in a raw window |
|---|---:|---:|---:|---:|---:|---:|
| Release-style | 5,100.0/s (5,100.0–5,100.0) | 5,149/s | 67% | ~10,000/s | 18,026.5/s (17,947.8–18,169.0) | 60.8 s of 62 |
| E1: `MAX_LEARNED_GRANT_DEPTH` 16 → 64 | 6,766.7/s (6,616.7–7,000.0) | 7,079/s | 77% | ~11,000/s | 18,324.5/s (18,188.7–18,406.5) | 61.0 s |
| E2: E1, and the GitOps loop no longer wakes on metrics changes | 7,066.7/s (6,866.7–7,766.7) | 8,465/s | 60% | ~13,500/s | 22,074.3/s (21,866.7–22,161.3) | 0.7 s |

E1 grew `held` to 42 chunks, the depth the lookahead learns from these
durations, and the cycle to 4 to 5 s. It still drains to zero before each
refill ([`experiment-e1/c27-probe-host.timeline.txt`](experiment-e1/c27-probe-host.timeline.txt)).
E2 shows the same shape with faster bursts
([`experiment-e2/c27-probe-host.timeline.txt`](experiment-e2/c27-probe-host.timeline.txt)),
which is why its CPU is *less* busy than E1's: the work runs faster and then
waits just as long. Neither experiment fixes the lockstep. Both are evidence
for what fixing it is worth.

## Proposed changes for step 2 (#1030), ranked

1. **Dispatch held chunks in order, and receipt each one as it finishes.**
   Feed the slots from the lowest held chunk first, so chunk N's tasks are all
   dispatched before chunk N+1's start. Chunks then finish one after another,
   each sync carries receipts, and the leader tops the node up every tick
   instead of once per cycle. Expected gain: the node runs at its burst rate
   all the time, about 10,000/s with today's code and about 13,000/s with
   change 2 on this rig, from 5,100/s now. That clears #1030's 8,000/s on its
   own. Correctness risk: low. It changes dispatch order, not what runs or
   what's recorded. Retries, per-chunk timeouts, cancellation and replay must
   keep their per-chunk fences, and a slow task still delays only its own
   chunk's receipt.
2. **Stop the GitOps loop cloning the cluster on every Raft commit.** Wake on
   a real leadership change (term or leader ID), not on any metrics update.
   Check leadership and whether GitOps is configured before taking the full
   `DesiredState`, and read only `gitops_config` for that. Expected gain:
   frees about one core per node. E2 measured +20% steady host rate on top of
   E1, and +22% on the raw baseline. Once change 1 makes the node CPU-bound,
   that core is worth roughly another 20 to 25%. It may also shorten the grant
   cycle in long sessions, where a bigger state makes each clone slower.
   Correctness risk: low to medium. A leader must still start syncing
   promptly when it gains leadership and when a trigger or webhook arrives, so
   it needs a test for each wake source.
3. **Learn a deeper grant from throughput, not just the 16-chunk cap.** E1
   alone gave +33%. With change 1 in place, depth matters less, because
   receipts flow every tick. Keep a small rise as a cushion for the 1 s sync.
   Correctness risk: medium. More held work per node means more unknown
   outcomes when a node is lost (automatic-replay arrays only; manual-replay
   arrays keep the baseline window) and a lumpier tail across nodes. The
   per-node share cap bounds it.
4. **vfork-style spawn in the helper.** Add `CLONE_VM | CLONE_VFORK` to the
   `clone3(CLONE_INTO_CGROUP)` call, and pass the environment to `execve`
   instead of assigning `environ` in the child. Expected gain: most of the
   47 µs of page faults and part of the 32 µs clone, about 50 µs of the 313 µs
   per task, so roughly 15% once CPU-bound. Correctness risk: medium. Under
   `CLONE_VM` the child must not write the parent's memory or call anything
   that allocates or takes locks; today's `perror` calls on the error paths
   use stdio. The capability drop and `PDEATHSIG` steps need review.
5. **One round trip per task between Bun and the helper.** Let the helper
   reap and report `Exited` together with its cleanup receipt, instead of
   waiting for Bun's cleanup request. Skip the `/proc/self/task/*/children`
   scan when the only child has been reaped. Expected gain: part of the
   helper's 72 µs and Bun's 15 µs protocol cost, plus two wakeups per task,
   about 5 to 10%. Correctness risk: medium. The #715 drain-after-exit rules
   and descendant reaping must still hold.
6. **Trim Bun's per-task path.** Check the namespace policy on the existing
   100 ms monitor rather than before every command. Stop creating a policy
   timer per attempt. Build the OCI process once per executor and patch only
   args and env. Take the `slots` lock once per task. Expected gain: about 25
   to 35 of the 64 µs, 5 to 10%. Correctness risk: low, except the policy
   check: moving it off the command path must not let a command start after
   enforcement is lost. Gate it on the same generation the monitor checks.
7. **Not worth doing for throughput:** the ledger and its fsyncs, cgroup
   setup, the leader tick's CPU and the concurrency cap. Each is under 0.1% of
   CPU or made no measurable difference.

Measure #1030 by the steady rate as well as the accepted median, until change
1 removes the 16,000-task quanta.

## Commands

All scripts are in [`tools/`](tools), as run. Each session ran as root in the
VM on a fresh boot, after `limactl stop rb-bench-1029 && limactl start rb-bench-1029`.

```sh
# The VM.
limactl create --name rb-bench-1029 --cpus 4 --memory 8 --disk 60 --vm-type vz \
    --mount-none --tty=false template:ubuntu-24.04
# Inside: build-essential clang llvm libbpf-dev pkg-config runc linux-tools-$(uname -r)
# python3 busybox-static, and rustup with 1.98.0. A 16 GiB swap file was on for
# builds only: the 8 GiB VM can't link the crate with one codegen unit without it.

# Builds (tools/build.sh, then tools/experiments.sh for e1 and e2).
cargo build --locked --release --features ebpf --bin bun --bin relish
CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=line-tables-only \
    RUSTFLAGS="-C force-frame-pointers=yes" \
    cargo build --locked --release --features ebpf --bin bun --bin relish

# Sessions (tools/run.sh wraps tools/session.sh).
run.sh prof          # profiling build: 2 host windows, perf -g at caps 27 and 32, sched_switch, perf sched
run.sh baseline 2 5  # the unchanged job_bench_rounds.sh, five rounds
run.sh caps rel      # probe, then five rounds of host and raw at caps 27, 32 and 16
run.sh caps e1       # probe, then five rounds at cap 27
run.sh caps e2

# Profiling, inside a window (tools/session.sh).
perf record -F 499 -a -g -o W.perf.data -- sleep 20
perf record -a -g -e sched:sched_switch -o W.perf.data -- sleep 5
perf sched record -a -o W.sched.data -- sleep 5

# Post-processing (tools/post.sh, tools/flame.py, tools/categorise.py).
perf script -i W.perf.data --no-inline -F comm,pid,tid,time,event,ip,sym,dso > W.script
flame.py fold-cpu W.script [--tgid BUN] > W.folded
flame.py fold-off W.script --tgid BUN | --comm helper > W.off.folded
flame.py svg W.folded --title T > W.svg
categorise.py SYSTEM.folded BUN.folded TASKS_PER_SECOND
perf sched latency -i W.sched.data --sort max
```

`tools/flame.py` folds and renders with the standard library only. It includes
a small Rust v0 symbol demangler, since perf 6.8 doesn't demangle v0 names. No
FlameGraph or inferno download was needed.

## Files

| Path | What |
|---|---|
| `baseline/` | The repeat run of `job_bench_rounds.sh`: 20 `relish bench --report` JSONs, `progress.log`, `binaries.txt` |
| `caps/` | Cap 16, 27 and 32 rounds and the probe window: reports, `summary.txt` (per-window CPU accounting), the probe's `batch-status` and timeline |
| `experiment-e1/`, `experiment-e2/` | The same for the two experiments |
| `profile/` | The profiling session: `summary.txt`, the `perf` top-frame lists, the category tables, `perf sched latency`, a `vmstat` |
| `flamegraphs/` | Six SVGs, on-CPU and off-CPU |
| `host-load.log`, `host-busy.log` | The Mac's load every 10 s; the second also logs the VM's CPU, everything else's CPU and the count of `rustc`/`cargo` processes |
| `tools/` | Every script as run, and the two experiment patches |

The raw `perf.data` files (60 to 160 MB each) and the `perf script` output
stayed in the VM.

# Host-job throughput after the #1030 changes (26 Oct 2026)

Step 2 of the 0.2.3 throughput goal ([#1030](https://github.com/reliaburger/reliaburger/issues/1030)):
measure single-node host-job throughput after the changes that landed for it,
against the same build the [step-1 profile](../2026-10-20-host-job-profile/README.md)
measured. The target is at least 8,000 host jobs a second on one 4-vCPU node,
as a five-round median, with no terminal failures and cleanup verified.

**Result: 7,500.0/s (7,483.3–7,500.0), up from 5,100.0/s. Target not met.**
The steady rate between 15 and 58 s is 7,921/s. Every host-job window
completed, with no terminal failures or retries, and cleanup verified.

What limits it now is the **grant round trip**, not CPU. The node finishes
everything it holds well inside one leader tick and then waits. A grant the
leader commits at tick *k* reaches the node at tick *k*+1, and its receipt is
accepted at tick *k*+2. The node holds at most 16 chunks of 1,000 tasks, so it
can't turn over more than 16,000 tasks every two 1-second ticks: **8,000/s is
the ceiling**, and we measure 7,921/s steady. The VM's CPU is 60% idle. In its
busy 0.8 s of each 2 s cycle it runs close to 19,000 tasks a second.

Two changes would lift that ceiling, and neither is done:

- **Change 3 from the profile**, a deeper learned grant depth. 32 chunks would
  double the ceiling to 16,000/s.
- **A new change**: get a grant to the node in the same tick that frees its
  room, so a chunk turns over in one tick instead of two. That also doubles the
  ceiling, without letting the node hold more work.

Either one would leave CPU as the next limit, at about 18,000/s on this rig.
Change 5 (one helper round trip per task) makes bursts cheaper, but bursts
aren't the limit while the VM is idle 60% of the time.

This is not the profile's hardware. The profile ran on an M1 Max. This record
ran on an **M2 Max**, the 0.2.0 throughput record's model. So compare the
baseline and current rows here with each other, not with the profile's
numbers. One thing does carry over: the baseline reproduces the profile's
host-job figures exactly (5,100.0/s in every cap-27 window, 3,766.7/s median
in the release script). Grant cycles bound host jobs, not CPU speed.

## The rig

| Item | Value |
|---|---|
| Host | Apple M2 Max (Mac14,5), 12 cores, 32 GiB, macOS 26.3.1 (Darwin 25.3.0). **Not** the profile's M1 Max |
| VM | `rb-bench-1030`, Lima 2.1.0 (VZ), 4 vCPU, 8 GiB, 60 GiB disk, `template:ubuntu-24.04`, `--mount-none`. Made for this run, left created and stopped |
| Guest | Ubuntu 24.04 aarch64, Linux **6.8.0-147-generic** for every measurement in the tables below (6.8.0-101 for the first pair, see [Discarded and repeated runs](#discarded-and-repeated-runs)), runc 1.4.0, perf 6.8.12 |
| Baseline source | `f68755d9`, the profile's source commit, before any #1030 change |
| Current source | `origin/release-0-2-3` at `259b6061` (#1265, #1346, #1351 and #1357) |
| Release-style builds | Rust 1.98.0 (`build.yml`'s pin), `cargo build --locked --release --features ebpf --bin bun --bin relish`, stripped as `[profile.release]` says ([`tools/build.sh`](tools/build.sh)). Baseline `bun` `a36349cf…e304`, `relish` `dbcf5a85…af60`. Current `bun` `d11bb1ee…a944`, `relish` `afcfe829…195c` |
| Profiling build | Current source, the same plus `CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=line-tables-only RUSTFLAGS="-C force-frame-pointers=yes"`. `bun` `6b985401…db2d`, `relish` `f1751c19…c833` |
| BusyBox, cap sessions | Ubuntu's `busybox-static` 1:1.36.1-6ubuntu3.1 at `/opt/bench/busybox`, SHA-256 `52151e7f…e933`, the same file as the profile's |
| BusyBox, release script and `--compare` | The pinned image's `/bin/busybox`, SHA-256 `f1947045…c0a5`, extracted with [`tools/fetch_busybox.py`](tools/fetch_busybox.py): the one the script's usage text names and the [0.2.0 throughput record](../2026-10-17-v0.2.0-job-throughput/README.md) ran |
| Scenario | `relish bench` defaults: 60 s windows, concurrency 27, 25m CPU request, one-core limit, 32 MiB, 1,000-task chunks, beside a 500m BusyBox web app on a fresh single-node cluster |

The full hashes are in each directory's `binaries.txt`. The baseline `bun`
doesn't match the profile's `cab1d7cb…f83b`, although the source and compiler
are the same. Paths are compiled into the binaries, and this VM's home directory
and source path differ.

Every session started on a fresh boot of the VM, as the profile requires.

### Host load

[`host-busy.log`](host-busy.log) samples the Mac every 10 s from 04:52 to
08:09 UTC: the 1-minute load, the number of `rustc` and `cargo` processes, the
VM's CPU and everything else's CPU. It also serves as the profile's
`host-load.log`, since [`tools/loadcheck.py`](tools/loadcheck.py) reads its
load column.

- **No compiler ran on the Mac at any time**: `rustc=0 cargo=0` in all 1,102
  samples. Both builds ran inside the VM, before any measurement.
- Everything outside the VM used 65% of one core on average.
- One process from another session was running throughout: a
  `target/debug/bun __process-owner` test fixture, orphaned 28 days ago, at
  about 2% of a core. I left it alone.
- As in the profile, the busy 4-vCPU VM adds about 4 to the load. So a strict
  "discard above 5" rule discards most windows here too: 5 of 11 in the
  baseline cap session, 1 of 11 in the current one, 16 of 20 and 13 of 20 in
  the two release-script sessions. Window-edge loads ranged from 1.79 to 7.33.
  I kept those windows, as the profile did, for the same reason: the host-job
  windows barely move with it. The current build ran between 7,466.7 and
  7,500.0/s in all ten cap-27 windows, on two kernels.

## Rates

### Cap 27, with CPU accounting

`tools/run.sh caps base|cur` ([`tools/session.sh`](tools/session.sh)): one
probe window, then five rounds of host and raw windows at cap 27, alternating
order. Each window also records `vmstat`, `/proc/stat`, Bun's own CPU and
cgroup CPU. The per-window table is in each directory's `summary.txt`.

| Build | Host jobs, median (range) | Steady 15–58 s | Host CPU busy | Raw, median (range) | Raw CPU busy | Bun CPU in a raw window |
|---|---:|---:|---:|---:|---:|---:|
| Baseline `f68755d9` | 5,100.0/s (5,100.0–5,100.0) | 5,161/s | 60% | 23,519.3/s (23,472.1–23,664.7) | 93% | 60.5 s of 62 |
| Current `259b6061` | **7,500.0/s (7,483.3–7,500.0)** | **7,921/s** | 39% | 29,604.6/s (29,540.7–29,682.4) | 89% | 0.5 s |
| Change | +47.1% | +53.5% | | +25.9% | | |

Every window completed, with no terminal failures or retries, and cleanup
was verified in each. The VM's CPU per started process in host windows fell
from 0.454 ms to 0.204 ms. Bun's own CPU in a host window fell from 83.6 s to
20.2 s (medians).

The raw scenario doesn't run through Bun, yet it gained 26%. In the baseline,
Bun burned a whole core with no tasks to run, which was the GitOps loop
(#1346). Now Bun uses 0.5 s of CPU in a 62 s raw window.

### The unchanged release script

[`scripts/release/job_bench_rounds.sh`](../../../scripts/release/job_bench_rounds.sh)
`--rounds 5`, the release-style builds, the pinned image's BusyBox:

| Scenario | Baseline, median (range) | Current, median (range) | Change |
|---|---:|---:|---:|
| Raw VM processes | 12,587.3/s (10,678.4–13,366.1) | 16,912.2/s (13,711.2–17,484.6) | +34.4% |
| Fresh containers | 12.9/s (12.8–13.3) | 13.4/s (13.2–13.6) | +3.9% |
| Shared containers | 2,583.3/s (2,316.7–2,750.0) | 6,483.3/s (6,266.7–6,683.3) | +151.0% |
| Host jobs | 3,766.7/s (3,766.7–4,833.3) | **7,500.0/s (7,383.3–7,500.0)** | +99.1% |

Every window completed, with no terminal failures and cleanup verified. The
fresh-container windows accepted retries, which later succeeded: one in the
baseline (round 3) and three in the current run (one in round 4, two in
round 5). The other scenarios had none. The host-job steady rate is 3,970/s
for the baseline and 7,940/s now.

In the profile, host windows ran slower inside the release script than in the
cap sessions: 3,766.7/s against 5,100.0/s. In the release script, each host
window follows the container scenarios. The likely cause was the GitOps loop
cloning a `DesiredState` those scenarios had grown. That gap is gone: the
current build runs 7,500.0/s in both. Shared containers gained the most,
2.5 times. They use the same dispatch path and Bun's freed core, but this
record didn't profile them.

Raw rates fall as the release-script session goes on, in both builds:
13,321 to 10,796 in the baseline and 17,485 to 13,711 now. They don't fall in
the cap sessions, which run no container scenarios. The release script records
no CPU accounting, so this record can't say what grows. It doesn't touch host
jobs, which hold at 7,500.0/s through round 5.

### `relish bench --compare` against 0.2.0

#1030 asks for each change's effect as `relish bench --scenario host --compare`
against the 0.2.0 reports on the same rig. `tools/run.sh compare cur` ran five
host-job and five shared-container windows on a fresh cluster. Each one ran
with `--compare` against the same round's report in the
[0.2.0 throughput record](../2026-10-17-v0.2.0-job-throughput/README.md). Two
things make those comparisons like for like:

- That record ran on an M2 Max too.
- The pinned image's BusyBox sat at the record's own path, `/opt/bench/busybox`.

So no comparison was informational: same window, concurrency, CPU request,
image and executable path. The reports carry no executable hash
(`executable_sha256` is null in both), so Relish can't check that the bytes
match. They do: `f1947045…c0a5` both times.

| Round | Host jobs, 0.2.0 → now | Change | Shared containers, 0.2.0 → now | Change |
|---:|---:|---:|---:|---:|
| 1 | 4,666.7 → 7,383.3 | +58.2% | 2,833.3 → 6,816.7 | +140.6% |
| 2 | 4,683.3 → 7,500.0 | +60.1% | 3,083.3 → 6,800.0 | +120.5% |
| 3 | 4,833.3 → 7,500.0 | +55.2% | 3,100.0 → 6,783.3 | +118.8% |
| 4 | 3,766.7 → 7,383.3 | +96.0% | 2,916.7 → 6,816.7 | +133.7% |
| 5 | 3,766.7 → 7,383.3 | +96.0% | 2,700.0 → 6,783.3 | +151.2% |
| **Median** | **4,666.7 → 7,383.3** | **+58.2%** | **2,916.7 → 6,800.0** | **+133.1%** |

No window regressed, and none had a terminal failure or retry. Cleanup was
verified in all ten. The comparisons are in `compare-vs-0.2.0/*-vs020.out`.

The 0.2.0 side is the published table, measured with the release script. On
the current side, the host median (7,383.3/s) is one or two chunks a window
below the 7,500.0/s of the cap and release-script sessions. I didn't isolate
why.

## What changed

All four changes were measured together. This record has no per-change split:
each PR showed its own effect in its tests, and this is the five-round
measurement they deferred to.

| Change | PR | What it did, as seen here |
|---|---|---|
| 1. Dispatch held chunks in order, receipt each as it finishes | [#1265](https://github.com/reliaburger/reliaburger/pull/1265) | Each chunk gets its own receipt as it finishes, so accepted counts move in steps of 1,000, not 16,000: 443,000 and 449,000 both occur. A burst of 15 chunks now finishes inside one tick |
| 2. GitOps loop wakes only on leadership changes | [#1346](https://github.com/reliaburger/reliaburger/pull/1346) | Bun's CPU in a raw window went from 60.5 s to 0.5 s. The GitOps category is empty in the profile below (it was 12.4% of VM CPU). Release-script host windows no longer run slower than cap-session ones |
| 4. vfork-style spawn in the executor helper | [#1351](https://github.com/reliaburger/reliaburger/pull/1351) | Per task, the helper's child pays 12.8 µs in `clone3` and 23.7 µs in page faults, against 32.1 µs and 46.7 µs in the profile (on the M1 Max, so read it as direction, not size) |
| 6. Per-task trims in Bun's pool path | [#1357](https://github.com/reliaburger/reliaburger/pull/1357) | Bun's own per-task path is 35.6 µs, against 63.6 µs in the profile. The faster host counts in that too, so this doesn't isolate the trims |

## What limits it now

### The node drains every tick

The probe window polled `relish batch-status` every 0.15 s
([`caps-cur/c27-probe-host.timeline.txt`](caps-cur/c27-probe-host.timeline.txt)).
The node's counters change once per sync, so each sync shows up as a jump:

```
   t(s)   held  accepted  started  started/s  active
   0.81  16000     50000    50001       6074       0
   1.76  16000     65000    65000      95840       0    <- 15 chunks run and accepted since the last sync
   2.73  16000     66000    66002       6069       0    <- 1 chunk
   3.85  16000     81000    81000      94006       0    <- 15 chunks
   4.82  16000     82000    82000       6106       0    <- 1 chunk
```

`held` sits at the 16-chunk cap, 16,000 tasks. Every chunk the node starts
has finished and been accepted by the next sync. `started` never runs ahead of
`accepted` at a sync, and `active` is 0 at all 149 samples (the baseline had
`active` above zero at 48 of 150). So `active` still drops to zero between
grants. What changed is that nothing is left running when a sync happens.

The 16 chunks run as two cohorts, 15 and 1, a tick apart. Each cohort turns
over every two ticks, so 16,000 tasks complete every 2 s.

### Why two ticks

Each leader tick (`leader_tick` in `src/bun/task_array_leader.rs`, every
`SYNC_INTERVAL` = 1 s) does three things:

1. It reads the committed state and sends each node a request with its current
   grants.
2. It collects the nodes' answers: progress and finished chunks.
3. It commits one `Sync` entry per array with the results and the new grants
   (`plan_sync`, which plans grants as if the results were applied).

The grants committed in step 3 go out with the next tick's step 1. Nothing
sends them sooner. So a grant committed at tick *k* reaches the node at tick
*k*+1. The node runs it in well under a second, but can only report it at tick
*k*+2, when its replacement is committed, and that replacement reaches the node
at tick *k*+3. Each of the node's 16 held chunks therefore turns over once every
two ticks, however fast the tasks run. That gives the ceiling:

> 16 chunks × 1,000 tasks ÷ (2 × 1 s) = **8,000 tasks/s**

The measured steady rate, 7,921/s, sits just under it. The five-round median,
7,500.0/s, also counts the window's first seconds, before the first grant
cycle.

### The CPU sits idle half of each cycle

The profiling build, cap 27, `perf record -F 499 -a -g` for 20 s in the middle
of a window, put in 0.1 s buckets ([`profile/c27-perfcpu-host.timeline.txt`](profile/c27-perfcpu-host.timeline.txt),
200 samples per bucket is all four vCPUs):

```
t(s)  all  idle  helper  task_array_node  ReusablePool
0.70  198   193       0                0             0
0.80  200   136      21                9             8    <- grant arrives: the 15-chunk burst
0.90  200     6      59               31            26
 ...                                                       (0.8 s at ~98% busy)
1.50  200     3      80               40            32
1.60  200   133      18               10             9
1.70  200   197       0                1             0
1.80  200   134      20                9             5    <- the 1-chunk cohort, ~0.2 s
1.90  198   134      21               12             8
2.00  198   197       0                0             0
 ...                                                       (~0.8 s idle)
2.80  200   145      13                9             7    <- next burst
```

In each 2 s cycle the VM spends about 0.8 s running the 15-chunk burst at
about 98% CPU, about 0.2 s on the single chunk, and about 1 s with nothing to
do. Fifteen thousand tasks in about 0.8 s is a burst rate close to 19,000/s. Over
the whole window the VM is idle 58% of the time.

### Where the CPU that is used goes

The same profile, with each sample put in one category by
[`tools/categorise.py`](tools/categorise.py), at the 7,488 tasks/s that window
ran ([`profile/c27-host-categories.txt`](profile/c27-host-categories.txt)).
The profile's figures are from the M1 Max and the baseline build, so they show
direction, not a like-for-like size:

| Cost | µs per task now | Profile (M1 Max, baseline) |
|---|---:|---:|
| Idle: no runnable work | 312.1 | 298.3 |
| Bun: GitOps loop | 0 | 106.2 |
| Bun: Raft core and log | 0.2 | 28.5 |
| Bun: per-task pool path (policy check, `wait_empty_task`, helper protocol, OCI process, rest of `ReusablePool::run`, `OwnedRunner`, chunk streaming) | 35.6 | 63.6 |
| Bun: runtime, timers, wakeups, other | 7.5 | 55.9 |
| Helper's child: `clone3` | 12.8 | 32.1 |
| Helper's child: `execve` | 28.2 | 48.6 |
| Helper's child: exit and teardown | 25.4 | 34.6 |
| Helper's child: page faults | 23.7 | 46.7 |
| Helper: wait, poll, pipes and socket syscalls | 39.1 | 54.9 |
| Helper: user space and other kernel | 6.2 | 16.7 |
| BusyBox `true` itself | 31.8 | 33.5 |
| Other processes and kernel threads | 11.0 | 32.4 |
| Ledger, leader tick and relish | 0.7 | 1.1 |
| **Busy CPU per task** | **about 222** | **about 555** |

At about 222 µs of CPU per task, four vCPUs could run about 18,000 host jobs a
second, close to the burst rate above. That is the next ceiling, once the
grant round trip is gone. The largest single item left is the helper's wait,
poll, pipe and socket work (39 µs), which change 5 targets. But per-task CPU
is not what limits the rate now, so cutting it won't move the median until the
node stops waiting.

Flamegraphs (hover a frame for its name and share):

- [`flamegraphs/host-oncpu-vm.svg`](flamegraphs/host-oncpu-vm.svg): the whole VM, host jobs, cap 27
- [`flamegraphs/host-oncpu-bun.svg`](flamegraphs/host-oncpu-bun.svg): Bun only
- [`flamegraphs/raw-oncpu-vm.svg`](flamegraphs/raw-oncpu-vm.svg): the whole VM, raw processes
- [`flamegraphs/host-offcpu-helpers.svg`](flamegraphs/host-offcpu-helpers.svg): the 27 helpers off-CPU, `sched:sched_switch` for 5 s.
  61% of their off-CPU time is blocked in `read()` on the control socket,
  waiting for Bun's next command: the idle second of each cycle

### What the evidence points at

| Option | What it changes | Ceiling on this rig | Risk |
|---|---|---:|---|
| Change 3: a deeper learned grant depth (`MAX_LEARNED_GRANT_DEPTH` 16 → 32) | The node holds twice the work, so each two-tick turnover moves 32,000 tasks | 16,000/s by grants, about 18,000/s by CPU | Medium, as the profile said: more held work means more unknown outcomes when a node is lost (automatic-replay arrays), and a lumpier tail. The per-node share cap still bounds it |
| New: deliver grants in the tick that frees the room | A chunk turns over in one tick, not two: the leader sends the grants it has just committed at once, rather than with the next tick's request | 16,000/s at the same 16-chunk depth | Lower for held work, since the node holds no more than now. But it changes when syncs happen. The control-version fence a node checks and writes on every new version must stay monotonic, and a second request per tick costs each node that gets one another fence write and fsync |
| Halve `SYNC_INTERVAL` | Same effect as one-tick turnover | 16,000/s | Doubles every node's sync traffic and the leader's per-tick work across the cluster, for every array. A blunt way to get what the previous row gets |
| Change 5: one round trip per task between Bun and the helper | Cheaper bursts | Unchanged: 8,000/s | Doesn't help until the node is CPU-bound |

The two doubling changes stack: either one alone clears 8,000/s on paper and
moves the limit to CPU. Change 5 and the remaining per-task trims are worth
doing after that, when CPU is the limit again. The concurrency cap stays at
27. Nothing here suggests changing it, so the manual's batch-jobs chapter is
unchanged.

## Discarded and repeated runs

- **`discarded/rounds-base-unattended-upgrades/`: the first release-script run
  of the baseline, discarded.** Ubuntu's `unattended-upgrades` started at
  06:13:49 UTC in the middle of round 2 and ran until 06:24:21. It upgraded
  glibc and systemd, re-executed systemd, and **stopped and restarted
  `rb-bun`** at 06:17:41 (journal). Round 3's host window failed: Bun wasn't
  reachable, the measurement was incomplete and cleanup couldn't be verified.
  Rounds 3 to 5 then ran at a third to a half of round 1's rate (host 1,300
  to 1,900/s against 3,766.7, raw 5,898 to 7,162/s against 13,347). The upgrade also installed kernel 6.8.0-147, which
  every later boot used. After it, I disabled the apt, fwupd, motd-news,
  man-db and update-notifier timers, masked the apt services and held snap
  refreshes, then repeated every session the upgrade could have touched. The
  directory keeps the reports and the summary for reference.
- **`kernel-101/`: the first cap-27 pair, on Linux 6.8.0-101, repeated.** Both
  sessions ran cleanly, with no apt activity in their boots' journals, before
  the upgrade. I repeated them on 6.8.0-147 so every number in the tables comes
  from one kernel. They agree: baseline 5,100.0/s (5,100.0–5,100.0), steady
  5,161/s, raw 23,216.0/s. Current 7,500.0/s (7,466.7–7,500.0), steady 7,921/s,
  raw 29,398.6/s.
- **The current build's release-script run** ran on the first boot after the
  upgrade, on 6.8.0-147, with the timers still enabled. Its boot journal shows
  no apt or upgrade activity, so it stands.
- No other window was discarded, and none failed. The very first attempt at the
  baseline cap session never started a window: `limactl copy` had dropped the
  scripts' execute bit (`/tmp/session.sh: Permission denied`). The driver now
  sets it.

## Commands

All scripts are in [`tools/`](tools). Each session ran as root in the VM on a
fresh boot. The driver did `limactl stop rb-bench-1030; limactl start
rb-bench-1030`, copied `session.sh` and `run.sh` to `/tmp` (and `chmod 0755`),
and ran `sudo sh /tmp/run.sh KIND VARIANT`.

```sh
# The VM.
limactl create --name rb-bench-1030 --cpus 4 --memory 8 --disk 60 --vm-type vz \
    --mount-none --tty=false template:ubuntu-24.04
# Inside: build-essential clang llvm libbpf-dev pkg-config runc linux-tools-$(uname -r)
# python3 busybox-static git, rustup with 1.98.0, and a 16 GiB swap file, on for the
# builds only (it isn't in fstab, so the measurement boots had none).
# After the discarded run:
systemctl disable --now apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service \
    fwupd-refresh.timer motd-news.timer update-notifier-download.timer man-db.timer
systemctl mask apt-daily.service apt-daily-upgrade.service
snap refresh --hold

# Sources: git archive of f68755d9 and 259b6061, copied in and unpacked.
# Builds (tools/build.sh SRC SHA NAME [prof]; binaries land in ~/bin-NAME).
build.sh ~/src-base f68755d9 base
build.sh ~/src-cur 259b6061 cur
build.sh ~/src-cur 259b6061 curprof prof

# The pinned image's BusyBox (f1947045...), for the release script and --compare.
python3 -I tools/fetch_busybox.py /tmp/busybox-image
install -m 0755 /tmp/busybox-image /opt/bench/busybox-image

# Sessions, in the order run (tools/run.sh).
run.sh caps base      # probe, then five rounds of host and raw at cap 27
run.sh caps cur
run.sh rounds base    # the unchanged job_bench_rounds.sh --rounds 5
run.sh prof curprof   # perf -g host and raw windows, sched_switch, probe
run.sh compare cur    # five rounds of host and shared, each --compare'd with 0.2.0's

# Post-processing (tools/post.sh in the VM; tools/analyse.py, probe.py and loadcheck.py anywhere).
post.sh
analyse.py caps-cur > caps-cur/summary.txt
probe.py caps-cur/c27-probe-host.probe.jsonl > caps-cur/c27-probe-host.timeline.txt
loadcheck.py host-busy.log caps-cur/progress.log
```

The current build's release-script session (`run.sh rounds cur`) and the first
cap pair ran before the discarded run. The repeated sessions ran after it, with
the timers disabled.

### Tool changes

The profile's tools ran against current code without fixes. These copies differ
from the profile's:

- `build.sh` takes the source directory, the commit and an output name, and
  builds one set (release or profiling) per call. The profile's built both
  from one fixed source.
- `run.sh` names this VM's paths and this record's sessions (`caps`, `rounds`,
  `prof`, `compare`). The `rounds` session points the release script at the
  pinned image's BusyBox. `compare` copies the 0.2.0 reports to `/tmp/v020` and
  installs that BusyBox at `/opt/bench/busybox`. I put the Ubuntu copy back by
  hand afterwards.
- `session.sh` gains a `compare:C` step: five rounds of host and
  shared-container windows, each with `--compare` against the same round's
  0.2.0 report. It also passes `--exec` only to the host and raw scenarios, as
  the release script does. Every other step is unchanged.
- `post.sh` reads this session's directory and window names, renders the SVGs
  and the 0.1 s timeline, and takes the categoriser's task rate from the
  window's CPU accounting.
- `categorise.py` also counts `oci::job_process`, which #1357 split out of
  `generate_job_oci_spec`, as the OCI process cost.
- `fetch_busybox.py` is new. It extracts `/bin/busybox` from the pinned
  image's arm64 layer on `public.ecr.aws` and prints its SHA-256.
- `analyse.py`, `probe.py`, `loadcheck.py` and `flame.py` are unchanged.

## Files

| Path | What |
|---|---|
| `caps-base/`, `caps-cur/` | The cap-27 sessions on 6.8.0-147: `relish bench --report` JSONs and output, CPU snapshots, `vmstat`, mid-window `batch-status`, `summary.txt` (per-window CPU accounting), the probe timeline, `progress.log`, `binaries.txt`, `bun.log` |
| `rounds-base/`, `rounds-cur/` | The release script's 20 reports and outputs per build, `progress.log`, `binaries.txt`, `summary.txt` |
| `compare-vs-0.2.0/` | Five host and five shared windows of the current build, each with its `--compare` result against 0.2.0 in the `.out` file |
| `profile/` | The profiling session: `summary.txt`, the `perf` top-frame lists, the category tables, the 0.1 s timeline, the probe timeline |
| `flamegraphs/` | Four SVGs, on-CPU and off-CPU |
| `kernel-101/` | The first cap-27 pair, on 6.8.0-101 |
| `discarded/rounds-base-unattended-upgrades/` | The discarded release-script run |
| `host-busy.log` | The Mac every 10 s: load, compiler processes, the VM's CPU and everyone else's |
| `tools/` | Every script as run |

The raw `perf.data` files (180 to 250 MB each), the `perf script` output, the
folded stacks and the probes' raw `batch-status` polls stayed in the VM.

# Where the time goes in v0.2.8 (2026-11-12)

The release profile of v0.2.8, measured beside v0.2.7 in the same VM, in alternating rounds. It's evidence, not a gate: every rule below reports what it would flag, and each flagged line needs a sentence saying why (maintainer decision, 26 October 2026).

## Answer

**0.2.8 runs as fast as 0.2.7, and its idle cluster is a little quieter.** On the same VM, in alternating rounds, host jobs ran at exactly the same rate (15,500 a second in all six windows, ratio 1.000) and shared containers 1.3% faster (11,483.3 a second against 11,333.3, interval 0.997–1.043), while the raw-process calibration moved 5% with an interval of 0.878–1.102, the host's own noise. Start lag is unchanged (p50 1 ms, p99 4 ms in both scenarios), and no window failed a task. Under host jobs the time goes where it went in 0.2.7: `perf` finds the VM 62% busy, Bun's task-array leader tick and node sync take 10.3% of the VM and the async runtime 1.4%, and the helpers and BusyBox tasks take nearly all the rest (the helper's syscalls 10.5%, exit and teardown 8.3%, BusyBox itself 7.7%, page faults 7.6%, `execve` 7.1%, process creation 3.5%); no category moved more than 1 point. Idle, Bun used 1.50% of a core against 1.60%, switched context 287 times a second against 332, and held 8.4 MiB more (168.5 MiB against 160.1 MiB). The rules flag six lines and none is a regression: all six come from the idle cluster's profile, where Bun had only 105 and 115 samples. Two are the agent loop and Grill trading the samples of one change: 0.2.8 moved the log forwarder's capture reads from the agent loop's task onto the blocking pool ([#1858](https://github.com/reliaburger/reliaburger/pull/1858)). Two are Mayo's `sysinfo` walk of `/proc`, which runs on rayon's threads with no frame the category map names. The API server and async-runtime moves are five and four samples, inside the noise.

Release log: 0.2.8 runs host jobs at the same rate as 0.2.7 and shared containers 1.3% faster on the same VM, with the same start lag and an idle Bun at 1.5% of a core.

The host was quiet: the 1-minute load had a median of 1.98 (0.73 to 4.84), and no compiler ran in any of 374 samples. Creating the VM took 13 minutes of the run's 62.

## Compared with v0.2.7

Both versions ran in the same VM, in alternating rounds. Ratios are v0.2.8 over v0.2.7, with a 95% bootstrap interval over the rounds. The "vs raw" columns divide each scenario by the same version's raw-process rate, so a slower rig doesn't read as a slower release.

| Scenario | v0.2.7, median (range) | v0.2.8, median (range) | Ratio (interval) | Steady, before / after | vs raw, before / after | Verdict |
|---|---:|---:|---:|---:|---:|---|
| Raw VM processes (calibration) | 37,362.6 (34,012.2-37,616.9)/s | 35,482.1 (33,025.2-37,471.1)/s | 0.950 (0.878-1.102) | - | 1.000 / 1.000 | no clear change |
| Host jobs, default cap | 15,500.0 (15,500.0-15,500.0)/s | 15,500.0 (15,500.0-15,500.0)/s | 1.000 (1.000-1.000) | 15,881 (15,881-15,881) / 15,881 (15,881-15,881) | 0.415 / 0.437 | no clear change |
| Shared containers | 11,333.3 (11,150.0-11,450.0)/s | 11,483.3 (11,416.7-11,633.3)/s | 1.013 (0.997-1.043) | 11,911 (11,762-12,010) / 12,084 (12,010-12,233) | 0.303 / 0.324 | no clear change |

Idle cluster, no work (absolute numbers):

| Measure | v0.2.7 | v0.2.8 |
|---|---:|---:|
| Bun's CPU, share of one core | 1.60 (1.53-1.65)% | 1.50 (1.42-1.50)% |
| Bun's context switches a second | 332 (328-335) | 287 (283-288) |
| Bun's RSS, KiB | 163,912 (163,512-174,028) | 172,508 (170,796-178,164) |

Category shifts, host jobs, share of the VM (`perfcpu-host`):

| Category | v0.2.7 | v0.2.8 | Points |
|---|---:|---:|---:|
| idle (no runnable work) | 36.7% | 37.7% | +1.0 |
| Bun: task ledger (group commit, fsync) | 0.1% | 0.1% | +0.0 |
| Bun: task-array leader tick and node sync | 10.2% | 10.3% | +0.1 |
| Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs) | 0.0% | 0.0% | +0.0 |
| Bun: Raft core and log | 0.0% | 0.0% | +0.0 |
| Bun: agent loop and supervisor | 0.0% | 0.0% | -0.0 |
| Bun: API server | 0.0% | 0.0% | +0.0 |
| Bun: metrics (Mayo) | 0.0% | 0.0% | -0.0 |
| Bun: containers (Grill) | 0.1% | 0.0% | -0.0 |
| Bun: other Reliaburger code | 0.0% | 0.0% | +0.0 |
| Bun: async runtime (scheduler, timers, parking) | 1.4% | 1.4% | +0.0 |
| Bun: uncategorised (extend the map) | 0.0% | 0.0% | -0.0 |
| helper/child: clone3 (process creation) | 3.5% | 3.5% | +0.0 |
| helper/child: execve | 6.9% | 7.1% | +0.2 |
| helper/child: exit and address-space teardown | 8.6% | 8.3% | -0.3 |
| helper/child: page faults | 8.1% | 7.6% | -0.5 |
| helper: wait, poll, pipes and socket syscalls | 10.3% | 10.5% | +0.2 |
| helper: user space and other kernel | 2.2% | 2.2% | +0.1 |
| busybox task: user space and other kernel | 7.9% | 7.7% | -0.2 |
| relish (raw scenario spawner and bench client) | 0.0% | 0.0% | +0.0 |
| other processes and kernel threads | 4.0% | 3.4% | -0.6 |

Category shifts, idle cluster, share of Bun (`perfcpu-idle`):

| Category | v0.2.7 | v0.2.8 | Points |
|---|---:|---:|---:|
| Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs) | 0.0% | 0.9% | +0.9 |
| Bun: Raft core and log | 2.9% | 5.2% | +2.4 |
| Bun: agent loop and supervisor | 13.3% | 2.6% | -10.7 |
| Bun: API server | 11.4% | 6.1% | -5.3 |
| Bun: metrics (Mayo) | 0.0% | 2.6% | +2.6 |
| Bun: containers (Grill) | 29.5% | 39.1% | +9.6 |
| Bun: other Reliaburger code | 10.5% | 13.0% | +2.6 |
| Bun: async runtime (scheduler, timers, parking) | 26.7% | 20.9% | -5.8 |
| Bun: uncategorised (extend the map) | 5.7% | 9.6% | +3.9 |

### Would flag

The rules don't gate a release yet (maintainer decision, 26 October 2026). Each line below needs a sentence saying why, in place of its *Explanation*.

- **extend the map**, uncategorised: previous, idle cluster, share of Bun. 5.7% of Bun's samples are uncategorised, over the 5% limit: extend BUN_RULES in scripts/release/profiling/profile_categories.py. *Explanation:* Six samples, all `/proc` walks by the `sysinfo` crate, which Mayo's collector (`src/mayo/collector.rs`) runs on rayon's threads, so the stack holds no `reliaburger::` frame for the map to match; a rule on `sysinfo::` and `rayon` frames would file them under metrics (Mayo), and it's left for the next change to the map rather than rewriting this record's generated tables.
- **extend the map**, uncategorised: candidate, idle cluster, share of Bun. 9.6% of Bun's samples are uncategorised, over the 5% limit: extend BUN_RULES in scripts/release/profiling/profile_categories.py. *Explanation:* The same Mayo `sysinfo` walk on rayon's threads, 11 samples of 115 this time (13 `refresh_procs` samples against 6 in 0.2.7, a difference of a handful of 10 ms samples in a minute); the same missing rule covers it.
- **explain**, category-move: Bun: agent loop and supervisor (idle cluster, share of Bun). 13.3% to 2.6%, -10.7 points. *Explanation:* 14 samples to 3, and 7 of 0.2.7's 14 were the log forwarder's `read_chunk` reading the web app's capture file on a tokio worker; [#1858](https://github.com/reliaburger/reliaburger/pull/1858) moved that read onto the blocking pool, where its stack runs through `grill::capture` and the blocking task's harness, so the work moved category, not away.
- **explain**, category-move: Bun: API server (idle cluster, share of Bun). 11.4% to 6.1%, -5.3 points. *Explanation:* 12 samples to 7, most of them certificate checks on API connections (`sesame::cert::verify_signature`) in both versions; five samples is inside the sampling noise of a profile with about 110 Bun samples (one standard deviation of the difference is about 4 points), and nothing in 0.2.8 changed that path.
- **explain**, category-move: Bun: containers (Grill) (idle cluster, share of Bun). 29.5% to 39.1%, +9.6 points. *Explanation:* 31 samples to 45: the capture reads that left the agent loop (`grill::capture::open_capture` and the blocking `read_chunk` tasks, 5 samples) plus `durable::read_bounded` reading the process owner's records (8 samples, where 0.2.7's profile named the same reads `process_owner::current_boot_id`, 6); Bun's total idle CPU fell from 1.60% to 1.50% of a core, so the share grew while the time didn't.
- **explain**, category-move: Bun: async runtime (scheduler, timers, parking) (idle cluster, share of Bun). 26.7% to 20.9%, -5.8 points. *Explanation:* 28 samples to 24 of a larger total, with context switches down from 332 to 287 a second; fewer worker wake-ups fits the reads that moved to the blocking pool, but four samples is within the noise, so this says no more than "no worse".

## Rates

### v0.2.7 (previous)

| Scenario | Rate, median (range) | Steady 15-58 s | VM CPU busy | Bun's CPU | Start lag p50 / p99 | Usable windows, task failures |
|---|---:|---:|---:|---:|---:|---|
| Raw VM processes (calibration) | 37,362.6 (34,012.2-37,616.9)/s | - | 91% | 0.01 cores | - | 3 of 3, 0 |
| Host jobs, default cap | 15,500.0 (15,500.0-15,500.0)/s | 15,881/s | 58% | 0.47 cores | 1 / 4 ms | 3 of 3, 0 |
| Shared containers | 11,333.3 (11,150.0-11,450.0)/s | 11,911/s | 94% | 0.50 cores | 1 / 4 ms | 3 of 3, 0 |

Idle cluster: Bun used 1.60% of one core (median of 3), 332 context switches a second, RSS 160.1 MiB.

### v0.2.8 (candidate)

| Scenario | Rate, median (range) | Steady 15-58 s | VM CPU busy | Bun's CPU | Start lag p50 / p99 | Usable windows, task failures |
|---|---:|---:|---:|---:|---:|---|
| Raw VM processes (calibration) | 35,482.1 (33,025.2-37,471.1)/s | - | 91% | 0.01 cores | - | 3 of 3, 0 |
| Host jobs, default cap | 15,500.0 (15,500.0-15,500.0)/s | 15,881/s | 59% | 0.47 cores | 1 / 4 ms | 3 of 3, 0 |
| Shared containers | 11,483.3 (11,416.7-11,633.3)/s | 12,084/s | 95% | 0.51 cores | 1 / 4 ms | 3 of 3, 0 |

Idle cluster: Bun used 1.50% of one core (median of 3), 287 context switches a second, RSS 168.5 MiB.

## Where the time goes

v0.2.7, host jobs, share of the VM (`perfcpu-host`, 39,920 samples):

| Category | Share | µs per task |
|---|---:|---:|
| idle (no runnable work) | 36.7% | 94.7 |
| Bun: task ledger (group commit, fsync) | 0.1% | 0.1 |
| Bun: task-array leader tick and node sync | 10.2% | 26.3 |
| Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs) | 0.0% | 0.0 |
| Bun: Raft core and log | 0.0% | 0.0 |
| Bun: agent loop and supervisor | 0.0% | 0.0 |
| Bun: API server | 0.0% | 0.1 |
| Bun: metrics (Mayo) | 0.0% | 0.0 |
| Bun: containers (Grill) | 0.1% | 0.2 |
| Bun: other Reliaburger code | 0.0% | 0.0 |
| Bun: async runtime (scheduler, timers, parking) | 1.4% | 3.6 |
| Bun: uncategorised (extend the map) | 0.0% | 0.1 |
| helper/child: clone3 (process creation) | 3.5% | 9.0 |
| helper/child: execve | 6.9% | 17.8 |
| helper/child: exit and address-space teardown | 8.6% | 22.2 |
| helper/child: page faults | 8.1% | 21.0 |
| helper: wait, poll, pipes and socket syscalls | 10.3% | 26.7 |
| helper: user space and other kernel | 2.2% | 5.6 |
| busybox task: user space and other kernel | 7.9% | 20.5 |
| relish (raw scenario spawner and bench client) | 0.0% | 0.1 |
| other processes and kernel threads | 4.0% | 10.3 |

v0.2.7, idle cluster, share of Bun (`perfcpu-idle`, 39,892 samples):

| Category | Share |
|---|---:|
| Bun: Raft core and log | 2.9% |
| Bun: agent loop and supervisor | 13.3% |
| Bun: API server | 11.4% |
| Bun: containers (Grill) | 29.5% |
| Bun: other Reliaburger code | 10.5% |
| Bun: async runtime (scheduler, timers, parking) | 26.7% |
| Bun: uncategorised (extend the map) | 5.7% |

**5.7% of Bun's samples are uncategorised, over the 5% limit: extend BUN_RULES in scripts/release/profiling/profile_categories.py.**

v0.2.8, host jobs, share of the VM (`perfcpu-host`, 39,920 samples):

| Category | Share | µs per task |
|---|---:|---:|
| idle (no runnable work) | 37.7% | 97.3 |
| Bun: task ledger (group commit, fsync) | 0.1% | 0.2 |
| Bun: task-array leader tick and node sync | 10.3% | 26.5 |
| Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs) | 0.0% | 0.0 |
| Bun: Raft core and log | 0.0% | 0.1 |
| Bun: agent loop and supervisor | 0.0% | 0.0 |
| Bun: API server | 0.0% | 0.1 |
| Bun: metrics (Mayo) | 0.0% | 0.0 |
| Bun: containers (Grill) | 0.0% | 0.1 |
| Bun: other Reliaburger code | 0.0% | 0.0 |
| Bun: async runtime (scheduler, timers, parking) | 1.4% | 3.6 |
| Bun: uncategorised (extend the map) | 0.0% | 0.1 |
| helper/child: clone3 (process creation) | 3.5% | 9.1 |
| helper/child: execve | 7.1% | 18.2 |
| helper/child: exit and address-space teardown | 8.3% | 21.5 |
| helper/child: page faults | 7.6% | 19.6 |
| helper: wait, poll, pipes and socket syscalls | 10.5% | 27.1 |
| helper: user space and other kernel | 2.2% | 5.8 |
| busybox task: user space and other kernel | 7.7% | 20.0 |
| relish (raw scenario spawner and bench client) | 0.0% | 0.1 |
| other processes and kernel threads | 3.4% | 8.8 |

v0.2.8, idle cluster, share of Bun (`perfcpu-idle`, 39,884 samples):

| Category | Share |
|---|---:|
| Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs) | 0.9% |
| Bun: Raft core and log | 5.2% |
| Bun: agent loop and supervisor | 2.6% |
| Bun: API server | 6.1% |
| Bun: metrics (Mayo) | 2.6% |
| Bun: containers (Grill) | 39.1% |
| Bun: other Reliaburger code | 13.0% |
| Bun: async runtime (scheduler, timers, parking) | 20.9% |
| Bun: uncategorised (extend the map) | 9.6% |

**9.6% of Bun's samples are uncategorised, over the 5% limit: extend BUN_RULES in scripts/release/profiling/profile_categories.py.**

Flamegraphs (hover a frame for its name and share):

- [`flamegraphs/candidate-host-offcpu-bun.svg`](flamegraphs/candidate-host-offcpu-bun.svg)
- [`flamegraphs/candidate-host-offcpu-helpers.svg`](flamegraphs/candidate-host-offcpu-helpers.svg)
- [`flamegraphs/candidate-host-oncpu-bun.svg`](flamegraphs/candidate-host-oncpu-bun.svg)
- [`flamegraphs/candidate-host-oncpu-vm.svg`](flamegraphs/candidate-host-oncpu-vm.svg)
- [`flamegraphs/candidate-idle-oncpu-bun.svg`](flamegraphs/candidate-idle-oncpu-bun.svg)
- [`flamegraphs/diff-host-oncpu-vm.svg`](flamegraphs/diff-host-oncpu-vm.svg)
- [`flamegraphs/diff-idle-oncpu-bun.svg`](flamegraphs/diff-idle-oncpu-bun.svg)
- [`flamegraphs/previous-host-offcpu-bun.svg`](flamegraphs/previous-host-offcpu-bun.svg)
- [`flamegraphs/previous-host-offcpu-helpers.svg`](flamegraphs/previous-host-offcpu-helpers.svg)
- [`flamegraphs/previous-host-oncpu-bun.svg`](flamegraphs/previous-host-oncpu-bun.svg)
- [`flamegraphs/previous-host-oncpu-vm.svg`](flamegraphs/previous-host-oncpu-vm.svg)
- [`flamegraphs/previous-idle-oncpu-bun.svg`](flamegraphs/previous-idle-oncpu-bun.svg)

`perf sched latency` and the probe's grant-cycle timeline:

- [`raw/previous/perf/perfsched-host.latency.txt`](raw/previous/perf/perfsched-host.latency.txt)
- [`raw/previous/perf/probe-host.timeline.txt`](raw/previous/perf/probe-host.timeline.txt)
- [`raw/candidate/perf/perfsched-host.latency.txt`](raw/candidate/perf/perfsched-host.latency.txt)
- [`raw/candidate/perf/probe-host.timeline.txt`](raw/candidate/perf/probe-host.timeline.txt)

## The rig

| Item | Value |
|---|---|
| Host | Mac17,14, Apple M5 Max, 18 cores, 128 GiB, macOS 27.2 (Darwin 27.2.0) |
| VM | `rb-profile-ffca5922`, Lima 2.2.1, VZ, 4 vCPU, 8 GiB, 60 GiB disk, template:ubuntu-24.04, no mounts, made for this run and deleted after |
| Guest | Ubuntu 24.04.5 LTS aarch64, Linux 6.8.0-142-generic, runc version 1.5.2, perf version 6.8.12 |
| BusyBox | Ubuntu's `busybox-static`, SHA-256 `52151e7f322f926b64049cdaa1410dc3ea6485525e0624b05813791c219ae933` |
| Previous | v0.2.7, commit `ace072c73c2d5a5d7124f3a4f879c88bf9c33839`, from `https://github.com/reliaburger/reliaburger/releases/download/v0.2.7`; `bun` `bb17a820861d084be4f6d162a57f9eca29b3eac8de68fdc2bc0f0843623e9155`, `relish` `8c663056cfce5f55c6b2741498b570dcc31822a8d35474651a1d67679109a8df` |
| Previous, profiling build | artefact profiling-build-ace072c73c2d5a5d7124f3a4f879c88bf9c33839-1 of build run 42320339626 |
| Candidate | v0.2.8, commit `e423d469a93b04d7bdc64582be1922ac5f453fd1`, from `https://github.com/reliaburger/reliaburger/releases/download/v0.2.8`; `bun` `8a212344a4ccfc8fdefeff608ec3e74ce4f6bcbbe51bccda0da4a99a51c5568b`, `relish` `546d297331cc895e0d11360cb001c8a8ae3b529e6dfed854767792b2828923ae` |
| Candidate, profiling build | artefact profiling-build-e423d469a93b04d7bdc64582be1922ac5f453fd1-1 of build run 43017213259 |
| Qualified digest | `d9d04a7708e06fb444e8ca880a73321539e2300f29bb141ba80e7781a4bbe9f8` (candidate.json) |
| Scenarios | raw,host,shared,idle; 60 s windows, the `relish bench` defaults otherwise, beside a 500m BusyBox web app on a fresh single-node cluster |

### Host load

The 1-minute load and the count of `rustc` and `cargo` processes on the host, at the start and end of every session. No round is thrown away for load: on a busy Mac every round would be. The alternating rounds take the noise instead.

| Session | Load before | Load after | Compilers seen |
|---|---:|---:|---:|
| previous r1 | 2.02 | 1.89 | 0 |
| candidate r1 | 1.75 | 1.78 | 0 |
| candidate r2 | 2.62 | 1.35 | 0 |
| previous r2 | 1.45 | 1.53 | 0 |
| previous r3 | 1.45 | 2.02 | 0 |
| candidate r3 | 1.79 | 1.87 | 0 |
| previous perf | 1.74 | 1.18 | 0 |
| candidate perf | 1.23 | 1.06 | 0 |

Sampled every 10 s: load 0.73 to 4.84 (median 1.98), compilers running in 0 of 374 samples.

### Wall time

Estimated 69 min before the run; it took 62 min. 3 rounds per version.

## Reproduce

```sh
scripts/release/qualify-profile.sh --candidate https://github.com/reliaburger/reliaburger/releases/download/v0.2.8 --qualified-digest d9d04a7708e06fb444e8ca880a73321539e2300f29bb141ba80e7781a4bbe9f8
```

## Files

| Path | What |
|---|---|
| `metadata.tsv`, `host-load.tsv` | What the script knew about the run, and the host's load |
| `summary.json`, `comparison.json` | Every window's numbers, and the comparison with its findings |
| `raw/LABEL/rN/` | One rate session: bench reports and progress, CPU snapshots, `vmstat`, `batch-status`, `binaries.txt` |
| `raw/LABEL/perf/` | The profiling session: folded stacks, top frames, category tables, `perf sched latency`, the probe timeline |
| `flamegraphs/` | On-CPU and off-CPU flamegraphs, and the differential ones |

# Where the time goes in v0.2.4 (2026-10-30)

The release profile of v0.2.4, measured beside v0.2.3 in the same VM, in alternating rounds. It's evidence, not a gate: every rule below reports what it would flag, and each flagged line needs a sentence saying why (maintainer decision, 26 October 2026).

## Answer

**0.2.4 is no slower than 0.2.3, and slightly faster.** On the same VM, in alternating rounds, host jobs ran 4.5% faster (15,466.7 a second against 14,800.0, interval 1.037–1.058) and shared containers 5.7% faster (8,366.7 against 7,916.7, interval 1.053–1.075), while the raw-process calibration moved 0.7%. Start lag is unchanged (p50 1 ms, p99 4 ms on host jobs; 1 and 8 ms on shared containers), and no window failed a task. Idle, Bun used 3.75% of a core against 3.81% and held 15 MiB less (158.5 MiB against 173.2 MiB). Neither speed-up crosses the rules' threshold, so the comparison says "no clear change". The rules flag 19 lines, and none of them is a regression: 17 are because 0.2.3 has no profiling build, so `perf` named none of its Bun frames, and every category "moves" from nothing; the other two flag Bun's idle CPU, which is the same in both releases.

Release log: 0.2.4 runs host jobs 4.5% and shared containers 5.7% faster than 0.2.3 on the same VM, with the same start lag and 15 MiB less idle memory.

The host wasn't silent. The 1-minute load had a median of 3.98, and 7.75 at the start of the first round, while Spotlight indexed a new worktree; no compiler ran at any sample. The alternating rounds share that noise between the versions. The first two attempts at this run exited before they started, on a preflight bug in the script ([#1488](https://github.com/reliaburger/reliaburger/issues/1488)): this run used the one-line fix.

## Compared with v0.2.3

Both versions ran in the same VM, in alternating rounds. Ratios are v0.2.4 over v0.2.3, with a 95% bootstrap interval over the rounds. The "vs raw" columns divide each scenario by the same version's raw-process rate, so a slower rig doesn't read as a slower release.

| Scenario | v0.2.3, median (range) | v0.2.4, median (range) | Ratio (interval) | Steady, before / after | vs raw, before / after | Verdict |
|---|---:|---:|---:|---:|---:|---|
| Raw VM processes (calibration) | 29,177.8 (28,292.9-29,792.4)/s | 29,393.1 (29,239.8-29,418.5)/s | 1.007 (0.981-1.040) | - | 1.000 / 1.000 | no clear change |
| Host jobs, default cap | 14,800.0 (14,616.7-14,916.7)/s | 15,466.7 (15,466.7-15,466.7)/s | 1.045 (1.037-1.058) | 15,658 (15,385-15,831) / 15,831 (15,831-15,831) | 0.507 / 0.526 | no clear change |
| Shared containers | 7,916.7 (7,783.3-7,916.7)/s | 8,366.7 (8,333.3-8,416.7)/s | 1.057 (1.053-1.075) | 8,908 (8,706-8,933) / 8,983 (8,958-9,082) | 0.271 / 0.285 | no clear change |

Idle cluster, no work (absolute numbers):

| Measure | v0.2.3 | v0.2.4 |
|---|---:|---:|
| Bun's CPU, share of one core | 3.81 (3.71-3.90)% | 3.75 (3.75-3.78)% |
| Bun's context switches a second | 324 (322-331) | 318 (317-321) |
| Bun's RSS, KiB | 177,380 (167,816-184,532) | 162,284 (161,952-172,860) |

Category shifts, host jobs, share of the VM (`perfcpu-host`):

| Category | v0.2.3 | v0.2.4 | Points |
|---|---:|---:|---:|
| idle (no runnable work) | 25.3% | 22.4% | -2.9 |
| Bun: task ledger (group commit, fsync) | 0.0% | 0.1% | +0.1 |
| Bun: task-array leader tick and node sync | 0.0% | 13.1% | +13.1 |
| Bun: Raft core and log | 0.0% | 0.0% | +0.0 |
| Bun: agent loop and supervisor | 0.0% | 0.0% | +0.0 |
| Bun: API server | 0.0% | 0.1% | +0.1 |
| Bun: metrics (Mayo) | 0.0% | 0.0% | +0.0 |
| Bun: containers (Grill) | 0.0% | 0.1% | +0.1 |
| Bun: other Reliaburger code | 0.0% | 0.0% | +0.0 |
| Bun: async runtime (scheduler, timers, parking) | 0.0% | 1.8% | +1.8 |
| Bun: uncategorised (extend the map) | 14.1% | 0.0% | -14.0 |
| helper/child: clone3 (process creation) | 4.2% | 4.6% | +0.4 |
| helper/child: execve | 9.3% | 9.7% | +0.3 |
| helper/child: exit and address-space teardown | 8.8% | 8.9% | +0.0 |
| helper/child: page faults | 8.8% | 8.4% | -0.4 |
| helper: wait, poll, pipes and socket syscalls | 12.9% | 13.4% | +0.6 |
| helper: user space and other kernel | 2.2% | 2.4% | +0.2 |
| busybox task: user space and other kernel | 11.4% | 11.3% | -0.2 |
| relish (raw scenario spawner and bench client) | 0.0% | 0.0% | -0.0 |
| other processes and kernel threads | 3.0% | 3.7% | +0.7 |

Category shifts, idle cluster, share of Bun (`perfcpu-idle`):

| Category | v0.2.3 | v0.2.4 | Points |
|---|---:|---:|---:|
| Bun: task-array leader tick and node sync | 0.0% | 2.0% | +2.0 |
| Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs) | 0.0% | 0.4% | +0.4 |
| Bun: Raft core and log | 0.0% | 5.1% | +5.1 |
| Bun: agent loop and supervisor | 0.0% | 11.4% | +11.4 |
| Bun: API server | 0.0% | 11.4% | +11.4 |
| Bun: gossip (Mustard) | 0.0% | 0.4% | +0.4 |
| Bun: containers (Grill) | 0.0% | 31.9% | +31.9 |
| Bun: other Reliaburger code | 0.0% | 4.7% | +4.7 |
| Bun: async runtime (scheduler, timers, parking) | 0.0% | 24.4% | +24.4 |
| Bun: uncategorised (extend the map) | 100.0% | 8.3% | -91.7 |

### Would flag

The rules don't gate a release yet (maintainer decision, 26 October 2026). Each line below needs a sentence saying why, in place of its *Explanation*.

- **would fail**, idle-cpu: idle cluster. Bun used 3.7% of one core with no work, over the 2% limit. *Explanation:* The same in 0.2.3 (3.81% of a core): the idle cluster isn't idle, since it keeps a 500m BusyBox web app running under supervision, with its health checks, metrics collection and Raft heartbeats. It isn't a regression; whether the 2% limit fits a cluster with an app on it is for the rules' first review.
- **extend the map**, uncategorised: previous, host jobs, share of the VM. 100.0% of Bun's samples are uncategorised, over the 5% limit: extend BUN_RULES in scripts/release/profiling/profile_categories.py. *Explanation:* 0.2.3 has no profiling build, so all of its Bun samples are unnamed (see the first move below). Nothing to map.
- **explain**, category-move: Bun: task-array leader tick and node sync (host jobs, share of the VM). 0.0% to 13.1%, +13.1 points. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **explain**, category-move: Bun: uncategorised (extend the map) (host jobs, share of the VM). 14.1% to 0.0%, -14.0 points. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **would fail**, new-category: Bun: async runtime (scheduler, timers, parking) (host jobs, share of the VM). 12.0% of Bun's CPU, from 0.0% in the previous release. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **would fail**, new-category: Bun: task-array leader tick and node sync (host jobs, share of the VM). 86.1% of Bun's CPU, from 0.0% in the previous release. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **extend the map**, uncategorised: previous, idle cluster, share of Bun. 100.0% of Bun's samples are uncategorised, over the 5% limit: extend BUN_RULES in scripts/release/profiling/profile_categories.py. *Explanation:* 0.2.3 has no profiling build, so all of its Bun samples are unnamed (see the first move below). Nothing to map.
- **extend the map**, uncategorised: candidate, idle cluster, share of Bun. 8.3% of Bun's samples are uncategorised, over the 5% limit: extend BUN_RULES in scripts/release/profiling/profile_categories.py. *Explanation:* 8.3% of 0.2.4's idle Bun samples land in no category: mostly `libc` and tokio blocking-pool threads with no Reliaburger frame above them. Mapping them is a follow-up for `profile_categories.py`; it doesn't change any number above.
- **explain**, category-move: Bun: Raft core and log (idle cluster, share of Bun). 0.0% to 5.1%, +5.1 points. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **explain**, category-move: Bun: agent loop and supervisor (idle cluster, share of Bun). 0.0% to 11.4%, +11.4 points. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **explain**, category-move: Bun: API server (idle cluster, share of Bun). 0.0% to 11.4%, +11.4 points. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **explain**, category-move: Bun: containers (Grill) (idle cluster, share of Bun). 0.0% to 31.9%, +31.9 points. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **explain**, category-move: Bun: async runtime (scheduler, timers, parking) (idle cluster, share of Bun). 0.0% to 24.4%, +24.4 points. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **explain**, category-move: Bun: uncategorised (extend the map) (idle cluster, share of Bun). 100.0% to 8.3%, -91.7 points. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **would fail**, new-category: Bun: API server (idle cluster, share of Bun). 11.4% of Bun's CPU, from 0.0% in the previous release. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **would fail**, new-category: Bun: Raft core and log (idle cluster, share of Bun). 5.1% of Bun's CPU, from 0.0% in the previous release. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **would fail**, new-category: Bun: agent loop and supervisor (idle cluster, share of Bun). 11.4% of Bun's CPU, from 0.0% in the previous release. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **would fail**, new-category: Bun: async runtime (scheduler, timers, parking) (idle cluster, share of Bun). 24.4% of Bun's CPU, from 0.0% in the previous release. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.
- **would fail**, new-category: Bun: containers (Grill) (idle cluster, share of Bun). 31.9% of Bun's CPU, from 0.0% in the previous release. *Explanation:* 0.2.3 has no profiling build (build.yml first made one for 0.2.4, with this script, #1358), so `perf` ran on its stripped binary and named none of Bun's frames. Every Bun category in 0.2.3 reads 0%, and this is 0.2.4 getting names, not a change in where the time goes.

## Rates

### v0.2.3 (previous)

| Scenario | Rate, median (range) | Steady 15-58 s | VM CPU busy | Bun's CPU | Start lag p50 / p99 | Usable windows, task failures |
|---|---:|---:|---:|---:|---:|---|
| Raw VM processes (calibration) | 29,177.8 (28,292.9-29,792.4)/s | - | 89% | 0.01 cores | - | 3 of 3, 0 |
| Host jobs, default cap | 14,800.0 (14,616.7-14,916.7)/s | 15,658/s | 70% | 0.57 cores | 1 / 4 ms | 3 of 3, 0 |
| Shared containers | 7,916.7 (7,783.3-7,916.7)/s | 8,908/s | 90% | 0.49 cores | 1 / 8 ms | 3 of 3, 0 |

Idle cluster: Bun used 3.81% of one core (median of 3), 324 context switches a second, RSS 173.2 MiB.

### v0.2.4 (candidate)

| Scenario | Rate, median (range) | Steady 15-58 s | VM CPU busy | Bun's CPU | Start lag p50 / p99 | Usable windows, task failures |
|---|---:|---:|---:|---:|---:|---|
| Raw VM processes (calibration) | 29,393.1 (29,239.8-29,418.5)/s | - | 89% | 0.01 cores | - | 3 of 3, 0 |
| Host jobs, default cap | 15,466.7 (15,466.7-15,466.7)/s | 15,831/s | 74% | 0.60 cores | 1 / 4 ms | 3 of 3, 0 |
| Shared containers | 8,366.7 (8,333.3-8,416.7)/s | 8,983/s | 93% | 0.51 cores | 1 / 8 ms | 3 of 3, 0 |

Idle cluster: Bun used 3.75% of one core (median of 3), 318 context switches a second, RSS 158.5 MiB.

## Where the time goes

v0.2.3, host jobs, share of the VM (`perfcpu-host`, 39,916 samples):

| Category | Share | µs per task |
|---|---:|---:|
| idle (no runnable work) | 25.3% | 71.9 |
| Bun: uncategorised (extend the map) | 14.1% | 39.9 |
| helper/child: clone3 (process creation) | 4.2% | 11.9 |
| helper/child: execve | 9.3% | 26.6 |
| helper/child: exit and address-space teardown | 8.8% | 25.1 |
| helper/child: page faults | 8.8% | 25.0 |
| helper: wait, poll, pipes and socket syscalls | 12.9% | 36.5 |
| helper: user space and other kernel | 2.2% | 6.3 |
| busybox task: user space and other kernel | 11.4% | 32.4 |
| relish (raw scenario spawner and bench client) | 0.0% | 0.0 |
| other processes and kernel threads | 3.0% | 8.5 |

**100.0% of Bun's samples are uncategorised, over the 5% limit: extend BUN_RULES in scripts/release/profiling/profile_categories.py.**

v0.2.3, idle cluster, share of Bun (`perfcpu-idle`, 38,751 samples):

| Category | Share |
|---|---:|
| Bun: uncategorised (extend the map) | 100.0% |

**100.0% of Bun's samples are uncategorised, over the 5% limit: extend BUN_RULES in scripts/release/profiling/profile_categories.py.**

v0.2.4, host jobs, share of the VM (`perfcpu-host`, 39,920 samples):

| Category | Share | µs per task |
|---|---:|---:|
| idle (no runnable work) | 22.4% | 58.4 |
| Bun: task ledger (group commit, fsync) | 0.1% | 0.2 |
| Bun: task-array leader tick and node sync | 13.1% | 34.2 |
| Bun: Raft core and log | 0.0% | 0.1 |
| Bun: agent loop and supervisor | 0.0% | 0.0 |
| Bun: API server | 0.1% | 0.1 |
| Bun: metrics (Mayo) | 0.0% | 0.0 |
| Bun: containers (Grill) | 0.1% | 0.1 |
| Bun: other Reliaburger code | 0.0% | 0.0 |
| Bun: async runtime (scheduler, timers, parking) | 1.8% | 4.8 |
| Bun: uncategorised (extend the map) | 0.0% | 0.1 |
| helper/child: clone3 (process creation) | 4.6% | 11.9 |
| helper/child: execve | 9.7% | 25.2 |
| helper/child: exit and address-space teardown | 8.9% | 23.1 |
| helper/child: page faults | 8.4% | 21.9 |
| helper: wait, poll, pipes and socket syscalls | 13.4% | 35.0 |
| helper: user space and other kernel | 2.4% | 6.3 |
| busybox task: user space and other kernel | 11.3% | 29.3 |
| relish (raw scenario spawner and bench client) | 0.0% | 0.0 |
| other processes and kernel threads | 3.7% | 9.6 |

v0.2.4, idle cluster, share of Bun (`perfcpu-idle`, 37,804 samples):

| Category | Share |
|---|---:|
| Bun: task-array leader tick and node sync | 2.0% |
| Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs) | 0.4% |
| Bun: Raft core and log | 5.1% |
| Bun: agent loop and supervisor | 11.4% |
| Bun: API server | 11.4% |
| Bun: gossip (Mustard) | 0.4% |
| Bun: containers (Grill) | 31.9% |
| Bun: other Reliaburger code | 4.7% |
| Bun: async runtime (scheduler, timers, parking) | 24.4% |
| Bun: uncategorised (extend the map) | 8.3% |

**8.3% of Bun's samples are uncategorised, over the 5% limit: extend BUN_RULES in scripts/release/profiling/profile_categories.py.**

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
| Host | Mac14,5, Apple M2 Max, 12 cores, 32 GiB, macOS 26.3.1 (Darwin 25.3.0) |
| VM | `rb-profile-63f679c4`, Lima 2.1.0, VZ, 4 vCPU, 8 GiB, 60 GiB disk, template:ubuntu-24.04, no mounts, made for this run and deleted after |
| Guest | Ubuntu 24.04.4 LTS aarch64, Linux 6.8.0-101-generic, runc version 1.4.0, perf version 6.8.12 |
| BusyBox | Ubuntu's `busybox-static`, SHA-256 `52151e7f322f926b64049cdaa1410dc3ea6485525e0624b05813791c219ae933` |
| Previous | v0.2.3, commit `d387d569bb1f18264382ffcffebe3bbf45fb7ed3`, from `https://github.com/reliaburger/reliaburger/releases/download/v0.2.3`; `bun` `3d15e567c682b0aaaa662d03e9656bb4105e651617cf38c269054b9ac0dff4ba`, `relish` `71d2e3cc0c9cd0d1b5adbe1aa121d33905d81bbc502d7164e6ef96719060a582` |
| Previous, profiling build | none: perf ran on the stripped release build, so it names no Bun frame |
| Candidate | v0.2.4, commit `964b56a2c41e2b43fb48aadef7d01839155c0dca`, from `https://github.com/reliaburger/reliaburger/releases/download/v0.2.4`; `bun` `bbb0679db09ac43e4d49bf98b9c32a35dbd634fa71db0cae00c6eae45b25e1eb`, `relish` `5a60ddd20d3fd8120341ac13d1493370fedeadf6d7c92b7bde2d58bb2bc97fbd` |
| Candidate, profiling build | artefact profiling-build-964b56a2c41e2b43fb48aadef7d01839155c0dca-1 of build run 40870779000 |
| Qualified digest | `340eaeba513c9740f058e51684713654f3df7c392ea8f19993ae1b5d12727d09` (candidate.json) |
| Scenarios | raw,host,shared,idle; 60 s windows, the `relish bench` defaults otherwise, beside a 500m BusyBox web app on a fresh single-node cluster |

### Host load

The 1-minute load and the count of `rustc` and `cargo` processes on the host, at the start and end of every session. No round is thrown away for load: on a busy Mac every round would be. The alternating rounds take the noise instead.

| Session | Load before | Load after | Compilers seen |
|---|---:|---:|---:|
| previous r1 | 7.75 | 2.46 | 0 |
| candidate r1 | 2.31 | 2.42 | 0 |
| candidate r2 | 2.33 | 2.00 | 0 |
| previous r2 | 2.00 | 3.38 | 0 |
| previous r3 | 3.02 | 2.54 | 0 |
| candidate r3 | 2.43 | 2.90 | 0 |
| previous perf | 2.94 | 3.10 | 0 |
| candidate perf | 3.47 | 2.92 | 0 |

Sampled every 10 s: load 1.85 to 10.29 (median 3.98), compilers running in 0 of 301 samples.

### Wall time

Estimated 69 min before the run; it took 50 min. 3 rounds per version.

## Reproduce

```sh
scripts/release/qualify-profile.sh --candidate https://github.com/reliaburger/reliaburger/releases/download/v0.2.4 --qualified-digest 340eaeba513c9740f058e51684713654f3df7c392ea8f19993ae1b5d12727d09 --previous v0.2.3
```

## Files

| Path | What |
|---|---|
| `metadata.tsv`, `host-load.tsv` | What the script knew about the run, and the host's load |
| `summary.json`, `comparison.json` | Every window's numbers, and the comparison with its findings |
| `raw/LABEL/rN/` | One rate session: bench reports and progress, CPU snapshots, `vmstat`, `batch-status`, `binaries.txt` |
| `raw/LABEL/perf/` | The profiling session: folded stacks, top frames, category tables, `perf sched latency`, the probe timeline |
| `flamegraphs/` | On-CPU and off-CPU flamegraphs, and the differential ones |

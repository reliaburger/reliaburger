# Where the time goes in v0.3.3 (2026-11-28)

The release profile of v0.3.3, measured beside v0.2.8 in the same VM, in alternating rounds. It's evidence, not a gate: every rule below reports what it would flag, and each flagged line needs a sentence saying why (maintainer decision, 26 October 2026).

## Answer

**0.3.3 runs jobs as fast as 0.2.8 on the same guest, so the slowdown in the homepage recordings since 0.3.1 isn't Reliaburger's.** This is the first profile of the 0.3 series, so it compares 0.3.3 with 0.2.8, the last profiled release, skipping 0.2.8 to 0.3.2 in one step. In the same VM, in alternating rounds, host jobs ran at exactly the same rate (15,500 a second in all six windows, ratio 1.000) and shared containers 0.7% faster (11,416.7 a second against 11,333.3, interval 1.003–1.010), while the raw-process calibration moved 2.7% with an interval of 0.871–1.092, the host's own noise. Start lag is unchanged (p50 1 ms, p99 4 ms in both scenarios), and no window failed a task. Under host jobs the time goes where it went in 0.2.8: `perf` finds the VM 63% busy, Bun's task-array leader tick and node sync take 10.6% of the VM and the async runtime 1.4%, and the helpers and BusyBox tasks take nearly all the rest; no category moved more than 1.3 points. Idle, Bun used 1.76% of a core against 1.85%, switched context 290 times a second against 291, and held 4.7 MiB more (175.1 MiB against 170.4 MiB). The rules flag three lines and none is a regression: all three come from the idle cluster's profile, where Bun had only 116 and 130 samples, and they move samples between categories while Bun's idle CPU went down.

[#2209](https://github.com/reliaburger/reliaburger/issues/2209) asked why 0.3.1's homepage recording ran 22 to 28% below 0.2.8's, VM baseline included. This rig answers the Reliaburger half: on one guest, Ubuntu 24.04 with Linux 6.8 (the profile's fixed template, which is 0.2.8's quickstart guest), 0.3.3 and 0.2.8 run host jobs and shared containers at the same rate. The homepage recordings run on the quickstart guest, which 0.3.0 moved to Ubuntu 26.04 and Linux 7.0, and their raw VM baseline, which runs no Reliaburger code, fell as much as the rest. This run didn't measure 26.04 against 24.04, so the guest is the likely cause, not a proven one.

Release log: on the same VM, 0.3.3 runs host jobs at 0.2.8's rate and shared containers 0.7% faster, with the same start lag and an idle Bun at 1.8% of a core.

## Compared with v0.2.8

Both versions ran in the same VM, in alternating rounds. Ratios are v0.3.3 over v0.2.8, with a 95% bootstrap interval over the rounds. The "vs raw" columns divide each scenario by the same version's raw-process rate, so a slower rig doesn't read as a slower release.

| Scenario | v0.2.8, median (range) | v0.3.3, median (range) | Ratio (interval) | Steady, before / after | vs raw, before / after | Verdict |
|---|---:|---:|---:|---:|---:|---|
| Raw VM processes (calibration) | 33,864.3 (33,294.8-37,601.2)/s | 32,961.3 (32,738.9-36,969.8)/s | 0.973 (0.871-1.092) | - | 1.000 / 1.000 | no clear change |
| Host jobs, default cap | 15,500.0 (15,500.0-15,500.0)/s | 15,500.0 (15,500.0-15,500.0)/s | 1.000 (1.000-1.000) | 15,881 (15,881-15,881) / 15,881 (15,881-15,881) | 0.458 / 0.470 | no clear change |
| Shared containers | 11,333.3 (11,300.0-11,500.0)/s | 11,416.7 (11,383.3-11,533.3)/s | 1.007 (1.003-1.010) | 11,886 (11,861-12,084) / 12,134 (12,109-12,184) | 0.335 / 0.346 | no clear change |

Idle cluster, no work (absolute numbers):

| Measure | v0.2.8 | v0.3.3 |
|---|---:|---:|
| Bun's CPU, share of one core | 1.85 (1.73-1.85)% | 1.76 (1.72-2.03)% |
| Bun's context switches a second | 291 (288-293) | 290 (290-299) |
| Bun's RSS, KiB | 174,484 (171,148-176,684) | 179,348 (177,696-200,988) |

Category shifts, host jobs, share of the VM (`perfcpu-host`):

| Category | v0.2.8 | v0.3.3 | Points |
|---|---:|---:|---:|
| idle (no runnable work) | 35.8% | 37.1% | +1.3 |
| Bun: GitOps loop | 0.0% | 0.0% | +0.0 |
| Bun: task ledger (group commit, fsync) | 0.1% | 0.1% | +0.0 |
| Bun: task-array leader tick and node sync | 10.4% | 10.6% | +0.1 |
| Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs) | 0.0% | 0.0% | +0.0 |
| Bun: Raft core and log | 0.0% | 0.0% | +0.0 |
| Bun: agent loop and supervisor | 0.0% | 0.0% | +0.0 |
| Bun: API server | 0.0% | 0.0% | -0.0 |
| Bun: metrics (Mayo) | 0.0% | 0.0% | +0.0 |
| Bun: gossip (Mustard) | 0.0% | 0.0% | +0.0 |
| Bun: service discovery (Onion) | 0.0% | 0.0% | +0.0 |
| Bun: containers (Grill) | 0.1% | 0.1% | +0.0 |
| Bun: other Reliaburger code | 0.0% | 0.0% | -0.0 |
| Bun: async runtime (scheduler, timers, parking) | 1.3% | 1.4% | +0.2 |
| helper/child: clone3 (process creation) | 3.6% | 3.7% | +0.0 |
| helper/child: execve | 7.2% | 7.3% | +0.1 |
| helper/child: exit and address-space teardown | 9.2% | 8.7% | -0.5 |
| helper/child: page faults | 7.7% | 7.4% | -0.3 |
| helper: wait, poll, pipes and socket syscalls | 10.7% | 10.6% | -0.0 |
| helper: user space and other kernel | 2.1% | 2.2% | +0.1 |
| busybox task: user space and other kernel | 7.9% | 8.1% | +0.1 |
| relish (raw scenario spawner and bench client) | 0.0% | 0.0% | -0.0 |
| other processes and kernel threads | 3.6% | 2.5% | -1.1 |

Category shifts, idle cluster, share of Bun (`perfcpu-idle`):

| Category | v0.2.8 | v0.3.3 | Points |
|---|---:|---:|---:|
| Bun: task-array leader tick and node sync | 1.7% | 2.3% | +0.6 |
| Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs) | 0.9% | 0.8% | -0.1 |
| Bun: Raft core and log | 0.9% | 3.8% | +3.0 |
| Bun: agent loop and supervisor | 7.8% | 7.7% | -0.1 |
| Bun: API server | 10.3% | 11.5% | +1.2 |
| Bun: metrics (Mayo) | 15.5% | 6.9% | -8.6 |
| Bun: service discovery (Onion) | 1.7% | 0.0% | -1.7 |
| Bun: containers (Grill) | 29.3% | 46.9% | +17.6 |
| Bun: other Reliaburger code | 12.1% | 7.7% | -4.4 |
| Bun: async runtime (scheduler, timers, parking) | 19.8% | 12.3% | -7.5 |

### Would flag

The rules don't gate a release yet (maintainer decision, 26 October 2026). Each line below needs a sentence saying why, in place of its *Explanation*.

- **explain**, category-move: Bun: metrics (Mayo) (idle cluster, share of Bun). 15.5% to 6.9%, -8.6 points. *Explanation:* 18 samples to 9. Nearly all of them are Mayo's `sysinfo` walk of `/proc` on rayon's threads (17 and 9 samples), which runs every 10 s, so how many walks land in a short idle window decides the share. Under host jobs Mayo is 0.0% of the VM in both.
- **explain**, category-move: Bun: containers (Grill) (idle cluster, share of Bun). 29.3% to 46.9%, +17.6 points. *Explanation:* 34 samples to 61, 0.15% of the VM. The same functions in both versions: `ProcessControl::inspect_status` (16 samples to 31), `finish_retirement` (13 to 18) and `process_owner::load` (10 to 13), the process-control polling of executors left over from the scenarios before the idle window, and the log capture reader (14 in both). It isn't more work overall: Bun's measured idle CPU fell from 1.85% to 1.76% of a core, so the samples moved here from Mayo and the async runtime. Worth a look if the next profile shows it again.
- **explain**, category-move: Bun: async runtime (scheduler, timers, parking) (idle cluster, share of Bun). 19.8% to 12.3%, -7.5 points. *Explanation:* 23 samples to 16, the other side of the Grill line: with the total nearly unchanged (116 and 130 samples), a few more samples in Grill's blocking tasks leave fewer for the scheduler and timers. Under host jobs the async runtime is 1.3% and 1.4% of the VM.

## Rates

### v0.2.8 (previous)

| Scenario | Rate, median (range) | Steady 15-58 s | VM CPU busy | Bun's CPU | Start lag p50 / p99 | Usable windows, task failures |
|---|---:|---:|---:|---:|---:|---|
| Raw VM processes (calibration) | 33,864.3 (33,294.8-37,601.2)/s | - | 90% | 0.01 cores | - | 3 of 3, 0 |
| Host jobs, default cap | 15,500.0 (15,500.0-15,500.0)/s | 15,881/s | 62% | 0.48 cores | 1 / 4 ms | 3 of 3, 0 |
| Shared containers | 11,333.3 (11,300.0-11,500.0)/s | 11,886/s | 94% | 0.50 cores | 1 / 4 ms | 3 of 3, 0 |

Idle cluster: Bun used 1.85% of one core (median of 3), 291 context switches a second, RSS 170.4 MiB.

### v0.3.3 (candidate)

| Scenario | Rate, median (range) | Steady 15-58 s | VM CPU busy | Bun's CPU | Start lag p50 / p99 | Usable windows, task failures |
|---|---:|---:|---:|---:|---:|---|
| Raw VM processes (calibration) | 32,961.3 (32,738.9-36,969.8)/s | - | 90% | 0.01 cores | - | 3 of 3, 0 |
| Host jobs, default cap | 15,500.0 (15,500.0-15,500.0)/s | 15,881/s | 63% | 0.49 cores | 1 / 4 ms | 3 of 3, 0 |
| Shared containers | 11,416.7 (11,383.3-11,533.3)/s | 12,134/s | 94% | 0.51 cores | 1 / 4 ms | 3 of 3, 0 |

Idle cluster: Bun used 1.76% of one core (median of 3), 290 context switches a second, RSS 175.1 MiB.

## Where the time goes

v0.2.8, host jobs, share of the VM (`perfcpu-host`, 39,920 samples):

| Category | Share | µs per task |
|---|---:|---:|
| idle (no runnable work) | 35.8% | 92.5 |
| Bun: GitOps loop | 0.0% | 0.0 |
| Bun: task ledger (group commit, fsync) | 0.1% | 0.2 |
| Bun: task-array leader tick and node sync | 10.4% | 27.0 |
| Bun: Raft core and log | 0.0% | 0.1 |
| Bun: agent loop and supervisor | 0.0% | 0.1 |
| Bun: API server | 0.0% | 0.1 |
| Bun: metrics (Mayo) | 0.0% | 0.1 |
| Bun: gossip (Mustard) | 0.0% | 0.0 |
| Bun: containers (Grill) | 0.1% | 0.2 |
| Bun: other Reliaburger code | 0.0% | 0.1 |
| Bun: async runtime (scheduler, timers, parking) | 1.3% | 3.3 |
| helper/child: clone3 (process creation) | 3.6% | 9.4 |
| helper/child: execve | 7.2% | 18.6 |
| helper/child: exit and address-space teardown | 9.2% | 23.7 |
| helper/child: page faults | 7.7% | 20.0 |
| helper: wait, poll, pipes and socket syscalls | 10.7% | 27.5 |
| helper: user space and other kernel | 2.1% | 5.5 |
| busybox task: user space and other kernel | 7.9% | 20.5 |
| relish (raw scenario spawner and bench client) | 0.0% | 0.1 |
| other processes and kernel threads | 3.6% | 9.3 |

v0.2.8, idle cluster, share of Bun (`perfcpu-idle`, 39,901 samples):

| Category | Share |
|---|---:|
| Bun: task-array leader tick and node sync | 1.7% |
| Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs) | 0.9% |
| Bun: Raft core and log | 0.9% |
| Bun: agent loop and supervisor | 7.8% |
| Bun: API server | 10.3% |
| Bun: metrics (Mayo) | 15.5% |
| Bun: service discovery (Onion) | 1.7% |
| Bun: containers (Grill) | 29.3% |
| Bun: other Reliaburger code | 12.1% |
| Bun: async runtime (scheduler, timers, parking) | 19.8% |

v0.3.3, host jobs, share of the VM (`perfcpu-host`, 39,920 samples):

| Category | Share | µs per task |
|---|---:|---:|
| idle (no runnable work) | 37.1% | 95.7 |
| Bun: GitOps loop | 0.0% | 0.0 |
| Bun: task ledger (group commit, fsync) | 0.1% | 0.2 |
| Bun: task-array leader tick and node sync | 10.6% | 27.3 |
| Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs) | 0.0% | 0.0 |
| Bun: Raft core and log | 0.0% | 0.1 |
| Bun: agent loop and supervisor | 0.0% | 0.1 |
| Bun: API server | 0.0% | 0.1 |
| Bun: metrics (Mayo) | 0.0% | 0.1 |
| Bun: gossip (Mustard) | 0.0% | 0.0 |
| Bun: service discovery (Onion) | 0.0% | 0.0 |
| Bun: containers (Grill) | 0.1% | 0.3 |
| Bun: other Reliaburger code | 0.0% | 0.1 |
| Bun: async runtime (scheduler, timers, parking) | 1.4% | 3.7 |
| helper/child: clone3 (process creation) | 3.7% | 9.5 |
| helper/child: execve | 7.3% | 18.8 |
| helper/child: exit and address-space teardown | 8.7% | 22.6 |
| helper/child: page faults | 7.4% | 19.2 |
| helper: wait, poll, pipes and socket syscalls | 10.6% | 27.4 |
| helper: user space and other kernel | 2.2% | 5.6 |
| busybox task: user space and other kernel | 8.1% | 20.8 |
| relish (raw scenario spawner and bench client) | 0.0% | 0.1 |
| other processes and kernel threads | 2.5% | 6.5 |

v0.3.3, idle cluster, share of Bun (`perfcpu-idle`, 39,903 samples):

| Category | Share |
|---|---:|
| Bun: task-array leader tick and node sync | 2.3% |
| Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs) | 0.8% |
| Bun: Raft core and log | 3.8% |
| Bun: agent loop and supervisor | 7.7% |
| Bun: API server | 11.5% |
| Bun: metrics (Mayo) | 6.9% |
| Bun: containers (Grill) | 46.9% |
| Bun: other Reliaburger code | 7.7% |
| Bun: async runtime (scheduler, timers, parking) | 12.3% |

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
| VM | `rb-profile-5c63de39`, Lima 2.2.1, VZ, 4 vCPU, 8 GiB, 60 GiB disk, template:ubuntu-24.04, no mounts, made for this run and deleted after |
| Guest | Ubuntu 24.04.5 LTS aarch64, Linux 6.8.0-142-generic, runc version 1.5.2, perf version 6.8.12 |
| BusyBox | Ubuntu's `busybox-static`, SHA-256 `52151e7f322f926b64049cdaa1410dc3ea6485525e0624b05813791c219ae933` |
| Previous | v0.2.8, commit `e423d469a93b04d7bdc64582be1922ac5f453fd1`, from `https://github.com/reliaburger/reliaburger/releases/download/v0.2.8`; `bun` `8a212344a4ccfc8fdefeff608ec3e74ce4f6bcbbe51bccda0da4a99a51c5568b`, `relish` `546d297331cc895e0d11360cb001c8a8ae3b529e6dfed854767792b2828923ae` |
| Previous, profiling build | artefact profiling-build-e423d469a93b04d7bdc64582be1922ac5f453fd1-1 of build run 43017213259 |
| Candidate | v0.3.3, commit `dd0337ada22cdeff1bad3b5b2fdbe8a957bd14dc`, from `https://github.com/reliaburger/reliaburger/releases/download/v0.3.3`; `bun` `23adb27bbf101b0c0068beb5918f346ce166b190fb6282f1ffe4709a76740087`, `relish` `ad47433ba5ad6a8c09ff7de2e011461b9343c929abfcf04d1c39d91f4f93f5a5` |
| Candidate, profiling build | artefact profiling-build-dd0337ada22cdeff1bad3b5b2fdbe8a957bd14dc-1 of build run 46025099701 |
| Qualified digest | `e64471a80595e383a3a10dc9bd398f931478649bc12f6bf039e8363fa8e5daeb` (candidate.json) |
| Scenarios | raw,host,shared,idle; 60 s windows, the `relish bench` defaults otherwise, beside a 500m BusyBox web app on a fresh single-node cluster |

### Host load

The 1-minute load and the count of `rustc` and `cargo` processes on the host, at the start and end of every session. No round is thrown away for load: on a busy Mac every round would be. The alternating rounds take the noise instead.

| Session | Load before | Load after | Compilers seen |
|---|---:|---:|---:|
| previous r1 | 2.35 | 1.80 | 0 |
| candidate r1 | 1.84 | 1.62 | 0 |
| candidate r2 | 1.60 | 1.55 | 0 |
| previous r2 | 1.46 | 1.54 | 0 |
| previous r3 | 1.38 | 2.32 | 0 |
| candidate r3 | 1.95 | 1.87 | 0 |
| previous perf | 1.73 | 1.29 | 0 |
| candidate perf | 1.17 | 1.22 | 0 |

Sampled every 10 s: load 0.76 to 5.16 (median 1.98), compilers running in 0 of 376 samples.

### Wall time

Estimated 69 min before the run; it took 62 min. 3 rounds per version.

## Reproduce

```sh
scripts/release/qualify-profile.sh --candidate https://github.com/reliaburger/reliaburger/releases/download/v0.3.3 --qualified-digest e64471a80595e383a3a10dc9bd398f931478649bc12f6bf039e8363fa8e5daeb --previous v0.2.8
```

## Files

| Path | What |
|---|---|
| `metadata.tsv`, `host-load.tsv` | What the script knew about the run, and the host's load |
| `summary.json`, `comparison.json` | Every window's numbers, and the comparison with its findings |
| `raw/LABEL/rN/` | One rate session: bench reports and progress, CPU snapshots, `vmstat`, `batch-status`, `binaries.txt` |
| `raw/LABEL/perf/` | The profiling session: folded stacks, top frames, category tables, `perf sched latency`, the probe timeline |
| `flamegraphs/` | On-CPU and off-CPU flamegraphs, and the differential ones |

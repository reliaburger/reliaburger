# V02 loop bounds on a5cc6184 (12 Nov 2026)

Lane D of the V02 qualification: [`v02-loops.yml`](../../.github/workflows/v02-loops.yml)
loops the upgrade, council and lease-recovery suites on GitHub's hosted
runners and reports a rule-of-three bound for every test
([#287](https://github.com/reliaburger/reliaburger/issues/287), second bullet).
The lane had never produced a record. This is its first.

**Result: every lane passed: 92 tests across 19 lanes, 9,099 runs, 0 failures.**
Before it could say that, the lane needed three fixes of its own, below.

## Environment

| Item | Value |
|---|---|
| Run | [43183887759](https://github.com/reliaburger/reliaburger/actions/runs/43183887759), dispatched by hand with the defaults (`duration` 5h30m, `count` 200) |
| Source | [`a5cc6184`](https://github.com/reliaburger/reliaburger/commit/a5cc6184ff8eeb38ad61449d2406ae1ee88a3375) on `0-3-0/v02-loops-bounds`: main at [`e242ae0f`](https://github.com/reliaburger/reliaburger/commit/e242ae0f598f7ec569eeb473428970523a40c7db) (0.2.8 plus its post-release docs) and the archive fix. No product code differs from main |
| Runners | `ubuntu-latest` (x86-64), `ubuntu-24.04-arm`, `macos-15`; Rust 1.98.0, nextest 0.9.151 |
| Time | 2026-11-12 08:33 GMT to 2026-11-12 15:12 GMT |
| Summariser | `loop_summary.py` as fixed in [`1dd48ce4`](https://github.com/reliaburger/reliaburger/commit/1dd48ce4), replayed over the run's uploaded JUnit reports and logs |

## Results

One row per lane. Every test in a lane ran the same number of times; the
[per-test table](2026-11-12-v02-loops-bounds/per-test.md) names each one.

| Lane | Platform | Tests | Runs per test | Failures | 95% bound (rule of three) |
|---|---|---|---|---|---|
| `test-upgrade-node`, 5h30m | x86-64 | 7 | 106 | 0 | < 2.83% |
| | arm64 | 7 | 146 | 0 | < 2.05% |
| | macOS | 7 | 154 | 0 | < 1.95% |
| `test-upgrade-cluster`, 5h30m | x86-64 | 11 | 36 | 0 | < 8.33% |
| | arm64 | 11 | 36 | 0 | < 8.33% |
| | macOS | 11 | 35 | 0 | < 8.57% |
| `council_disaster_recovery`, 5h30m | x86-64 | 9 | 165 | 0 | < 1.82% |
| `cluster_failover`, 5h30m | x86-64 | 15 | 53 | 0 | < 5.66% |
| `registry-lease-expiry` ×200 | x86-64, arm64, macOS | 1 | 200 | 0 | < 1.50% |
| `registry-new-leader` ×200 | x86-64, arm64, macOS | 1 | 200 | 0 | < 1.50% |
| `export-lock-checkpoint` ×200 | x86-64, arm64, macOS | 2 | 200 | 0 | < 1.50% |
| `test-storage-owner-death` ×200, as root on Btrfs | x86-64, arm64 | 1 | 200 | 0 | < 1.50% |

Zero failures in *n* independent runs bounds the true failure rate below 3/*n*
at 95% confidence. It says nothing about a rarer race. The cluster upgrade
bound is the weakest: each iteration runs eleven tests that each start a
three-node cluster, so five and a half hours buys 35 or 36 of them.

No loop broke its bound. The two tests that failed in September's run
passed every time here: `isolated_replacement_reverts_without_restarting_its_workload`
(150 of 150 failed on macOS then, 0 of 406 now across the three platforms) and
`new_leader_retains_registry_cleanup_until_the_killed_writer_returns`
(2 of 200 on x86-64 then, 0 of 600 now). Both September failures came from the
harness. Every upgrade iteration after the first found the previous one's
workloads still holding its fixed port, until the workload reaper
([`eced11c0`](https://github.com/reliaburger/reliaburger/commit/eced11c0)) and
the port leases ([#734](https://github.com/reliaburger/reliaburger/issues/734))
landed.

## What the lane needed first

The run's own jobs all show as failed. Each one failed in its Summarise step,
after its loop step passed (the job-lease loops excepted, below). Getting to a
record took three dispatches.

**The archive build never finished.** The build job archived every test
binary in the workspace. In the first dispatch,
[run 43174277022](https://github.com/reliaburger/reliaburger/actions/runs/43174277022)
on main, the `macos-15` build ran into its 60-minute limit and the arm runner
got a shutdown signal mid-compile (exit 143), as it had in September's
[run 36114785981](https://github.com/reliaburger/reliaburger/actions/runs/36114785981).
Every loop needs the build job, so one failed platform skipped all of them.
[`a5cc6184`](https://github.com/reliaburger/reliaburger/commit/a5cc6184ff8eeb38ad61449d2406ae1ee88a3375)
archives only the binaries the loops select and gives the job 90 minutes.
The three builds then took 3.5, 12 and 15 minutes. We didn't prove that memory
killed the arm runner; both failures came about five minutes into compiling
the crate from a cold cache, and the smaller archive hasn't failed since.

**The summariser couldn't read a coloured log.** The workflow sets
`CARGO_TERM_COLOR=always`, and nextest colours its
`Summary [...] 200/200 stress run iterations` line, so `loop_summary.py` found
no iteration count and called every lane incomplete. A clean 200 of 200 failed
its job. [`1dd48ce4`](https://github.com/reliaburger/reliaburger/commit/1dd48ce4)
strips the escape codes first. This run kept the old summariser, so the table
above is the fixed one replayed over its artefacts; the loops themselves are
untouched.

**The job-lease loop had no test.**
[`2560765c`](https://github.com/reliaburger/reliaburger/commit/2560765c) retired
node-local jobs on 10 October, and with them
`node_job_lease_reaps_a_surviving_process_after_bun_is_killed`. Its loop ran
nothing on all three platforms and failed with "no tests to run". Job test
leases now live in Raft beside application leases, which `cluster_failover`'s
lease tests loop above.
[`2276b48f`](https://github.com/reliaburger/reliaburger/commit/2276b48f) drops
the lane and adds a check that every test a lease loop names still exists.
Those three job-lease rows are the only ones left out of the table.

## Host load

Runners were scarce. Other branches' CI, mutation and fuzz runs held the
account's runners, so the loops started between 09:03 and 10:03 GMT, not
together, and the run took 6 h 39 min from dispatch to the combined summary.
A time-boxed loop's run count depends on its runner, so these bounds come from
GitHub's hosted machines on one day, not from a quiet host.

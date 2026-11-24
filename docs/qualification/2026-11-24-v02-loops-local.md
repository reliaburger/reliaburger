# V02 loop bounds on one Mac Studio, 00e4d204 (23–24 Nov 2026)

Lane D of the V02 qualification
([#287](https://github.com/reliaburger/reliaburger/issues/287), second bullet),
run for the first time on the Mac Studio instead of GitHub's runners, with
[`qualify-v02-loops-local.sh`](../../scripts/release/qualify-v02-loops-local.sh).
The [12 Nov record](2026-11-12-v02-loops-bounds.md) is the GitHub run it
copies.

**Result: FAIL, and not a bound we can use.** 73 tests across 13 lanes ran
6,083 times. 82 runs failed, in four tests. The host wasn't quiet: five to
eight agents ran `make ci`, `make test` and `make test-cluster` on the same
Mac for the whole run. Two of the four failures are port collisions with
other processes on the host, one is load or a collision inside the VM, where
two lanes share the loopback, and the fourth is most likely a process-owner bug
already fixed after this candidate
([#2236](https://github.com/reliaburger/reliaburger/issues/2236)). None of them
shows a new product bug, but a loop this noisy can't rule one out either, so
the macOS upgrade lane needs a rerun on a quiet host before this lane gives a
number.

## Environment

| Item | Value |
|---|---|
| Source | `00e4d204b` on `0-3-2/v02-loops-local`: the 0.3.2 release candidate [`2aae7e2a`](https://github.com/reliaburger/reliaburger/commit/2aae7e2a0) plus the local runner. No product code differs from 0.3.2 |
| Runner | `scripts/release/qualify-v02-loops-local.sh --parallel --window 11h` (5h30m loops, `count` 200), from `~/github/rb-wt/032-v02-local` |
| Host | Mac Studio (M5 Max), macOS 27.2, 18 cores, 128 GiB; rustc 1.99.1, nextest 0.9.151 |
| VM | Lima 2.2.1, `reliaburger-test`: aarch64, 8 CPUs, 39 GiB, kernel 6.8.0-149-generic; rustc 1.99.1, nextest 0.9.152 |
| Time | 2026-11-23 16:57 GMT to 2026-11-24 03:49 GMT (10 h 51 min). Archives built by 17:01 |
| Evidence | `~/github/rb-wt/v02-local-runs/2026-11-23/` on the Studio: each lane's `loop.log`, `junit.xml`, `summary.md` and `lane.sh`, `environment.txt`, `schedule.log`, and the [combined summary](2026-11-24-v02-loops-local/per-test.md), kept here |

## Results

One row per lane. Every test in a lane ran the same number of times; the
[per-test table](2026-11-24-v02-loops-local/per-test.md) names each one.

| Lane | Platform | Loop | Tests | Runs per test | Failures | 95% bound (rule of three) |
|---|---|---|---|---|---|---|
| `test-upgrade-node` | macOS | 5h17m | 7 | 142 | 76, all in one test | six tests < 2.11%; one failed |
| | arm64 | 5h18m | 7 | 175 | 4, all in one test | six tests < 1.71%; one failed |
| `test-upgrade-cluster` | macOS | 5h24m | 11 | 34 | 1 | ten tests < 8.82%; one failed |
| | arm64 | 5h24m | 11 | 43 | 0 | < 6.98% |
| `council_disaster_recovery` | arm64 | 3h09m | 9 | 93 | 0 | < 3.23% |
| `cluster_failover` | arm64 | 3h11m | 19 | 20 | 0 | < 15.00% |
| `registry-lease-expiry` ×200 | macOS, arm64 | | 1 | 200 | 0 | < 1.50% |
| `registry-new-leader` ×200 | macOS | | 1 | 200 | 1 | failed |
| | arm64 | | 1 | 200 | 0 | < 1.50% |
| `export-lock-checkpoint` ×200 | macOS, arm64 | | 2 | 200 | 0 | < 1.50% |
| `test-storage-owner-death` ×200, as root on Btrfs | arm64 | | 1 | 200 | 0 | < 1.50% |

The time-boxed loops are shorter than GitHub's 5h30m because `--window 11h`
fitted each host's queues into eleven hours, builds included. The two council
lanes run on arm64 here, where the workflow runs them on x86-64, and they got
the least time: `cluster_failover`'s 20 runs give the weakest bound in the
table.

### The skipped lanes

The Studio has no x86-64 host, so
[`v02-loop-lanes-local.json`](../../scripts/release/v02-loop-lanes-local.json)
skips all eight x86-64 lanes, and the summary lists them as skipped:
`test-upgrade-cluster`, `test-upgrade-node`, `council_disaster_recovery`,
`cluster_failover`, `registry-lease-expiry`, `registry-new-leader`,
`export-lock-checkpoint` and `test-storage-owner-death`. On GitHub those run on
`ubuntu-latest`. This record bounds nothing on x86-64.

## Host conditions

The runner held the quiet lock, which pauses the power-cut endurance run, and
[testing.md](../testing.md#loops-on-one-mac) says nothing else should build or
test on the Mac meanwhile. That didn't hold. Agents working on the 0.3.3 train
(worktrees `~/github/rb-wt/033-*`) ran at background priority
(`taskpolicy -b`) for the whole ten hours: their session logs show 33 runs of
`make ci`, `make test`, `make test-doc`, `make test-cluster` or `make test-slow`
started between 18:09 and 03:43 GMT, and many single-test `cargo nextest` runs.
Five to eight agents were working at any one time. Load average was 20 to 66 when
sampled; no load log was kept, so we can't line each failure up with a load
figure.

Two things follow. Every agent's portable suite starts real Bun processes on
loopback ports in the same 27000–29999 range the upgrade and registry fixtures
lease from, on the same host. And the VM's two queues (upgrade and
cluster-heavy) shared its eight vCPUs and its loopback by design.

Nothing in the session logs ran `make test-upgrade`, `make test-upgrade-node` or
any `self_upgrade` binary with `RELIABURGER_UPGRADE_TESTS=1`. The portable
suite builds the `self_upgrade` binary and runs it once with `--help`
(`warm_executables.py`), but its tests are all `#[ignore]`d, so no agent bound
`tests/self_upgrade.rs`'s fixed ports 46011–46071.

## The failures

### 1. `self_upgrade::isolated_replacement_reverts_without_restarting_its_workload`, macOS, 76 of 142

**Most likely a product bug already fixed after this candidate (#2236),
exposed by load. Not proven.**

74 of the 76 failed at `tests/self_upgrade.rs:802:52`, `called Option::unwrap()
on a None value`, 3.3 to 9.2 s into the test (59 of them under 3.7 s). A pass
takes 32 to 43 s. Line
802 is the first `workload_pid("rejoin")` straight after `deploy_testapp`'s wait
saw port 46071 serve HTTP: `/v1/status` listed no pid for the app. The failures
ran from 22:32 to 03:42 GMT, the whole loop, in runs of one to five separated by
runs of one to five passes, starting with iteration 3. No other test in the lane
failed, and the other three tests that read a pid straight after a deploy (on
46011–46031, without `--cluster`) never came back empty in 426 reads.

The other two failures were port collisions (failure 3 below):

- Iteration 89, 01:44 GMT, `tests/self_upgrade.rs:500:9`, "timed out waiting for
  node healthy": every Bun start for 30 s got `failed to bind Bun API on
  127.0.0.1:28120: Address already in use`.
- Iteration 120, 02:54 GMT, `tests/self_upgrade.rs:249:17`, "timed out waiting
  for version v0.2.0": the v0.2.0 replacement kept exiting with `reporting bind
  failed ... 127.0.0.1:27096: Address already in use`.

Why not a collision on port 46071: only `tests/self_upgrade.rs` uses it, in the
candidate and in every 033 worktree, no agent ran that suite, and 46071 is below
macOS's ephemeral range (49152–65535). The 26 September stale-listener failure
([flakes register](../flakes.md)) looked like this one, a pid of `None` straight
after the deploy wait, but its fix is still in place: the harness reaps every
process under its root on every path and `shutdown` asserts nothing is left, and
that assertion never fired here. No `testapp` or owner process from the run was
left on the host afterwards.

What fits is [#2236](https://github.com/reliaburger/reliaburger/issues/2236),
found on this Studio the same evening. A process owner answered one client per
10 ms turn, and a status read asks it three questions at once (liveness, pid,
exit code). On a loaded host the turn runs long, the pid answer misses the
status reader's 500 ms deadline, and `/v1/status` reports the instance with
`pid: null` and `runtime_unknown: true`. `workload_pid` reads one status and
takes that as "no pid". The fix, [PR #2249](https://github.com/reliaburger/reliaburger/pull/2249),
landed on `release-1-3-3` on 24 November, after this candidate; under six busy
loops on the Studio it took `job_recovery` from 21 failures in 200 runs to 0 in
300. What this doesn't explain is why only the one test that runs Bun with
`--cluster` failed. The failing runs kept no `/v1/status` body, so
`runtime_unknown` isn't confirmed. The GitHub run of 12 November passed this
test 154 of 154 on `macos-15`, and the arm64 VM passed it 175 of 175 here.

### 2. `self_upgrade::single_node_upgrade_preserves_running_containers`, arm64, 4 of 175

**Load or a collision inside the VM; not a product bug on this evidence.**

All four failed at `tests/self_upgrade.rs:500:9`, "timed out waiting for web
serving on 46012", at 30.78 to 30.87 s, when a pass takes 17.5 to 23.4 s. That's
the second deploy, which replaces the running `web` on 46011 with a canary
generation on 46012, given 30 s to serve. Bun printed no error. The failures
came at 00:40, 01:37, 01:57 and 02:34 GMT, all while the VM's cluster-heavy
queue ran `cluster_failover` (00:38 to 03:49), with its eight-node clusters, on
the same eight vCPUs. The 2 h 12 min of overlap with `council_disaster_recovery`
before that had none; 0 of about 70 against 4 of about 105 is suggestive, not
significant.

A collision is possible here, where it isn't on macOS: 46011–46071 sit inside
Linux's ephemeral range (32768–60999), so an outgoing connection or a `bind(0)`
listener from `cluster_failover`'s nodes can hold 46012 when the canary binds
it. That all four hit 46012, and never the other five fixed ports, isn't
explained by either cause. The Mac failed this test 0 times in 142, and the VM
ran it 171 other times without a slow deploy.

### 3. `self_upgrade_cluster::paused_upgrade_can_be_aborted_or_replaced_by_a_rollback`, macOS, 1 of 34

**A port collision on the shared host.**

Iteration 14 failed at `tests/self_upgrade_cluster.rs:122:9`, "timed out waiting
for the full council of voters", after 228 s, starting at 19:05 GMT. The third
node restarted again and again with `failed to start cluster runtime: reporting
bind failed ... 127.0.0.1:28792: Address already in use`, so the council never
formed. 28792 came from `reserve_port_block_lease`, and the harness held its
lease for the whole test.

A lease only keeps out processes that take one. Every port the fixtures pick
comes from 27000–29999 ([`tests/support/bun_process.rs`](../../tests/support/bun_process.rs)),
but `reserve_address`, `reserve_ports` and `reserve_port_block` drop their lease
as they return, before Bun binds. The portable suite and `make test-cluster`
start Bun that way (`tests/suite/{app_metrics,bun_auth_startup,bun_startup_exit,container_runtime_required,missing_binary,node_renewal,recovery_seeds}.rs`,
`write_portable_node_config`, `documentation_first_run.rs`). If such a Bun picks
a port, and a lease holder probes it before that Bun binds, both think it's
free. On a loaded host Bun takes seconds to bind, which is a wide window.
Bun's default `[network] port_range`, 10000–32767, covers the whole lease range
too, so any Bun's app host ports can land in it.

No agent's test run shows in the session logs at 19:05, and the macOS
`registry-new-leader` lane was running beside it, so which process held 28792
isn't known. The two upgrade-node collisions at 01:44 and 02:54 came while
agents' `make test` and `make test-cluster` runs were going (01:30, 01:42,
01:45; 02:44), and so did failure 4.

### 4. `registry_recovery::new_leader_retains_registry_cleanup_until_the_killed_writer_returns`, macOS, 1 of 200

**A port collision on the shared host.**

Iteration 179, at 21:14 GMT, failed at `tests/support/bun_process.rs:198:13`
after 44 s: the writer's replacement, started on the address the killed writer
had, exited at once with `failed to bind Bun API on 127.0.0.1:27881: Address
already in use`. The test held 27881's lease from the start, and the writer had
served on it for 40 s before the kill, so another process bound it in the
moments between the kill and the restart. Two agents had started `make ci` at
21:10 and 21:11. The arm64 lane, in the VM's own network namespace, passed 200
of 200.

## What this record supports

- Nothing on macOS. Three of the five macOS lanes failed, and the 76 failures in
  `isolated_replacement_reverts_without_restarting_its_workload` hide whether
  that test also fails on a quiet host.
- On arm64, the bounds in the table hold for the lanes that passed, at this
  host's load. `test-upgrade-node` is the exception.
- No new product issue. Failure 1's likely cause is #2236, already fixed on the
  0.3.3 train.

Two harness fixes would stop the next record losing runs this way, and both
apply on GitHub's runners too: keep the lease of every port a fixture hands
Bun until that Bun exits, and move `tests/self_upgrade.rs`'s fixed testapp
ports into the leased range. The test's `workload_pid` should also wait while
`runtime_unknown` is set and print the status body when it gives up, so a
failure says which cause it was.

## Rerun

The macOS `test-upgrade-node` lane needs a rerun on a quiet host: no agents and
no other lanes on the Mac. At the workflow's 5h30m that's about 5 h 35 min with
the build; 142 runs, this record's count, took 5 h 17 min. It should run on the
0.3.3 train with #2249, since this candidate lacks the fix. The macOS
`registry-new-leader` lane (2 h 31 min) can run beside it in the other queue.
The macOS `test-upgrade-cluster` lane (5 h 24 min) shares the upgrade queue,
so rerunning it as well makes the quiet window about 11 hours. The arm64
`test-upgrade-node` lane can rerun in the VM alongside, with the VM's
cluster-heavy queue idle.

# Plan: a faster test and feedback loop

Created 6 November 2026, for the maintainer to review. It surveys every loop
we wait on, from `make ci` on a laptop to the 8-hour final, says where the
time goes with measured numbers, and ranks what would shorten each loop.
Nothing in CI, the Makefile or the code has changed; parts of it may go into
[0.2.7](archive/2026-11-04-plan-0.2.7.md).

It builds on the October plan, [CI and test
efficiency](2026-10-11-ci-and-test-efficiency.md), whose [18 October
status](2026-10-11-ci-and-test-efficiency.md#status-at-18-october-2026)
records what's done. [What the October plan left](#what-the-october-plan-left)
below says which of its items still matter.

## The short version

A pull request into a train waits about **28 minutes**, almost all of it for
one job, portable Linux, and a third of that job isn't the test suite. A full
run (a push to a train or to `main`, the nightly, the candidate build) waits
**45 to 190 minutes**, and the difference is the macOS queue: the
organisation's plan runs five macOS jobs at once, every full run asks for
three, and each of the three compiles the whole suite to run a third of a
three-minute test pass. A release then waits **15 hours** from the candidate
build to the final's verdict, because the nightly qualification that was
built to shorten it has never run.

The five changes worth the most, cheapest first:

| # | Change | Loop | Saves | Cost |
|---|---|---|---|---|
| [F1](#f1-one-macos-job-instead-of-three-shards) | One macOS job instead of three shards | Full runs | 2 of the 3 macOS slots each full run takes; about 60% of macOS minutes; most of the p90 queue of 90 min | XS |
| [F2](#f2-take-doctests-and-the-nightly-signing-test-off-the-light-path) + [F3](#f3-write-the-coverage-report-once) | Doctests and the nightly-signing test in their own job; one coverage report | Train PRs | portable Linux 28 → about 19 min | S |
| [F4](#f4-a-candidate-build-waits-for-mains-ci-instead-of-repeating-it) | A candidate build waits for `main`'s running CI instead of repeating it | Release | about 36 min and 3 macOS jobs per candidate | S |
| [F5](#f5-run-the-nightly-qualification-every-night) | Run the nightly qualification every night, with an automatic nightly build | Release gate | about 13 h of a full gate, 3 h of a patch gate, when the freeze finds a green night | S–M |
| [F7](#f7-a-studio-runner-for-the-macos-job) | A self-hosted runner on the Studio (or a second Mac) for the macOS job | Full runs | macOS compile 17–40 min → an estimated 5–8 | M, plus a security decision |

## How this was measured

- **CI:** every `ci.yml` run over 5 minutes from
  [run 41630708113](https://github.com/reliaburger/reliaburger/actions/runs/41630708113)
  (3 November, 11:50 UTC) to
  [run 42188209481](https://github.com/reliaburger/reliaburger/actions/runs/42188209481)
  (6 November, 08:30 UTC): 100 runs, 147 macOS jobs. Durations come from
  `gh api …/runs/ID/jobs`; queue time is a job's start minus its run's
  creation. Step and test-pass splits come from the step timestamps and from
  the evidence envelopes' `junit.xml`, whose `timestamp` is the moment
  nextest started and whose `time` is the test pass.
- **Release:** the 0.2.6 records,
  [build run 42011462991](https://github.com/reliaburger/reliaburger/actions/runs/42011462991)
  and the [0.2.6 closure](../qualification/2026-11-06-v0.2.6-release-closure.md).
- **Local:** not measured. The brief allowed one timed build, but the Studio
  was at a load average of 20 on 18 cores with other agents' builds and a
  24-hour soak running, so a `cargo build --timings` would have measured the
  neighbours and slowed the soak. [M1](#m1-measure-the-compile-on-a-quiet-host)
  says how to get the number.

## Where the time goes

### A pull request into a train (the light set)

Light-set jobs, medians over the sample:

| Job | Median | p90 | Max |
|---|---|---|---|
| portable Linux | 28.2 | 32.9 | 39.8 |
| source bundle builds | 15.0 | 16.6 | 17.0 |
| lint | 4.4 | 5.4 | 9.6 |
| minimum Rust 1.97.0 | 4.1 | 4.9 | 9.1 |
| CI policy | 1.1 | 1.4 | 1.8 |
| demo app, audit, contract boundaries | under 1 | | |

Portable Linux is the whole critical path. [testing.md](../testing.md#which-jobs-a-pull-request-runs)
says "about 25 minutes" and [releasing.md](../releasing.md#merge-trains-and-their-ci)
"15–20 minutes"; the second is out of date. Here's that job in the train
push [run 42126022681](https://github.com/reliaburger/reliaburger/actions/runs/42126022681)
(job 127590682976), 28.4 minutes:

| Part | Minutes | From |
|---|---|---|
| Setup, toolchain, cache restore, apt | 1.8 | step times |
| Instrumented build and test listing | 8.2 | step start 23:01:17 to nextest start 23:09:29 |
| The test pass | 6.8 | `junit.xml` `time="405.964"` |
| Three `llvm-cov report` passes (lcov, HTML, the floor) | 6.0 | nextest end 23:16:15 to step end 23:22:16 |
| Doctests | 3.7 | step |
| Nightly-signing key test (rebuilds the crate) | 2.5 | step |

So 12 of the 28 minutes are reports and two more builds of the crate that
the coverage run can't share. The tests themselves add up to 26.9 CPU-minutes;
the slowest is 42 s
(`registry_recovery::new_leader_retains_registry_cleanup_until_the_killed_writer_returns`),
and four `wrapper::proxy` tests wait out 30-second production timeouts.

### A full run

Pushes to `main` that passed took a median of **74.3 minutes** and up to
192.3 ([run 41647525047](https://github.com/reliaburger/reliaburger/actions/runs/41647525047)).
The Linux heavy jobs finish in under 35:

| Job | Median | Max |
|---|---|---|
| portable macOS (each shard) | 38.3–42.1 | 66.5 |
| privileged Linux (24.04 and 26.04) | 26.2–26.3 | 35.2 |
| benchmarks | 26.3 | 40.1 |
| task-array benchmarks | 17.3 | 30.6 |
| multi-node cluster, acceptance | 16.1 each, after build tests (4.4) | 22.4 |

Everything above 35 minutes is macOS. Across the 147 macOS jobs:

- **queue:** median 6.9 minutes, **p90 90.2**, max 159.6; 84 of the 147
  waited more than 5 minutes. The October numbers were p90 24 and worst 47
  ([testing.md](../testing.md#which-jobs-a-pull-request-runs)), so it has
  got worse, not better, as trains and agents multiplied.
- **what a shard does:** shard 3 of
  [run 42126022681](https://github.com/reliaburger/reliaburger/actions/runs/42126022681)
  spent 30 minutes compiling and 3.1 testing (`time="187.017"`, its third of the suite).
  On `main`'s
  [run 42186865091](https://github.com/reliaburger/reliaburger/actions/runs/42186865091),
  with a full cache match, the same shard compiled for 17 minutes and tested
  for 2.9. The three shards compile the identical suite three times. The
  runner is a 3-core M1 with 7 GB of RAM
  ([GitHub's runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners));
  the Linux `build tests` job, on 4 cores and 16 GB, compiles its subset in
  4.4.
- **the slot budget:** 5,737 macOS job-minutes in the three days, so on
  average almost two macOS jobs were busy around the clock, and a push to
  `main` also starts `build.yml`'s macOS `relish` build. One full run plus
  that build takes four of the five slots, so a second full run's shards
  queue ([Actions limits](https://docs.github.com/en/actions/reference/limits)).
- **work thrown away:** 4 of the 6 pushes to `release-0-2-7` in the sample
  were cancelled by the next merge, 31 to 52 minutes in. Cancelled runs burnt
  557 macOS job-minutes, about a tenth of the total.

The [merge queue](../releasing.md#merge-trains-and-their-ci) isn't in play:
the repository's only queue ruleset (25015244) still names
`refs/heads/release-0-2-1`, so trains 0.2.2 to 0.2.7 merge on the light set
and the push runs everything afterwards. The October plan flagged this as
urgent for 0.2.2 (B1) and it's still open.

### Caches

The repository holds 9.57 GB of caches against GitHub's 10 GB limit, all
saved by `main`: portable 1.47 GB, macOS 1.05, profiling 1.02, linux-debug
0.79, s3 0.79, linux-privileged 0.81 and so on. `linux-privileged-2604`
isn't in the list at all, so the Ubuntu 26.04 job most likely starts cold
whenever it's evicted. `s3` and `linux-debug` build the same profile under
separate keys. rust-cache never caches the workspace crate itself, and with
one crate of 443,000 lines that's most of every job's compile; that is the
ceiling on what any cache can do here.

### A release

The 0.2.6 candidate, from dispatch to verdict on 5–6 November (times UTC):

| Step | From | To | Wall |
|---|---|---|---|
| Candidate build ([42011462991](https://github.com/reliaburger/reliaburger/actions/runs/42011462991)) | 10:31 | 12:01 | 89 min |
| Stage ([42023854823](https://github.com/reliaburger/reliaburger/actions/runs/42023854823)) | | | 3 min |
| Power cut and reboot, 25 per lane | 12:10 | 12:50 | 40 min |
| V02 fast tier | 12:53 | 14:02 | 68 min |
| Jobs fast tier | 14:06 | 15:08 | 62 min |
| Jobs final, 8 h plus the 2¼ h quiet tail | 15:12 | 01:28 | 10 h 15 min |

About 15 hours, in series on one quiet host. Two things stand out:

- The build re-ran the whole of `ci.yml` (`validate-release`), because it was
  dispatched 3 seconds after the push to `main` whose CI it could have
  reused ([`ci_reuse.py`](../../scripts/release/ci_reuse.py) only accepts a
  run that has already passed). Its macOS shards then queued 38–43 minutes
  behind that same push run's shards.
- `nightly.py freeze-check` found no green nightly, because no nightly has
  ever run: there's no `~/reliaburger-nightly` on the Studio and no launchd
  job. Each night needs someone to dispatch `build.yml` and `stage.yml` by
  hand ([releasing.md](../releasing.md#nightly-qualification-on-the-train-tip)),
  and nobody did.

### Locally

`make ci` is `fmt-check lint test test-doc test-ci-scripts check-ignored`, in
that order ([Makefile](../../Makefile)). That's two Clippy passes of every
target (all features, then none), one test build plus a run of every binary
(`warm-executables`), the nextest pass, a doctest build, and only then the
seconds-long script checks. Each agent worktree also has its own target
directory: seven on the Studio today, 3.6 to 24 GB each, each built cold from
703 locked crates. The Linux linker is already fast on x86_64, where Rust 1.90
made `rust-lld` the default
([Rust 1.90 announcement](https://blog.rust-lang.org/2025/09/18/Rust-1.90.0/));
the aarch64 Lima VMs still link with the system linker.

## The changes, ranked

Each says what it speeds up, by how much, what it costs and what could go
wrong. Savings marked *estimate* need [M1](#m1-measure-the-compile-on-a-quiet-host)
or a trial run to confirm.

### F1. One macOS job instead of three shards

**Loop:** full runs, the candidate build, the macOS-labelled PR.
**Saves:** 2 macOS slots per full run. One job compiles once (17–35 min),
lints (4–7), runs every test (about 9, three shards' worth) and the doctests:
roughly 30–50 minutes in one slot, against about 120 job-minutes in three
today. Over the sample's 5,737 macOS minutes that's about 3,500 saved. With
one slot per full run, five full runs fit at once, so most of the 90-minute
p90 queue goes.
**Costs:** the macOS job is about 6 minutes longer than today's slowest shard
on an empty queue. `SHARDS` in `workflow_assembly.py` goes to 1, the matrix
goes, and the evidence gate's shard merge stays (it's correct for one).
**Risk:** low. [#846](https://github.com/reliaburger/reliaburger/issues/846)
already named this as the fallback: "If nothing helps, go back to one macOS
job, which frees runners". The shards came from
[#833](https://github.com/reliaburger/reliaburger/issues/833) and never moved
the critical path.
**When:** 0.2.7.

### F2. Take doctests and the nightly-signing test off the light path

**Loop:** every train PR.
**Saves:** about 6 minutes of portable Linux (3.7 doctests, 2.5 nightly
signing). Both build the crate without instrumentation, so they don't need
to wait behind the coverage run.
**Costs:** one new Linux job, "doctests and feature tests", restoring the
`linux-debug` cache; or both steps move to `source bundle builds`, which
already compiles an uninstrumented crate and finishes at 15 minutes (it'd go
to about 21, still under portable Linux's new 19).
**Risk:** low. The evidence gate doesn't own doctests; `every_job.py` must
list the new job if there is one.
**When:** 0.2.7.

### F3. Write the coverage report once

**Loop:** every train PR.
**Saves:** an *estimated* 3–4 of the 6 minutes after the test pass. Today
three `cargo llvm-cov report` calls each read every binary's coverage map
again. One call can write the lcov file and enforce `--fail-under-lines`;
the HTML report only matters when someone opens it, so it can be written on
pushes to `main` and the nightly only, or generated from the lcov file
offline.
**Costs:** a change to `coverage_owner.py` and the `coverage` target. The
coverage floor stays exactly where it is.
**Risk:** low; check first that one `report` call accepts both the lcov
output and the floor (the
[cargo-llvm-cov README](https://github.com/taiki-e/cargo-llvm-cov#readme)
documents the flags; nobody has tried the combination here).
**When:** 0.2.7. With F2: **portable Linux 28 → about 19 minutes**.

### F4. A candidate build waits for `main`'s CI instead of repeating it

**Loop:** release, at freeze.
**Saves:** for 0.2.6, the build would have finished when `main`'s CI did
(52 minutes, in parallel with the 38-minute binary builds) instead of at 89,
and three macOS jobs would never have queued. About 36 minutes per
candidate, and 0.2.6 had two candidates.
**Costs:** `ci_reuse.py` learns one more answer: when a `push` run on the
same commit is queued or in progress, wait for it (poll, with a deadline of
say 120 minutes) and then apply the existing rules. The `find passing CI`
job's timeout rises to match. Or, simpler, the runbook says to dispatch only
after `main`'s CI is green; the script change makes that impossible to
forget.
**Risk:** low. Every rule about what may be reused stays.
**When:** 0.2.7. Done in 0.2.8
([#1722](https://github.com/reliaburger/reliaburger/issues/1722)): the
script waits, and `candidate.json` names the run as `source_ci`.

### F5. Run the nightly qualification every night

**Loop:** the release gate.
**Saves:** a freeze with a green nightly on the candidate's tree runs only
the staged installs (two runs of about a minute) instead of 3 h 10 min of
fast tiers and power cuts, and 10 h 15 min of final for a full gate. For
0.2.6 that would have been about 2 hours from dispatch to verdict instead of
15.
**Costs:** the mechanism is built and tested (#1363, `nightly.py`); what's
missing is operation:
1. a scheduled workflow (or a `schedule:` on `build.yml`) that builds the
   tip of the active train in nightly mode at, say, 16:00 London and runs
   `stage.yml` with `channel=nightly` on success, so the 18:00 launchd job
   finds a staged build without anyone dispatching anything;
2. the [`nightly-qualify.plist.example`](../../scripts/release/nightly-qualify.plist.example)
   installed on the Studio, with the train name in one place;
3. for a full gate, the nightly-key soak `bun`, built and signed on the
   Studio before the final starts, which `nightly.py` could do itself before
   its first tier.
**Risk:** medium. The Studio is then busy every night from 18:00 to about
08:00 for a full gate, which is the time agents build too; the quiet-host
rule already forbids that during soaks, so evening agent work has to happen
on the laptop. A nightly that fails needs same-day triage, or it's noise.
**When:** the automation in 0.2.7, so 0.2.7's own freeze can use it.

### F6. Stop throwing away train runs

**Loop:** full runs on a train.
**Saves:** most of the 557 cancelled macOS minutes in three days (a tenth of
all macOS time), and every train commit gets a finished full run.
**Two ways:**
- **(a) the merge queue on every train**, as the docs already claim: change
  ruleset 25015244 to include `refs/heads/release-*`, with batching (up to
  five PRs, a few minutes' wait), so one full run validates a batch before
  it lands. The push that follows lands exactly the commit the queue tested,
  so the push run can then be skipped when a successful `merge_group` run
  on the same SHA exists (the check `ci_reuse.py` already makes). Merges
  take longer, roughly a full run after the light set, but nothing reaches
  the train untested.
- **(b) keep merging on the light set**, but don't let a train push cancel
  an older one that has already started its macOS job; or run the train's
  macOS job on a schedule (hourly, when the tip moved) instead of per push.
**Costs:** (a) is a repository setting plus a small change to the push
path; (b) is a change to the concurrency group.
**Risk:** (a) adds merge latency, and the queue's batching has never been
used here; (b) leaves the train ahead of its evidence for a while, as today.
**When:** a decision for the maintainer (D3); (a)'s setting is quick.

### F7. A Studio runner for the macOS job

**Loop:** full runs, and macOS PRs.
**Saves:** an *estimated* compile of 5–8 minutes instead of 17–40, on 18
cores and 128 GB instead of 3 and 7, and no queue at all. With F1 in place
the macOS job would take about 15 minutes, under the Linux heavy jobs.
**Costs and how to make it safe:**
- GitHub says self-hosted runners "should almost never be used for public
  repositories", because a pull request can run code on them
  ([secure use](https://docs.github.com/en/actions/reference/security/secure-use)),
  and runner groups refuse public repositories unless overridden
  ([runner access](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/manage-access)).
  So the Studio would take only `push`, `merge_group`, `schedule` and
  `workflow_dispatch` jobs, never `pull_request`; it runs as its own user,
  ideally in an ephemeral macOS VM (Apple's licence allows two per host)
  rebuilt per job, with no secrets in the job.
- **The quiet-host rule.** A build during a soak costs slow turns (the
  0.2.6 A/B run). So `runs-on` reads a repository variable,
  `macos-15` or the Studio's label, and every soak script and the nightly
  flip it to `macos-15` and stop the runner service before their first tier,
  and back afterwards. `nightly.py` already refuses to start while `rustc`
  runs; it would also refuse while the runner has a job.
- A second, small Mac (a Mac mini) as the dedicated runner removes the
  conflict entirely and keeps the Studio quiet. That's money, not code.
**Risk:** medium: security if the event filter is ever loosened, and a
runner that's offline leaves jobs waiting, which the variable handles.
**When:** later; decision D2.

### F8. Tidy the caches

**Loop:** every job that restores one.
**Saves:** about 0.8 GB (s3 shares `linux-debug`), which keeps the
26.04 job's cache from being evicted; small but free.
**Costs:** two `shared-key` lines.
**When:** 0.2.7.

### F9. Make `make ci` fail fast and share dependency builds locally

**Loop:** local, before every commit.
**Saves:** a script-check or ignored-owner failure turns up in seconds
instead of after the full build (reorder: `fmt-check test-ci-scripts
check-ignored lint test test-doc`). A local [sccache](https://github.com/mozilla/sccache)
cache shared by every worktree would let a new worktree reuse the 703
dependency builds; sccache doesn't cache incrementally compiled crates, so
the workspace crate still builds normally. For the aarch64 Lima VMs,
`rust-lld` (or mold) as the linker for `aarch64-unknown-linux-gnu`.
**Costs:** a Makefile reorder (XS); sccache is opt-in per developer, through
`RUSTC_WRAPPER`, and documented in testing.md.
**Risk:** low. *Estimate* only until M1.
**When:** the reorder in 0.2.7; the rest once measured.

### F10. Find out why macOS compiles so slowly

**Loop:** full runs.
The same suite compiles in 17 minutes on one macOS run and 30–45 on another,
and the hosted Mac has 7 GB of RAM for a 443,000-line crate. If it's paging,
fewer parallel jobs (`CARGO_BUILD_JOBS=2`) or dropping debug info for
dependencies (`[profile.test.package."*"] debug = false`) may beat the
default. Record `vm_stat` during the build for a few runs before changing
anything. **When:** with F1; it's measurement.

### F11. Split the crate (later)

**Loop:** every one.
One crate means every change rebuilds all 443,000 lines with a mostly
single-threaded front end, in every job and every worktree, and no cache can
help. A workspace of a few crates (wire types and protocol, the council,
runtimes, the CLI, the dashboard) would compile in parallel, let caches keep
the crates a change didn't touch, and make test-impact analysis possible:
nextest's `rdeps()` selects by package, so with one package it selects
everything. The October plan's small-crate pilot (B6) was deferred as
speculative; M1 gives it the evidence. **When:** 0.3.x at the earliest, as
its own plan.

### F12. Fast tiers on hosted runners in parallel (later)

`soak.yml` already runs the V02 fast tier on a hosted Linux runner with KVM
([releasing.md](../releasing.md#soaking-a-candidate-in-ci)), last used on
6 October. Running both fast tiers there each night, beside the Studio's
power cuts, would take 2 h 10 min off the Studio's night. But a hosted runner
is a noisy 4-core host, so its turn budgets can't gate; it'd be an early
warning, and the Studio's nightly still decides. **When:** after F5 runs.

### What isn't worth doing now

- **sccache on CI.** Within one run the jobs build different features and
  profiles (instrumented, `ebpf`, `embed-source`, release), and across runs
  the crate always changed. It would save dependency builds rust-cache
  already restores.
- **Test-impact analysis on today's crate.** The test pass is 6.8 minutes of
  a 28-minute job, and with one package nothing narrows the build.
- **More nextest shards anywhere.** Each shard repeats the compile, the
  expensive part.
- **Self-hosted Linux runners.** Hosted Linux is free and plentiful for a
  public repository, and the light set runs on pull requests, which can't
  safely use a self-hosted runner.

### M1. Measure the compile on a quiet host

Before F9 to F11, on the Studio between soaks: `cargo build --timings
--tests` from cold and after touching one file, with
`CARGO_TARGET_DIR=$HOME/github/rb-wt/target-ci-speed`, and the same as a
one-off `workflow_dispatch` step on `macos-15` that uploads
`target/cargo-timings/`. That splits front end, codegen and link, and says
which of F10 and F11 pays.

## What the October plan left

From its [18 October status](2026-10-11-ci-and-test-efficiency.md#status-at-18-october-2026),
checked against `release-0-2-7` today:

- **Done** and not revisited: Q2 (one extraction per job), Q3 (cancel
  superseded runs), Q5, B2's cron clock, B3, B4, S1, S2, S3, and the macOS
  double compile (#846).
- **Still open, and it matters more now:** B1, the merge queue. The ruleset
  still covers only `release-0-2-1`; this plan's F6 is the same item.
- **Still open, small:** Q4, the duplicate 65,536-chunk fixture
  (`run_one_task_chunks_on_ten_nodes` is still called from two tests in
  `src/meat/task_array_state.rs`), and B2's fixed settle sleeps in
  `tests/placement.rs` (3 and 6 seconds, five of them). Both are cluster-job
  minutes, off the critical path.
- **Superseded:** B6's "evaluate one shared macOS archive" becomes F1 (one
  job, no archive) and F7; the small-crate pilot becomes F11 after M1.

## For 0.2.7

Small, safe, each its own PR on the train with failing tests first where
there's code (the CI scripts have fixtures: `test_select_jobs.py`,
`test_ci_workflow.py`, `test_ci_reuse.py`, `test_shard_partition.py`):

1. F1, one macOS job.
2. F2 and F3, portable Linux to about 19 minutes.
3. F4, `ci_reuse.py` waits for a running push run.
4. F5's automation: the scheduled nightly build and stage, and installing
   the launchd job.
5. F8, the cache keys, and F9's reorder of `make ci`.
6. The doc fixes this survey found: [releasing.md](../releasing.md#merge-trains-and-their-ci)'s
   "15–20 minutes" and "where a train has one", and
   [testing.md](../testing.md#which-jobs-a-pull-request-runs)'s timings and
   macOS queue numbers. The CI section of book chapter 15, "Ready for
   Production", gets the shard reversal and its numbers.

F6(a) is a settings change the maintainer makes, if D3 says so.

## Later

F7 (after D2), F10 and M1, F11 (its own plan), F12 (after F5 has a few
weeks of nights), and the October leftovers Q4 and B2.

## Decisions for the maintainer

| | Question | Recommendation |
|---|---|---|
| D1 | Go back to one macOS job, accepting about 6 more minutes on an idle queue for two fewer slots per run? | Yes |
| D2 | A self-hosted macOS runner: on the Studio by day with an automatic switch around soaks, on a second Mac, or not at all? | A second Mac if the budget allows; otherwise the Studio by day, push and schedule events only |
| D3 | Trains: turn the merge queue on for `release-*` with batching (and skip the duplicate push run), or keep merging on the light set and stop cancelling started macOS jobs? | The queue: the docs already promise it |
| D4 | Reserve the Studio every night, 18:00 to about 08:00, for the nightly qualification, from 0.2.7 on? | Yes |
| D5 | Write the coverage HTML only on `main` and the nightly? | Yes; the lcov file and the floor stay on every run |
| D6 | When to plan the crate split? | After M1, not before 0.3.0 ships |

## Sources

- CI runs, all in [reliaburger/reliaburger Actions](https://github.com/reliaburger/reliaburger/actions):
  [41630708113](https://github.com/reliaburger/reliaburger/actions/runs/41630708113) (sample start),
  [42188209481](https://github.com/reliaburger/reliaburger/actions/runs/42188209481) (sample end),
  [42126022681](https://github.com/reliaburger/reliaburger/actions/runs/42126022681) (train push, step and test splits),
  [42186865091](https://github.com/reliaburger/reliaburger/actions/runs/42186865091) (`main` push, full cache match),
  [42132719996](https://github.com/reliaburger/reliaburger/actions/runs/42132719996) and
  [42119636996](https://github.com/reliaburger/reliaburger/actions/runs/42119636996) (train PRs),
  [41647525047](https://github.com/reliaburger/reliaburger/actions/runs/41647525047) (slowest `main` push),
  [42109515439](https://github.com/reliaburger/reliaburger/actions/runs/42109515439) (a cancelled train push),
  [42011462991](https://github.com/reliaburger/reliaburger/actions/runs/42011462991) (0.2.6 candidate build),
  [42023854823](https://github.com/reliaburger/reliaburger/actions/runs/42023854823) (0.2.6 stage).
- [0.2.6 release closure](../qualification/2026-11-06-v0.2.6-release-closure.md),
  [0.2.6 jobs final](../qualification/2026-11-06-v0.2.6-job-qualification.md),
  [V02 fast](../qualification/2026-11-05-v0.2.6-sustained-v02-fast.md),
  [jobs fast](../qualification/2026-11-05-v0.2.6-job-qualification-fast.md),
  [power cut](../qualification/2026-11-05-power-cut-reboot.md).
- [CI and test efficiency plan](2026-10-11-ci-and-test-efficiency.md),
  [testing.md](../testing.md), [releasing.md](../releasing.md),
  [`ci.yml`](../../.github/workflows/ci.yml),
  [`build.yml`](../../.github/workflows/build.yml),
  [`select-jobs.sh`](../../scripts/ci/select-jobs.sh),
  [`ci_reuse.py`](../../scripts/release/ci_reuse.py),
  [`nightly.py`](../../scripts/release/nightly.py),
  [`.config/nextest.toml`](../../.config/nextest.toml),
  [Makefile](../../Makefile).
- Issues: [#833](https://github.com/reliaburger/reliaburger/issues/833),
  [#846](https://github.com/reliaburger/reliaburger/issues/846),
  [#1363](https://github.com/reliaburger/reliaburger/issues/1363),
  [#1443](https://github.com/reliaburger/reliaburger/issues/1443).
- GitHub: [hosted runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
  (macos-15: 3 M1 cores, 7 GB; ubuntu-24.04: 4 cores, 16 GB),
  [Actions limits](https://docs.github.com/en/actions/reference/limits)
  (Free: 20 concurrent jobs, 5 macOS),
  [secure use of Actions](https://docs.github.com/en/actions/reference/security/secure-use),
  [managing runner access](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/manage-access),
  [merge queues](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue).
- Tools: [Rust 1.90 announcement](https://blog.rust-lang.org/2025/09/18/Rust-1.90.0/)
  (LLD by default on x86_64 Linux),
  [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov#readme),
  [sccache](https://github.com/mozilla/sccache).

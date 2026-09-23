# CI feedback loop and test layout

23 September 2026. Analysis of the CI workflow and the test suite after PR #167
and the review tiers (#172–#175), with a staged plan to get fast, meaningful
feedback.

## Where we are

The numbers come from the last 60–150 `ci.yml` runs (19–23 Sep), per-step
timings from runs 35841711522, 35825495014 and 35754296716, and the JUnit from
run 35841814073.

**Time.** A green push takes 28–36 minutes of wall clock with no queue, and
44–64 minutes when pushes overlap. The critical path is `portable Linux`
(median 30.5 min, p90 37). The first failing job usually reports after
7–16 minutes. One push costs about 212 runner-minutes (190–275). PRs into
`main` also trigger `build.yml`, which adds 35–120 more.

**Most of that is compiling.** About two-thirds of runner time is compilation.
Every job builds its own copy of the same thing:

- The portable suite (about 4,150 tests, about 5 minutes per run) runs **seven
  times per push**:
  - `portable Linux` default and no-default;
  - `minimum Rust` default and no-default;
  - `coverage` default and no-default;
  - `portable macOS`.
- About 12 separate full test-profile builds of all 75 test binaries happen per
  push, at 8–9 minutes each when cold.
- `wall-clock acceptance`, `multi-node cluster` and both upgrade jobs each
  spend about 9 minutes compiling everything to run 3–31 tests (13 seconds to
  4 minutes of execution).
- Three optimised builds of about 21 minutes each: `fast benchmarks`,
  `large benchmarks` and `10k-member scale`. The 10k test itself runs for
  **0.08 seconds**.
- `make examples` does a separate dev-profile `cargo build` (up to 5 minutes)
  to run 21 dry-runs.

**The cache almost never hits.** The repository's Actions cache is at 10.38 GB
against GitHub's 10 GB limit. Thirteen jobs each keep their own 0.7–1.5 GB
key, so every PR evicts the last one. `main` holds no cache at all, and caches
are saved only when a job succeeds. The result:

- `portable Linux` missed the cache in 19 of 22 runs;
- `minimum Rust` missed in 16 of 22;
- about 40% of all jobs started cold.

**Half the minutes are thrown away.** 38 of 60 runs were cancelled by a newer
push: 4,727 of 8,911 runner-minutes (53%). We push every 8–20 minutes onto a
pipeline that takes 30 or more. Four stacked PRs pushed together queued for
9–23 minutes against the free plan's 20-concurrent-job cap.

**Runs that add no coverage.** `default = ["kubernetes"]` is the only default
feature, and nothing in `src/` is `cfg(not(feature = "kubernetes"))`. The
no-default run is the same 4,110 tests minus 40 Kubernetes ones. All it proves
is that the crate builds without `k8s-openapi`, which
`cargo clippy --no-default-features --all-targets` proves in seconds.

**Duplicates inside the privileged job.**
`scripts/release/qualify-oci-interruptions.sh` runs `owned_runc` (13) and
`owned_network` (4). `make test-linux` then runs the same 17 tests again,
because it excludes only `oci_crash`.

**Tests no job runs, or that pass without testing anything.**

| Test | Why it never runs |
|---|---|
| `grill::snapshot::tests::snapshot_restore_recovers_corrupted_data` | Btrfs-gated, but its name doesn't match `test-linux`'s `btrfs_` filter. The job even sets `RELIABURGER_BTRFS_TESTS`. |
| `registry_routable_push` | Not ignored; it returns early unless `RELIABURGER_ROUTABLE_TESTS` is set. It shows as **passing** in every portable run. |
| `grill::apple::tests::apple_serves_a_readonly_bind_mount_with_requested_identity_and_port` | Not selected even by `make test-apple`. |
| `bun::gpu::tests::nvidia_detector_finds_hardware`, `ketchup::export::tests::export_to_real_s3_manual` | Manual hardware and credential tests with no target. |

**Broken reporting.** `make test-no-default` overwrites
`target/nextest/ci/junit.xml`, so the uploaded JUnit covers the no-default run.
The "Verify JUnit output" step checks the file before it is overwritten.

**Flakes.** Most of the 92 failed jobs in the window were two deterministic
regressions that were later fixed (the authz route matrix, and Parquet on
Linux), so they weren't flakes. The open flakes are these:

| # | Test | Evidence | Likely cause | Kind |
|---|---|---|---|---|
| F1 | `oci_crash::normal_clustered_bun_recovers_enrolled_consumer_before_adoption` | Failed run 35849160255 attempt 1, passed on re-run; 8/8 in the VM | After recovery, retiring `-g1-0` loops on "original service withdrawal is unproven": the service map never shows the backend withdrawn | **Product race** |
| F2 | `cluster_failover::decommissioned_worker_releases_cleanup_and_stays_retired_after_leader_change` | 2/2 runs on different branches | The test waits for the new membership on the leader only (`tests/cluster_failover.rs:96-111`), then kills it before followers apply it | Harness (check whether the product has the same gap) |
| F3 | `placement::capacity_refusal_from_the_live_scheduler_forwards_without_committing_an_app` | 2 failures | Final `release_test_lease().unwrap()` doesn't retry the retryable 503 | Harness |
| F4 | `placement::concurrent_node_kills_and_leader_change_preserve_reserved_capacity` | 2 failures | `clear_fault` is acknowledged before quorum-risk accounting sees the node healthy, so the next inject gets 400 | **Product ordering** |
| F5 | `job_recovery::killed_bun_preserves_job_budget_and_requires_explicit_rerun` | macOS, 21.7 s against a 20 s budget | Fixed wall-clock budget on a loaded runner | Harness |
| F6 | `ebpf::standalone_discovery_recovery_preserves_original_routing_and_cleanup` | Once on `main` | The 2 s force-kill timeout in `src/bun/agent.rs:120,134` expires on a busy host | **Product timeout** |
| F7 | `mustard::protocol::tests::gossip_convergence_five_nodes` | Once | Unseeded probe RNG and bounded re-broadcasts: occasionally 4 of 5 members | Harness |

The EPIPE family (process owners) and the host-netns collisions are fixed (T2.9,
T1.11, `crash-recovery` test group).

**Slow tests.** Test time sums to about 1,150 seconds for about 300 seconds of
wall clock. 19 tests (231 s) take an exact multiple of 5 seconds because they
wait out their whole timeout on the happy path. Examples: the four
`process_recovery::cancelled_queued_*` tests at 20 s each, `wtf_watch` at 30 s,
and `documentation_first_run` at 190 s, which has grown well past the "two
first-run paths" its header describes.

**Layout overlaps.**

- 73 `tests/*.rs` files are 73 binaries, and each links the whole crate (about
  9 GB of debug executables). A trial that merged them into one binary cut the
  rebuild from 35–45 s to 15–16 s.
- `tests/chaos.rs::chaos_council_partition_majority_continues` is a weaker,
  sleep-based copy of
  `src/council/node.rs::partition_majority_continues_minority_cannot_write`.
- Cluster start-up and wait helpers are copied across placement,
  cluster_failover, cluster_gossip, chaos and reconstruction: `wait_until` in 5
  files, `local()` in 4, and `bun --cluster` wiring hand-copied twice.
- `tests/reconstruction.rs` mirrors two `reconstruction::diff` unit tests.
- `make examples` duplicates what `relish_cli` already does with
  `CARGO_BIN_EXE_relish`.

## Where we want to be

- **First signal in 3–5 minutes.** fmt, clippy (both feature sets), doctests.
- **Full portable signal in about 12–15 minutes.** One instrumented Linux run
  gives tests, coverage and JUnit in one go, alongside macOS.
- **Heavy acceptance built once.** Cluster, upgrade, wall-clock and privileged
  Linux jobs run in parallel from shared build artefacts.
- **Pre-merge and nightly split.** Benchmarks and scale run on `main` and
  nightly.
- **Totals.** About 90–110 runner-minutes per push, critical path about 15–18
  minutes.
- **Flake policy.** No known flakes: every open flake is fixed or tracked here
  with a cause. `retries = 0` stays. A failure that passes on a single re-run
  goes into this plan's flake table the same day.
- **No dead tests.** Every test runs in some job (or in a named manual target),
  and a gated test that returns early is `#[ignore]`d instead of reporting a
  pass.

## Plan

One commit per item, stacked on the review tiers. CI changes don't have unit
tests, so each item's "test" is a CI run showing the expected jobs, test counts
and timings. Record the before and after numbers in the PR description.
Chapter 15 of the book gets a section on how and why the pipeline is laid out.

### Phase 1: stop wasting minutes (config only, about −100 runner-min/push)

- [ ] **C1.1 Fix the cache.**
  - Give the jobs that build the same profile a shared key (`shared-key` in
    `Swatinem/rust-cache`), with `save-if` true only on pushes to `main`.
    Everything else restores.
  - Add a small job on `main` that warms the shared key even when tests fail.
    This keeps us well under 10 GB, and every PR (stacked or not) starts from
    `main`'s cache.
  - Expected: portable Linux cold 36 min → warm about 23 min.
- [ ] **C1.2 Stop running the no-default suite.**
  - Replace `make test-no-default` (portable Linux), the minimum-Rust
    no-default run and the second `llvm-cov` pass with one
    `cargo clippy --no-default-features --all-targets -D warnings` in the lint
    step.
  - Update the Makefile, `make ci` and the READMEs.
  - Saves about 35 runner-minutes and one of the seven suite runs per job.
- [ ] **C1.3 Make minimum Rust a check.** Keep the locked
  `cargo check --all-targets` for both feature sets on 1.97. The two test runs
  (about 28 min) repeat the stable suite. They could come back as a nightly job
  if we ever see a behaviour difference between compilers.
- [ ] **C1.4 One instrumented Linux run.**
  - `portable Linux` runs the suite once, under `cargo llvm-cov nextest`, and
    enforces the 78.65% floor there.
  - The standalone `coverage` job goes.
  - JUnit then comes from the only run, which fixes the overwrite.
  - The trade-off: Linux tests run only instrumented, which is slightly slower
    per test but saves a whole job. See decision D1.
- [ ] **C1.5 Run the 10k acceptance in debug, inside `test-slow`.** It takes
  0.3 s in a debug build. Drop the `#[ignore]` reason that sends it to
  `bench-10k`, and delete the 21-minute release job.
- [ ] **C1.6 Benchmarks on `main` and nightly only.**
  - Merge `bench` and `bench-large` into one job (they share `target/release`)
    that runs on push to `main`, on a nightly `schedule`, and on PRs that touch
    `src/mustard/**` or `benches/**`.
  - Nothing gates on Criterion output today, so PRs lose no signal. See D2.
- [ ] **C1.7 Remove the privileged-job duplicates.**
  - Add `& not binary(owned_runc) & not binary(owned_network)` to
    `test-linux`, since the qualify script already runs them under `unshare`.
  - Raise `timeout-minutes` from 30 to 40. One green run was cancelled at the
    cap while saving its cache.
- [ ] **C1.8 Trim `build.yml` on PRs.**
  - Build only Linux x86-64 on PRs, and only when `Cargo.*`, `src/**` or
    packaging changes.
  - Build the PDF only when `docs/**` changes.
  - Keep the full four-target matrix for `main` and manual runs.
  - The macOS x86 release build (13–56 min) is the slowest job in the repo.
    See D3.
- [ ] **C1.9 Path filters for docs-only PRs.** A PR touching only `docs/**`,
  `*.md` or `website/**` runs fmt, the doc and link checks, and the two tests
  that read docs: `documentation_first_run` and the embedded manual in
  `src/relish/manual`. #175 spent 274 runner-minutes proving that Markdown
  compiles.

- [ ] **C1.10 Light path for stacked PRs (D5).** PRs whose base isn't `main`
  skip the acceptance suites (wall-clock, cluster, upgrade, privileged Linux)
  unless labelled `full-ci`. They still run lint, the portable suites and
  minimum Rust. PRs into `main`, pushes to `main` and the nightly run get
  everything. Before merging a stacked PR that GitHub retargeted to `main`,
  push or re-run it so the full set runs.

### Phase 2: build once, run everywhere (about −30 runner-min, shorter critical path)

- [ ] **C2.1 `cargo nextest archive`.**
  - One `build-tests` job (portable, default features) produces a nextest
    archive plus the `bun` and `relish` binaries.
  - `wall-clock acceptance`, `multi-node cluster` and both upgrade jobs
    download it and run with `--archive-file`. Each drops from about 9–13
    minutes to 1–5.
  - Merge `wall-clock acceptance`, `single-node upgrade` and `cluster upgrade`
    into one `acceptance` job that runs the three suites in sequence.
  - The privileged job keeps its own `ebpf` build.
- [ ] **C2.2 Examples as an integration test.** Replace `make examples` with a
  test that loops over `examples/**/*.toml` through `CARGO_BIN_EXE_relish`. That
  removes the separate dev build, and macOS runs it too.
- [ ] **C2.3 Faster first signal.** Put fmt and clippy in their own small job,
  so a lint failure reports in about 3–5 minutes instead of waiting behind the
  suite.

### Phase 3: fix the flakes (tests first: reproduce, then fix)

Reproduce each one before fixing: a loop of the single test under load (for
example `nextest run --test-threads=1` beside `stress-ng`, or 20 repeats in the
VM). A fix counts only when the loop that used to fail passes.

- [ ] **C3.1 (F1)** Find why the service-map withdrawal proof never arrives after
  clustered consumer recovery. Capture the discovery journal and service map
  when the loop spins. This is a product race and it blocks a clean #174 story.
  Check whether Tier 4's `drain_all` or inventory changes are involved, or
  whether it predates them.
- [ ] **C3.2 (F4)** Make fault clearing report success only once quorum-risk
  accounting sees the node healthy, or have the safety check derive from the
  same view. Product fix, plus a test that injects straight after a clear.
- [ ] **C3.3 (F6)** Put the force-kill confirmation deadline in the agent config.
  Under load, 2 s is too short for `runc kill` plus the exit observation. Pick
  a production default and test it with a slow fake runtime.
- [ ] **C3.4 (F2)** Wait until every surviving member has applied the new
  membership before killing the leader. Then check whether real leader
  replacement after decommission has the same gap.
- [ ] **C3.5 (F3)** Retry the retryable 503 in the placement cleanup helper.
- [ ] **C3.6 (F5)** Replace the fixed 20 s budget with an event-driven wait
  (poll job state with a generous overall deadline), and put `job_recovery` in
  the `process-heavy` group.
- [ ] **C3.7 (F7)** Seed the gossip RNG in the five-node convergence test, or
  loop until convergence with a round cap.
- [ ] **C3.8 Flake register.** Add a short "Known flakes" table to
  `docs/progress.md` (test, first seen, cause, owner item), maintained by the
  rerun-once rule above.

### Phase 4: tidy the layout (smaller builds, less duplication)

- [ ] **C4.1 Merge the small integration binaries.**
  - Move the 47 small `tests/*.rs` files (1–18 tests each, no gating) into one
    `tests/suite/` binary with a module per file.
  - Keep the gated, heavy binaries separate: ebpf, oci_crash, owned_*, cluster,
    upgrade, `documentation_first_run`.
  - Update `.config/nextest.toml` and the Makefile filters from `binary(x)` to
    `test(/^x::/)` where needed.
  - Watch the re-exec fixtures (`runtime_selection`, `process_recovery`).
  - Expected: 1.5–3 min less per test build, and caches small enough to fit
    easily. See D4.
- [ ] **C4.2 Shared cluster test support.** One `tests/support/cluster.rs` for
  `start_node`, `local`, `wait_until` and `wait_for_leader`, used by placement,
  cluster_failover, cluster_gossip, chaos and reconstruction.
- [ ] **C4.3 Delete overlaps.**
  - `tests/chaos.rs`'s council-partition test, which the unit test covers
    better. Keep the rest of chaos.rs in the portable suite if it's in-memory,
    as its header says.
  - The two `reconstruction.rs` tests that mirror unit tests.
  - The small name-level duplicates in `batch`, `integration` and `onion`,
    after confirming they add nothing.
- [ ] **C4.4 Split `documentation_first_run`.** Keep the two first-run paths.
  Move the registry crash, cron SIGKILL and node-job lease tests into
  `registry_authority`, `job_recovery` or a gated acceptance binary, so the
  portable suite loses about 150 seconds of serialised work.
- [ ] **C4.5 Stop waiting out timeouts.**
  - For the 19 happy-path tests that sleep a full 5/10/20/30 s, prove absence
    with `tokio::time::pause` where the code allows it, or shorten the bound
    through an injectable deadline.
  - Target: portable suite wall time down from about 300 s to under 200 s.
    That saving repeats on every job that runs it.

### Phase 5: every test runs somewhere

- [ ] **C5.1** Fix the `test-linux` filter so
  `snapshot_restore_recovers_corrupted_data` runs: rename it `btrfs_…`, or
  select `RELIABURGER_BTRFS_TESTS` tests by binary and module.
- [ ] **C5.2** Make `registry_routable_push` `#[ignore]` with a reason, so it
  stops reporting a pass. Then either give it a target (it needs a routable
  address; the privileged job may qualify) or delete it.
- [ ] **C5.3** Add the missing Apple test to `make test-apple`.
- [ ] **C5.4** List the manual-only tests (GPU, S3, host reboot, Apple) and their
  targets in `docs/README.md`'s testing section, so "not in CI" is a written
  decision.

## Decisions

All five accepted as recommended on 23 September 2026. For D5, stacked PRs
(bases other than `main`) run the light path. The heavy jobs run on PRs into
`main`, on pushes to `main`, and on any PR labelled `full-ci`.


- **D1. Coverage inside portable Linux.** *Recommended:* one instrumented run is
  both the test gate and the coverage gate. *Alternative:* keep a separate
  uninstrumented run and move coverage to `main` and nightly.
- **D2. Benchmarks off PRs.** *Recommended:* `main` + nightly + PRs touching
  gossip. They have no baseline or threshold, so today they only catch
  panics. Add a regression threshold later if we care about performance
  drift.
- **D3. `build.yml` on PRs.** *Recommended:* Linux x86-64 only, path-filtered.
  *Alternative:* keep all four targets on PRs into `main` only, not `codex/**`.
- **D4. How far to consolidate `tests/`.** *Recommended:* merge the 47 small
  files (C4.1). *Alternative:* merge everything, which is faster to build but
  loses per-binary nextest groups and makes the gated suites awkward.
- **D5. How we push.** 53% of minutes were cancelled runs. Stacked review PRs
  could run the light path (Phase 1–2 jobs minus acceptance) and save the heavy
  jobs for PRs into `main`. This is also a working habit: run `make ci` locally
  before pushing, and push in batches.

## Expected result

| | Now | After phase 1 | After phase 2 |
|---|---|---|---|
| Runner-minutes per push | about 212 (+35–120 `build.yml`) | about 110 | about 90 |
| Critical path | 30–37 min | about 20–23 min | about 15–18 min |
| First failure signal | 7–16 min | 7–12 min | 3–5 min (lint), about 12 (tests) |
| Portable suite runs per push | 7 | 2 (Linux instrumented, macOS) | 2 |
| Test-profile builds per push | about 12 | about 7 | about 3 |

Phase 3 is about trust rather than speed: after it, a red CI means a bug.

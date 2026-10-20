# Plan: faster CI, stronger tests and release qualification

Created 11 October 2026; **reviewed and refreshed 14 October 2026** against
[PR #821](https://github.com/reliaburger/reliaburger/pull/821), continuing the
review of the [0.2.0 release train, PR #725](https://github.com/reliaburger/reliaburger/pull/725).
**Refreshed again on 18 October 2026** against `release-0-2-2` at
[`44dd922f`](https://github.com/reliaburger/reliaburger/tree/44dd922f):
Q2 is done in [#1050](https://github.com/reliaburger/reliaburger/issues/1050)
with nextest's own build reuse, replacing the closed
[#933](https://github.com/reliaburger/reliaburger/pull/933). The
[18 October status](#status-at-18-october-2026) says which items are done,
which are tracked by an issue, and which are proposed but not yet filed.
The objective is faster useful feedback and fewer repeated qualification runs,
within free standard GitHub Actions capacity and existing maintainer machines.

This plan covers CI scheduling and compilation, what the tests prove, tests
that can be consolidated or replaced, coordination between development
sessions, and release-candidate soaks. It separates current behaviour from
remaining proposals. Repository merge-queue settings have not been verified;
workflow support alone does not establish that integration is protected.

The 14 October work leaves the running 0.2.0 soak alone: no new VMs, Rust
builds or product processes are needed for this review or the CI evidence
helpers. Runtime validation of later test changes belongs in CI or a separate
qualification window, not beside that soak.

## Evidence and limits of the estimates

The current source baseline is PR #821 at
[`1952483b1f73a8ef3a63e3b7a301994d12cd008c`](https://github.com/reliaburger/reliaburger/tree/1952483b1f73a8ef3a63e3b7a301994d12cd008c),
compared with the original 11 October review at
[`eb65764a4d3318168edc1834df44fc558d485ba0`](https://github.com/reliaburger/reliaburger/tree/eb65764a4d3318168edc1834df44fc558d485ba0).
Q2's first implementation, PR #933, was closed in favour of #1050 (see Q2).
The train is moving: recheck each finding against its implementation base and
open fixes before assigning work. This document does not claim these changes
have reached `main`.

The review read the [testing guide](../testing.md),
[harness design](../design/test-harness.md), [release runbook](../releasing.md),
critical test bodies, workflows, soak drivers and checkers, and completed CI
reports. It did not run a new qualification campaign. It builds on the
[September CI plan](2026-09-23-ci-feedback-loop.md); shared archives, the
single portable integration binary, and removal of repeated no-default test
runs have already shipped and are not new tasks here.

Current timing observations from completed runs:

| Observation | Evidence and implication |
|---|---|
| A full run took about 42 minutes; acceptance extracted the archive eight times, spending 403.52 seconds extracting | [Run 38403580100](https://github.com/reliaburger/reliaburger/actions/runs/38403580100): one extraction per job would remove about **5 minutes 49 seconds** of acceptance work before accounting for new verification overhead |
| Cluster discovery/execution extracted twice, spending 193.1 seconds extracting | The same run has about **1 minute 43 seconds** of removable cluster extraction |
| Each of three macOS shards performed a full build, taking 27 minutes 38 seconds to 33 minutes 46 seconds, for about two to three minutes of tests per shard | The same run shows compilation is the dominant remaining bottleneck; more test shards do not share that build |
| Removing repeated extraction would move that run's critical path to macOS | The estimated whole-run benefit is only **about two minutes**, despite larger Linux job savings |
| Another complete run took about 37.4 minutes | [Run 38393009631](https://github.com/reliaburger/reliaburger/actions/runs/38393009631) has an estimated **3.7-minute** whole-run opportunity from extraction reuse; this is a counterfactual from logs, not a measured implementation result |

Historical evidence from the 11 October review remains useful for integration
and queueing, but does not replace the current timings:

| Observation | Evidence and implication |
|---|---|
| Four sampled successful train runs took about 37–47 minutes, median about 40 | Historical active-run baseline |
| Two other sampled PR runs took about 80 minutes, roughly 41 minutes outside the active critical path | Queueing and dependency waits need separate measurement; [run 38118828700](https://github.com/reliaburger/reliaburger/actions/runs/38118828700) |
| Acceptance extracted the approximately 2.7 GB archive eight times, taking about 12 minutes 16 seconds | [Run 38100761710](https://github.com/reliaburger/reliaburger/actions/runs/38100761710) identified the repetition; its older extraction speed must not be used as the expected saving now |
| #731 had four observed base-update CI rounds lasting about 43–50 minutes | Ordered integration could avoid some invalidated runs; it cannot eliminate validation of new combined code |
| #731 and #754 passed separately, merged 88 seconds apart, and failed together on a missing Linux-only struct field | Combined validation matters: [failure](https://github.com/reliaburger/reliaburger/actions/runs/38115026681/job/114398221129) |
| #764 and #768 made the same one-line repair; #768's CI continued about 19 minutes after closure | Repair ownership and cancellation save capacity: [duplicate record](https://github.com/reliaburger/reliaburger/pull/768#issuecomment-6106383543) |

Individual test timings below come from historical run 38100761710, not the
current `1952483b` source baseline. A saved test-second is not necessarily a saved
wall-clock second: tests run concurrently and the critical path can move.
Estimated gains must be measured after implementation, not added together.

Initial targets, subject to measurement:

- Verify Q2 removes repeated extraction and reduces full-run time by the
  measured critical-path opportunity, currently about **two to four minutes**
  in the two recent samples. Do not promise a ten-minute whole-run saving.
- Treat **30–35 minutes** for comparable, warm, unqueued full integration runs
  as a later engineering target that also needs compilation improvements,
  rather than an expected result of extraction reuse alone.
- Reduce full runs invalidated by integration order and runner-minutes per
  merged change; report queue delay separately.
- Aim eventually for a **5–10 minute warm development feedback lane** while
  retaining comprehensive validation of the queued merge candidate. A lighter
  train-PR policy already exists; achieving this duration still needs
  measurement and structural work.
- Improve the claims established by tests and soaks without lowering the
  coverage floor, adding retries, or accepting stale evidence.

## Priorities and sequencing

P0 means establish first, or complete before relying on the affected release
claim. P1 means high-value implementation next. P2 means follow after those
foundations and measurements. These priorities do not schedule changes into
the active release train automatically. Effort is relative scope, not a time
commitment.

| ID | Priority | Task | Scope | Depends on |
|---|---|---|---|---|
| Q1 | P0 | Establish comparable measurements and ownership | Small | Nothing |
| Q2 | P1 | Extract and verify each archive once per consumer job | Small/medium | Q1 |
| Q3 | P1 | Cancel superseded work and protect required-job capacity | Small | Q1 |
| Q4 | P1 | Consolidate the remaining duplicate state fixture | Small | Q1 |
| Q5 | P1 | Consolidate successful upgrade scenarios | Small/medium | Q1 |
| B1 | P1 | Verify ordered native merge-queue enforcement | Medium; workflow and repository settings | Q1; Q2/Q3 recommended first |
| B2 | P1 | Replace elapsed-time fixtures and cluster-only matrices | Medium | Q1 |
| B3 | P0 | Repair tests whose asserted behaviour differs from their claim | Medium; independent fixes | Revalidate each finding |
| B4 | P1 | Cover late publication and generated lifecycle interactions | Medium | B3 inventory |
| B5 | P2 | Replace shared append-only records with deterministic fragments | Medium | Q1 ownership |
| B6 | P2 | Reduce compilation cost and verify the feedback policy | Large, incremental | Q1, B1 before further cadence changes |
| S1 | P0 | Make RC acceptance require executed coverage | Medium | Revalidate current soak fixes |
| S2 | P1 | Establish accumulation exposure and workload/fault coverage | Medium | S1 |
| S3 | P2 | Rationalise stress loops and assess a combined final campaign | Medium | S1/S2; equivalence evidence |

Quick wins are independent small changes with limited design work. B1 is
high priority despite being in the larger-change group. B3 and S1 protect
confidence and can proceed alongside speed work; they should not wait for a
crate refactor.

Status at the 14 October review:

| Item | Current status |
|---|---|
| Q1 | Current runs remeasured above; comparable ongoing reporting and ownership remain open |
| Q2 | Initial implementation in #933 (since closed); hosted equivalence and timing validation pending |
| Q3 | Partial: newer train pushes and PR updates supersede older runs; closed-PR cancellation and capacity outcomes still need verification |
| Q4 | Partial/obsolete: legacy mock job-stop tests were removed with their execution path; duplicate 65,536-chunk setup remains |
| Q5 | Open: both proposed consolidations remain; cluster fixtures now use three voters, so historical per-test savings need remeasurement |
| B1 | Partial: CI handles `merge_group` and checks the queue SHA; repository queue/rule settings and combined-green behaviour are unverified |
| B2 | Partial: manual cron time now drives the capacity test through the production scheduler, while the leadership test retains real UTC coverage; the other fixture tasks remain |
| B3 | Partial/obsolete: legacy callback routes and their misleading test were removed; settlement, TLS and actual-worker findings remain |
| B4/B5 | Open |
| B6 | Partial: ordinary PRs into a release train omit macOS and benchmarks; heavy owners need `full-ci`, which still omits macOS there. Full non-PR integration remains required; compile sharing/refactoring is open |
| S1/S2/S3 | Open; final job qualification is now 12 hours by maintainer decision, not 24 |

### Status at 18 October 2026

Checked against `release-0-2-2` at `44dd922f`, the repository's rulesets and
every issue and PR. **Done** means the code already does it; **tracked** names
the issue that owns it; **proposed** means it's still worth doing and has no
issue yet, so the maintainer can file it; **deferred** means not worth an
issue until new evidence appears.

| Item | Status | Evidence |
|---|---|---|
| Q1 timing report | Deferred | #1050's PR records the before/after timing for acceptance and cluster on the same runner class. A standing report isn't worth its own issue yet |
| Q2 one extraction per consumer job | **Done**, [#1050](https://github.com/reliaburger/reliaburger/issues/1050) | See Q2 below |
| Q3 cancel superseded work | Partly done; **proposed** for closed PRs | `ci.yml` cancels older runs on PR updates and train pushes, but its `pull_request` types have no `closed`, so a closed PR's run keeps going (the #768 case). The benchmark gate's separate treatment is done (#857, #859, #865) |
| Q4 duplicate 65,536-chunk fixture | **Proposed** | `src/meat/task_array_state.rs` still calls `run_one_task_chunks_on_ten_nodes` from both tests |
| Q5 consolidate upgrade walks | **Done** ([#1055](https://github.com/reliaburger/reliaburger/issues/1055)) | Retention starts on v0.4 and makes one real upgrade; the rolling walk submits through a follower and the separate forwarding walk is retired |
| B1 merge queue | Partly done; **proposed, urgent** | `ci.yml` handles `merge_group` and binds evidence to its head SHA. But the only queue ruleset targets `refs/heads/release-0-2-1` by name, so `release-0-2-2` has no rules at all: its PRs merge after the light checks alone, though [releasing.md](../releasing.md) and [testing.md](../testing.md) say every train has a queue |
| B2 manual-clock cron capacity | **Done** | `tests/cluster_task_arrays.rs` uses `CronClock::manual`; the leadership test keeps the real UTC clock (#748, #868) |
| B2 placement sleeps | **Proposed** | `tests/placement.rs` still has fixed 3 s and 6 s settle sleeps |
| B2 upgrade rejection matrices and proxy/Council deadlines | **Proposed**, as one issue | Each rejection case still starts a real `ClusterHarness`; the proxy inactivity tests use production timeouts |
| B3 legacy callback routes | **Done** | `legacy_batch_routes_are_gone` in `src/bun/api/tests.rs` |
| B3 settlement helpers | **Done**, [#1058](https://github.com/reliaburger/reliaburger/issues/1058) | The cancel-request tests in `src/bun/api/tests.rs` are named as cancellation tests; new tests stall the leader's real `JobApplyCommit` and failed-hook `DeployCancel` writes and fail when either deadline is removed |
| B3 stalled TLS reclamation | **Done**, [#1057](https://github.com/reliaburger/reliaburger/issues/1057) | `stalled_tls_handshakes_are_bounded_and_reclaimed` in `src/wrapper/proxy.rs` now proves a real `rustls` client is refused while the slots are full, that the server closes the stalled sockets, and that a real handshake and request then succeed; it fails with the bound or the deadline removed |
| B3 real worker in the rolling walk | **Done** ([#1056](https://github.com/reliaburger/reliaburger/issues/1056)) | The walk starts four nodes, reads the three voters and the one worker from Raft membership, sends swapped labels the leader must correct, and asserts worker, council, then leader |
| B4 late publication | **Done** ([#1059](https://github.com/reliaburger/reliaburger/issues/1059)) | `src/bun/api/late_publication_tests.rs` holds the standalone writer past its caller's deadline, checks admission and dispatch stay fenced, then reopens and retries the same `Idempotency-Key` |
| B4 broader lifecycle property test | **Done** ([#1060](https://github.com/reliaburger/reliaburger/issues/1060)) | `generated_lifecycles_keep_identity_conservation_and_fences` in `src/meat/task_array_store.rs` drives repeated and changed requests, grants, stale completions, worker loss, unknown outcomes, replay acknowledgements, cancellation, cron fires, pruning and reopen through `TaskArrays::apply`. It found that a stop after a requeue kept attempts on retired chunks, so the snapshot refused to reopen |
| B5 per-record fragments | Deferred | Only worth doing if conflicts in the shared manifest, bindings and flakes files are measured as frequent |
| B6 repeated macOS compilation | **Done** (decided) | [#846](https://github.com/reliaburger/reliaburger/issues/846) and PR #903 removed the double compile; a shared macOS archive was measured and rejected, because the build stays on the critical path |
| B6 small-crate pilot | Deferred | Speculative; the single crate is deliberate |
| B6 explicit build targets | **Done** as far as it goes | [#904](https://github.com/reliaburger/reliaburger/issues/904) fixed the Make recipes' double compiles |
| B6 light/full split against branch rules | Folded into B1 | A light-green PR on `release-0-2-2` merges without a queue today |
| S1 executed coverage in RC verdicts | **Done** for the final verdict ([#1061](https://github.com/reliaburger/reliaburger/issues/1061)) | The record's execution inventory counts completed walks, faults, specials, pulses, exports, the baseline, the final drain and the verified candidate bytes against the 8-hour schedule; a final refuses to start without a soak build, a digest or its own bootstrap; `soak.yml` passes `--tier custom` as a diagnostic |
| S2 accumulation exposure | Partly done; **proposed** | Per-runtime accepted successes are reported. Fault and node choices come from a logged, repeatable seed, observed fault effects are recorded, and each leak checker's armed minutes per node are reported ([#1062](https://github.com/reliaburger/reliaburger/issues/1062)). Failing a final on short exposure waits for a schedule decision: the RSS baseline still resets on a pid or image change, and no Bun lived long enough to arm `leak-rss` on one node of the 0.2.0 final |
| S3 one final campaign | **Done** (maintainer decision) | [#991](https://github.com/reliaburger/reliaburger/issues/991) and PR #996: one eight-hour job final replaces the separate V02 final. The loop-lane bounds are [#287](https://github.com/reliaburger/reliaburger/issues/287) |

Earlier-attempt evidence is also supported now: a failed-job rerun may retain
successful jobs and an archive from an earlier attempt of the **same commit
and workflow run**. This is not reuse across commits or unrelated runs.

## Conditions applying to every task

- Preserve zero retries, the existing line-coverage floor, required owners,
  platform prerequisites and distinct real-runtime/crash boundaries.
- Retain exact commit, run, attempt, host, selection, manifest and executable
  provenance. Missing, skipped, unfinished, stale or failed evidence must not
  qualify a gate. The current protocol permits a successful earlier attempt
  of the same commit and run during failed-job reruns, with the original
  attempt preserved in its seals and builder provenance. It rejects another
  commit/run, future or malformed attempts, and inconsistent owners within
  one job. Do not widen that exception into arbitrary evidence reuse.
- Before deleting or moving a test, record its claim, failure condition,
  decisive assertion, replacement and retained real-system boundary. Show
  the replacement fails with the original defect or a focused disabled
  safeguard. Source presence or a passing renamed test is insufficient.
- Reconcile finite contracts, ignored bindings, owner selections and generated
  documentation in the same change as any test identity/cadence change.
  Do not silently narrow a gate by changing a filter.
- Keep implementation explanations in the relevant existing book chapter and
  update testing/release instructions with the corresponding change. Related
  editorial edits can be assembled once per integration batch; they still
  merge with its code.
- Use standard hosted runners and existing machines. No paid runners or
  additional services are assumed. Preserve hardware/manual qualification
  where hosted runners cannot supply the required environment.

## Quick wins

### Q1 — Establish measurements and coordinate work

**Tasks**

- Produce a compact timing report from existing Actions metadata, step logs
  and JUnit. Separate queue/dependency delay, compile, download/extraction,
  execution, total wall time, runner-minutes and cancellation waste.
- Record runner class, commit, selected gates and warm/cold cache state. Use
  subsequent ordinary runs for comparison rather than launching a benchmark
  campaign solely to collect timings.
- Assign one integration owner for a busy subsystem and one for shared test
  infrastructure/contracts. Claim repair issues before opening overlapping
  fixes. Coordinate shared chapters at the end of each related batch.

**Expected gain:** visibility into the actual bottleneck; fewer duplicate
repairs and invalidated runs. No direct duration reduction is claimed.

**Acceptance criteria**

- [ ] A baseline links at least five comparable completed runs where available,
  with any smaller sample explicitly labelled; subsequent reports use the
  same definitions and separate cold runs.
- [ ] Reports include median and range; only report tail percentiles once the
  sample supports them. Cancelled/failed runs remain visible as cost.
- [ ] Every active integration repair has an explicit owner; related changes
  identify their integration order and shared-file coordinator.
- [ ] No conclusion attributes all improvement to test execution or claims
  that two development sessions necessarily double throughput.

### Q2 — Extract the archive once per consumer job

**Status:** done in [#1050](https://github.com/reliaburger/reliaburger/issues/1050),
using nextest's own build reuse. #933's first design was dropped; see below.

**Scope:** `scripts/ci/workflow_adapter.py`, `workflow_assembly.py`,
`make_owner.py` and the `cluster` and `acceptance` jobs in
`.github/workflows/ci.yml`.

**What #1050 does:**

1. One step per consumer job, `workflow_adapter.py extract`, checks
   `tests.tar.zst` against the build job's `archive_sha256` output (one hash of
   the archive per job, where each owner used to hash it again), then runs
   `cargo nextest list --archive-file tests.tar.zst --extract-to .
   --extract-overwrite --workspace-remap . --list-type binaries-only`. Listing
   only binaries extracts without starting any test binary.
2. It writes `target/archive-extraction.json`: the job's context, the archive
   and origin-receipt digests, the extraction command and the digests of
   `target/nextest/binaries-metadata.json` and `cargo-metadata.json`. The
   receipt's digest is the step output `archive_extraction_sha256`.
3. Every owner in the job takes that output as `--archive-extraction`, loads
   the receipt by it, and runs its Make recipe with `--binaries-metadata`,
   `--cargo-metadata`, `--target-dir-remap target --workspace-remap .` instead
   of `--archive-file`. Its child checks the two metadata digests before
   discovery, as it checks every other owner input.
4. The sealed owner plan carries the receipt. `aggregate` checks that an
   archived owner's selectors reuse the extracted build and never open the
   archive, that its metadata inputs match the receipt, and that the receipt
   names the builder's archive (from the trusted `build-tests` output) and the
   owner's own nextest.

**Why #933's design was dropped:** #933 treated the extracted tree as untrusted.
It inventoried and hashed every extracted file into a receipt, and each owner
rehashed the lot before discovery, between discovery and execution, and
afterwards: three passes over multi-GB test binaries per owner, unmeasured,
in 1,363 lines. On hosted CI (run 39021673911) both consumer jobs stopped at
the new step with `invalid immutable archive payload path`, then each owner
with `trusted archive extraction digest required`. Its path whitelist allowed
only the two metadata files under `target/nextest`, and a real archive also
extracts `target/nextest/libdirs` (the Rust standard library the binaries link
against). The second error followed from the first: no receipt, no digest.

The extracted tree lives inside one job on one runner, and nothing outside that
job can write to it, so it needs no new trust boundary. Binding the evidence to
the archive needs only the archive's digest, checked once, and the digests of
the metadata that tells nextest which binaries to run. Unlike #933, #1050 is
built on the train's current `ci.yml`, so it keeps #977's download timeouts
and retries and #1018's build matrix.

**Validation:** `scripts/ci/test_archive_extraction.py`,
`test_workflow_assembly.py`, `test_make_owner.py` and `test_ci_workflow.py`
cover the workflow structure, the owners' reuse selectors and the evidence
binding, with their detection controls. A local smoke run with real nextest
extracted a small crate's archive through `extract` and ran `list` and `run`
with the reuse selectors from a different workspace path. The hosted timing is
in #1050's PR.

**Expected gain:** in run 38403580100, about **5 minutes 49 seconds** less
acceptance extraction and **1 minute 43 seconds** less cluster extraction,
before verification overhead. These jobs overlap. The whole-run opportunity
is about **two minutes**, limited by macOS; the other recent run suggests
**3.7 minutes**. These estimates are not additive and are not post-change
measurements.

**Acceptance criteria**

- [x] Logs show one extraction per consumer job and unchanged required gate
  selections, runtime prerequisites and exact discovered identities.
- [x] Discovery and execution use the same extracted build and remapped
  workspace; no hidden compile replaces the saved extraction.
- [x] Mismatched builder/archive provenance, changed metadata and a failed
  owner are rejected by the existing evidence chain. Individual binaries are
  not rehashed: they never leave the job (see above).
- [ ] Before/after timings show extraction and full critical-path changes;
  shrinking or splitting the archive is a separate measured follow-up. The
  numbers are in #1050's PR.

### Q3 — Stop superseded work and protect required-job capacity

**Status:** partial. Current concurrency cancels older runs on replacement PR
updates and newer release-train pushes. Train-PR selection already omits
macOS and benchmarks. These are existing mechanisms, not new proposals.

**Remaining tasks:** verify closed-PR cancellation, inventory enforced versus
informational jobs, and measure busy-period capacity. Keep cancellation scoped
so it does not terminate another PR, a current queue candidate or an RC
campaign. Verify queue-event cancellation with B1 and preserve successful
same-run jobs on partial reruns.

**Expected gain:** reduced queue delay and wasted runner-minutes. The observed
duplicate consumed about 19 additional minutes after closure; this is an
example of avoidable work, not a per-PR saving.

**Acceptance criteria**

- [ ] A superseded/closed PR's active work ends within a five-minute target,
  while unrelated and current-candidate work continues.
- [ ] Scheduling changes preserve enforced checks. In particular, the
  task-array benchmark's existing 2× regression gate is not treated as the
  reporting-only gossip benchmark.
- [ ] Comparable busy-period runs show queue delay and total consumption;
  additional shards are not accepted merely because one isolated run is faster.

### Q4 — Consolidate the remaining duplicate scenario setup

**Status:** partly obsolete. `src/bun/job_lifecycle_tests.rs` and the legacy
job execution path were removed after the original review. Retire the two
mock-stop timeout tasks; do not reintroduce the path to perform this cleanup.

**Remaining scope:** `src/meat/task_array_state.rs`.

**Task:** run the shared 65,536-chunk scenario once and retain both its
size-bound and post-deserialisation grant assertions. The current tests
`accepted_state_stays_bounded_with_one_task_chunks_on_ten_nodes` and
`compaction_keeps_accepted_grant_answers_correct` still repeat the helper.

**Expected gain:** one repeated large fixture, measured as test CPU/time.
The old estimate for two ten-second mock waits no longer applies. No separate
compile or whole-run gain is promised.

**Acceptance criteria**

- [ ] The consolidated test checks peak size, terminal status and valid state,
  then checks retained attempts/owners after serialisation and reopening.
- [ ] Both former claims remain represented in the contract inventory; unrelated
  current-runtime stop/ownership regressions remain.
- [ ] Before/after test time is recorded without changing production defaults.

### Q5 — Consolidate repeated successful upgrades

**Scope:** `tests/self_upgrade.rs` and `tests/self_upgrade_cluster.rs`.

**Status:** done ([#1055](https://github.com/reliaburger/reliaburger/issues/1055)).
The retention test starts on v0.4 with v0.1–v0.3 and their `.sig` envelopes
installed, makes one real upgrade to v0.5 and checks GC the moment the node
reports settled; it took about 40 s on a Mac (137.7 s for four transitions in
#1049's sample), so its 720 s nextest override is gone. The rolling walk
sends its start through a node that reports itself a Raft follower of the
leader, and
`upgrade_start_sent_to_a_node_that_is_not_the_leader_reaches_the_leader` is
retired from the test file, the bindings, the manifest and the matrix.

**Tasks:** start retention testing on v0.4 with v0.1–v0.3 already installed,
then perform one real upgrade to v0.5. Do not seed while still running v0.1:
the previous version is legitimately protected. Submit the main rolling walk
through an observed follower and retire the separate successful forwarding
walk once it is equivalent.

**Expected gain:** eliminate one complete successful cluster walk and three
setup transitions from retention. These cost 86 seconds and 62 seconds
respectively in the historical sample, but the cluster fixture has since
shrunk to three voters. Remeasure current tests; those numbers are not a
current saving forecast.

**Acceptance criteria**

- [x] Retention still exercises real exec, verification, commit and deletion of
  old binaries and signature sidecars; the settled marker cannot precede GC.
- [x] The main rolling test proves follower forwarding and retains ordering,
  version convergence and surviving-leader assertions.
- [x] Pause/resume, rollback, interrupted download and role-correction coverage
  remain. Actual worker topology is handled explicitly in B3.
- [x] Smaller API forwarding/authentication tests and protected-version store
  tests remain; the retired names are reconciled with their required owners.

## Larger changes

### B1 — Validate integration through a native merge queue

**Status:** partial in the reviewed PR. CI already handles `merge_group`,
binds checkout/evidence to its head SHA and requires full owners for non-PR
integration events. Repository queue settings, rules and actual queue
operation have not been verified.

**Remaining tasks**

- Audit the event/check matrix for `pull_request`, `merge_group`, push,
  scheduled and manual runs, including every required build workflow.
  Confirm required contexts, selections, timeouts and cancellation.
- Verify the active `release-0-2-1` branch rules before enabling or changing
  queue settings. Proposed initial build concurrency is one, with only
  non-failing PRs eligible and no queue jumping. Consider `main` after proving
  the release-branch flow.
- Exercise compatible and conflicting candidate pairs. Workflow support alone
  does not establish that every merge passes the full queue candidate.
- Preserve the existing distinction between light train-PR feedback and full
  integration under B6. Avoid permanently duplicating every full run.

**Expected gain:** fewer manual base-update cycles and combined-green
integration failures. Some avoidable rounds cost 43–50 minutes in the sample;
no fixed saving per PR is justified. The queue still builds new combined
commits and does not resolve textual conflicts or batch builds merely because
several PRs can merge together.

**Acceptance criteria**

- [ ] A controlled pair that passes individually but breaks together is stopped
  before integration; a compatible pair merges in order.
- [ ] Every required context completes for both PR and queue events; full
  integration evidence is bound to the queue commit and workflow run, retaining
  each producer's valid current or earlier attempt on a failed-job rerun.
- [ ] Failed groups, removed PRs, conflicts and replacement groups cannot reuse
  stale green evidence or leave checks waiting forever.
- [ ] Explicit policy fixtures cover release PRs, stacked PRs, retargeting,
  executable documentation, queue events and benchmark/build selection.
- [ ] Record runs per merged change and runner-minutes before/after. Document
  how to restore the prior full-validation flow if queue operation fails.

GitHub's native queue is available for public organisation repositories;
there is no need to operate a bors-style service for this proposal.
[GitHub queue documentation](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue)
(checked 11 October 2026).

### B2 — Make expensive fixtures wait for behaviour, not elapsed time

**Status:** partial. The capacity cron test already uses `CronClock::Manual`
and advances it after bulk admission through production
`fire_due_schedules`. The leadership test remains the real UTC boundary.
Record that work as implemented, not another clock-injection task.

Deliver the remaining changes separately, using the actual production paths:

| Task | Expected gain | Acceptance criteria |
|---|---|---|
| Verify and retain the implemented manual-clock capacity fixture in `tests/cluster_task_arrays.rs` | Capacity test no longer waits for a UTC minute; current net saving is unmeasured | Preserve normal registration, production firing, Raft writes and reserved-capacity assertions. Retain the leadership/identity test as real UTC coverage; do not inject `JobFire` as a substitute |
| Expose readiness/completed reconciliation observations for `tests/placement.rs` | Remove recurring 3/6/15-second sleeps; net gain depends on actual work | Capacity/membership is observed before submission; no-churn/no-resurrection assertions span several completed real cycles, rather than checking immediately |
| Move declarative upgrade rejection matrices to an actual-router fixture with in-memory Raft and small probe servers | Replace several 23–58-second cluster starts | Each rejection verifies HTTP response, node-probe handling and no committed upgrade; retain the real candidate-executable incompatibility boundary |
| Parameterise proxy/Council deadlines for complete small tests and keep production-duration smoke | Reduce repetition within roughly 128 proxy test-seconds per platform and ten-second hung-write cases | Preserve fresh/pooled clients, active streams and stalled headers/body; observe write entry before advancing time. Do not shorten product defaults or assume paused time controls real I/O |

For each row, identify the retained real boundary and update contract/gate
ownership before retiring the old selection. Report executed test time and
full-job time separately. Keep production-duration checks in the full slow
gate initially; moving them to a different cadence requires B6's policy work.

### B3 — Repair misleading coverage claims

These are findings about test evidence, not confirmed defects in the product.
Recheck them against active fixes before implementation. Split by subsystem
so an assertion repair does not wait for a broad harness rewrite.

| Task and reviewed location | Required replacement proof |
|---|---|
| Retired: the misleading callback test and legacy batch endpoints were removed | Keep `legacy_batch_routes_are_gone` in `src/bun/api/tests.rs`. Do not preserve or restore the deleted route. Any new dropped-response test must target current task-array synchronisation and prove recovery without duplicate effects |
| `src/bun/api/tests.rs`: hung commit/failed-hook settlement helpers actually send cancellation requests | Rename and retain useful cancellation cases; enter and stall the actual commit and failed-hook settlement operations, then verify bounded unavailability, exact durable intent and the app gate |
| `src/wrapper/proxy.rs`: stalled TLS reclamation checks only a five-byte TCP write | Prove slots were occupied, let their deadlines expire, then complete a real TLS handshake and request. Disabling reclamation must make the test fail |
| `tests/self_upgrade_cluster.rs`: the workers/council/leader test now uses three nodes, all real Raft voters | Add a verified worker through real eligibility/membership control; assert worker-before-council ordering. Four nodes now allow three voters plus a worker, but assert actual membership rather than assuming its identity. Preserve the deliberate incorrect-label/authoritative-role correction regression; relabelling a voter is not a worker fixture |

**Expected gain:** stronger behavioural coverage and more truthful diagnosis.
No speed saving is assumed, although replacement fixtures should use the
smallest layer that can prove the boundary.

**Acceptance criteria**

- [ ] Each scenario observes the intended fault before checking recovery.
- [ ] Each test fails with a focused defect/control that defeats its named
  guarantee, then passes with the implementation restored.
- [ ] Names, comments, contract claims and owner assignments describe what
  actually ran. Removed product paths have their obsolete test tasks retired;
  current recovery boundaries are not discarded merely for resembling old tests.

### B4 — Add missing lifecycle combinations economically

**Status:** open. Existing storage-failure tests prove synchronous publication
failure; they do not cover a blocking writer publishing after its caller
expires. The current generated chunk model covers plan, complete, lose and
cancel, not the broader lifecycle combinations below.

**Tasks**

- Add a standalone late-publication test around the actual blocking writer:
  pause before publication, expire the caller, refuse subsequent admission
  and dispatch, release the write, reopen storage, and resubmit the same ID.
- Extend property tests over actual common-lifecycle state transitions:
  repeated/changed requests, grants, stale/current completions, worker loss,
  unknown outcomes, replay acknowledgement, cancellation, cron firing,
  pruning and serialise/reopen. Keep the generated model small enough for
  ordinary CI and retain failing seeds/minimised sequences.

**Expected gain:** cover interactions without adding one multi-node end-to-end
test per combination. This adds coverage; any net speed benefit is unmeasured.

**Acceptance criteria**

- [ ] Late publication yields one durable run identity and conserved outcomes;
  no work executes through an uncertain-publication fence.
- [ ] Generated cases establish identity consistency, atomic refusal,
  conservation, stale-result rejection, unknown-outcome fencing and no app
  publication before successful required hooks, as applicable to each model.
- [ ] Tests exercise production transition code, not a second implementation
  with no conformance check; failed sequences are reproducible.

### B5 — Generate shared metadata from independent records

**Scope:** flake records, `tests/contracts/manifest.json`,
`ignored-bindings.json`, their validators and generated documentation.

**Tasks:** design one authoritative fragment per independent record and a
deterministic assembler. Preserve global uniqueness, exact binary/test/gate
assignments, ignored identities and source fingerprints. Generate the
canonical manifest/reports once during integration; decide explicitly which
outputs are committed. Having every PR regenerate the same shared output
would retain much of the current conflict problem.

**Expected gain:** fewer conflicts and base-update cycles in shared records;
no direct test-duration saving. This is a schema/generator change, not JSON
concatenation. It will not resolve conflicting edits to actual product code.

**Acceptance criteria**

- [ ] Independent additions merge without editing one common insertion point.
- [ ] Assembly is deterministic regardless of filesystem/input order; duplicate
  IDs, missing bindings and conflicting assignments fail clearly.
- [ ] Migration preserves the complete reviewed inventory, provenance and
  owner schedule, and seals evidence against the canonical manifest hash.
- [ ] Generated reports cannot drift from the inventory used for qualification;
  book and operator documentation still accompany their implementation batch.

### B6 — Reduce compilation cost and verify feedback cadence

**Status:** partial. The current train-PR policy already skips macOS and
benchmarks; `full-ci` adds heavy owners but still leaves macOS out of those
PRs. Pushes, queue groups and full integration retain the complete obligations.
Repository enforcement of the queued merge candidate remains B1's open check.

**Remaining tasks**

- Measure Cargo build/link contributions on macOS and Linux, dependency
  invalidation, archive contents and cache size. Current macOS shards each
  repeat a 27–34-minute full build for two to three minutes of tests.
  Evaluate one verified macOS build/archive shared by its consumers before
  adding more shards; compare runner-minutes, download/extraction overhead
  and total wall time. Preserve the macOS lint owner and shard evidence.
  Existing caches already hit; do not repeat the September cache diagnosis
  without new evidence.
- Prototype extracting one cohesive production parser/state-machine module
  into a small crate. The independent cron contract crate is a useful example
  of testing actual production source without the full product dependency
  graph. Do not copy business logic into a test-only substitute.
- Use explicit build targets for selected layers. Preserve distinct platform,
  coverage-instrumented and optimised builds where they prove different things.
- Verify the existing fast train-PR/full integration split against B1's actual
  branch rules before claiming a fast green PR is safe to merge. Retain full
  portable, privileged, cluster, upgrade and slow obligations on the merge
  candidate. Extend cadence changes only after that evidence exists.
  Evaluate further sharding after Q2 and queue measurements.

**Expected gain:** potentially the largest improvement to the local edit/test
loop and the macOS compile bottleneck; magnitude is unknown until the pilot.
The 5–10-minute warm feedback target is aspirational. This is not a reason to
delay Q2 or the assertion fixes.

**Acceptance criteria**

- [ ] The pilot reports clean and incremental build time and dependency changes;
  it is retained only if the measured benefit justifies the module boundary.
- [ ] Production imports the same implementation that the isolated tests use.
- [ ] A fast green PR cannot merge without successful full queue validation;
  executable documentation, enforced benchmarks and platform checks retain
  explicit owners and triggers.
- [ ] Any shard/layout change reports total runner-minutes, downloads, peak
  concurrency and busy-period queue delay as well as isolated wall time.
- [ ] The testing guide explains which command gives local feedback and which
  evidence qualifies integration; ordinary archive bytes never stand in for
  coverage-instrumented or another platform's execution.

## Release-candidate soaks

*Superseded on 16 October by [#991](https://github.com/reliaburger/reliaburger/issues/991):
one eight-hour job final now replaces the separate V02 final. The paragraph
below is the 14 October text.*

Retain the distinction between staged installation, approximately 90-minute
compressed fault feedback, eight-hour final V02 qualification and **12-hour**
final job qualification. The maintainer changed the job tier from 24 to 12
hours on 12 October; `qualify-jobs-sustained.sh` and the release runbook now
record that policy. This is not a saving delivered by this plan. Its default
aggregate target remains 1,390 accepted successes/second. Duration is part of
accumulation coverage, not merely a number of repetitions to compress.

### S1 — Require executed coverage for a passing RC verdict

**Scope:** `qualify-sustained.sh`, `qualify-jobs-sustained.sh`,
`sustained_check.py`, `job_soak.py`, their script tests, the hosted soak
workflow and release runbook.

**Status:** open. Existing job coverage and positive-drain checks remain
valuable; the missing piece is complete tier-specific acceptance evidence.

**Tasks:** define the required observations for each tier and bind them to
verified staged assets/candidate identity, including an adopted `--home`.
Record completed workloads, faults, upgrade walks and special scenarios.
Require those records in acceptance. Give final V02 (eight hours) and final
jobs (12 hours) separate decisions rather than relying on metadata labels or
the V02 duration check. Make hosted custom runs explicitly diagnostic, or
supply the complete prerequisites of a formal tier.

The reviewed final verdict checks a clean result, duration, full schedule and
candidate digest, but lacks a complete mandatory execution inventory. The
driver can skip upgrades without `--soak-bun`; hosted soak currently passes no
tier. These are reasons to tighten evidence, not to discard useful custom runs.

**Expected gain:** confidence that a clean report establishes the release
claim, and earlier detection of incomplete campaigns. No shorter final
duration is proposed in this task.

**Acceptance criteria**

- [ ] Script fixtures fail qualification for a missing/skipped mandatory
  upgrade, fault, workload, special, candidate verification or final drain,
  even when elapsed time and other observations look clean.
- [ ] Required evidence refers to actual completed execution and checked bytes,
  not just configuration labels or a digest string in metadata.
- [ ] Fast, custom, final V02 and final job verdicts are distinct and cannot be
  promoted into one another by changing report labels.
- [ ] Retained evidence supports justified checker-only re-evaluation; missing
  observations cannot be reconstructed, and product changes require a fresh
  candidate and fresh qualification.

### S2 — Measure accumulation exposure and actual fault/workload effects

**Status:** open. Resource checkers still reset on PID/image changes and
return no finding before sufficient warm-up; insufficient exposure does not
itself fail acceptance. The new `idle_memory.py` is a separate diagnostic,
not proof of sustained accumulation exposure, and starts product processes;
do not run it beside the active soak.

**Tasks**

- Require and report uninterrupted eligible process-age windows for RSS and
  descriptor checks. Restarts/exec reset those observations; schedule a quiet
  window long enough to arm each checker and collect comparison samples.
- Record a reproducible random seed, actual node/phase choices, timings and
  observed fault effects. Distinguish disk pressure from observed ENOSPC and
  write throttling from observed persistence delay.
- Report accepted rates by runtime beside the aggregate rate. Retain bulk
  no-op load, but stratify independent-effect audits across important runtime
  and fault combinations and add small non-trivial I/O/CPU canaries.
- Define required combinations explicitly. Current runtime-by-node and
  runtime-by-fault coverage does not prove every three-way combination; do
  not impose a full Cartesian matrix without a risk-based reason.

**Expected gain:** catch accumulation and recovery failures that a busy but
frequently restarted or unevenly loaded campaign can miss. Better diagnosis
should reduce repeated investigations; no quantified time saving is assumed.

**Acceptance criteria**

- [ ] Reports state eligible minutes, checker activation and evaluated samples;
  insufficient exposure fails the relevant final qualification claim.
- [ ] Controlled leak/fault fixtures demonstrate the checkers detect their
  intended failure; seed plus recorded decisions supports reproduction.
- [ ] Every required fault is observed affecting active work and recovering;
  unsupported or ineffective injection fails rather than silently skipping.
- [ ] Existing durable acknowledgement, lost/duplicate outcome, ownership,
  cleanup, turn-budget, certificate and export-integrity checks remain.
- [ ] Per-runtime workload/effect coverage cannot be satisfied by one runtime's
  aggregate throughput. Any new throughput threshold is justified separately;
  recorded diagnostic metrics are not silently turned into flaky gates.

### S3 — Reduce repeated stress work and assess one final campaign

**Status:** open. The existing loop workflow already builds and reuses one
archive per platform; preserve that rather than proposing it as new work.

**Tasks:** separate short cross-platform smoke, targeted long loops for a
specific regression, and periodic broad stress sweeps. Retain verified
archive reuse and qualify frozen staged bytes separately. Evaluate whether
one **12-hour** campaign can meet the complete union of current V02 and
job-qualification requirements. Do not assume the changed duration establishes
that equivalence.

**Expected gain:** avoid broad hours-long stress runs for every small fix.
A combined campaign could remove a separate eight-hour run and repeated
setup, but only if equivalent exposure is demonstrated. It cannot be counted
as a saving yet; additional quiet phases may require more time or different
scheduling.

**Acceptance criteria**

- [ ] Every stress lane has a named purpose, platform scope and trigger;
  targeted loops do not remove final required platform/runtime coverage.
- [ ] Repeated identical cycles are not presented as independent statistical
  evidence or a substitute for elapsed accumulation time.
- [ ] Before replacing separate final runs, a reviewed coverage map and an
  executed campaign prove installation, required upgrades/faults, concurrent
  apps, target load, independent effects, uninterrupted exposure and teardown
  against the exact candidate.
- [ ] Until equivalence is established, the separate required final campaigns
  remain. A product fix invalidates the old candidate's qualification.

## Delivery and completion

1. Keep Q1 measurements current; finish Q2's hosted archive/evidence validation.
   Verify Q3's remaining cancellation cases. Offline checker/report fixtures
   and this status refresh can proceed without disturbing the running soak.
2. Audit B1's repository settings and combined-candidate enforcement against
   the existing event policy. Start the remaining B3 assertions and S1 evidence
   work independently.
3. Deliver Q4's remaining fixture consolidation, Q5 and one remaining B2
   change at a time. Record preserved claims and measure current timings;
   do not claim the removed legacy work or implemented cron seam again.
4. Implement B4 and S2; use measured shared-file conflict pressure to order B5.
   Prioritise B6's repeated macOS compilation after Q2 because it limits the
   whole-run saving. Assess S3 only with the earlier equivalence evidence.

Treat any further cadence change as an explicit coverage decision, not a path
filter optimisation hidden inside another PR. Source-only preparation of
Rust tests does not close their runtime acceptance criteria; validate those
later in CI or an appropriate qualification window.

Each implementation PR should identify the task ID, intended benefit,
validation evidence, remaining limitations, affected contracts/owners and
corresponding testing/book/runbook updates. Complete checklist items only
after their evidence exists. Re-measure using ordinary subsequent runs; do
not repeatedly launch the full suite just to improve the sample.

The plan is successful when comparable runs meet the stated timing targets
or explain the remaining measured bottleneck, busy-period queue waste and
runs per merged change decrease, every retired scenario has a proven
replacement, and RC verdicts establish the required executed coverage and
exposure. Faster green results alone are not completion.

## Constraints and work deliberately deferred

- Keep both development sessions if useful; coordinate their integration and
  shared resources rather than assuming fewer machines means less work.
- Do not start with a VM per agent: a documented local VM was already at
  96 GB/96 GB, with a 39 GB shared target. Bound simultaneous compilation and
  preserve host isolation for privileged tests before adding VMs.
- GitHub Free's standard concurrency is 20 jobs, including at most five macOS
  jobs; hosted jobs are capped at six hours. The eight-/12-hour campaigns
  therefore continue on existing maintainer infrastructure.
  [Actions limits](https://docs.github.com/en/actions/reference/limits)
  (checked 11 October 2026).
- Retain cheap scale tests and distinct real boundaries. A sampled
  10,000-member gossip test took less than a second; fake-runner tests do not
  replace real OCI/cgroup adoption, cleanup or crash recovery.
- Preserve the coverage floor, but assess critical production branches and
  assertion quality separately: inline test bodies contribute to line
  coverage, and exact inventory selection does not prove a useful assertion.
- No blanket nightly-only move, broad test purge, retries, paid runners or
  separate merge-bot service is part of this plan.

# Shared batch capacity reservations

Issue: [#543](https://github.com/reliaburger/reliaburger/issues/543).

Two submissions can currently schedule against the same worker report before
either execution appears there. The ordinary app scheduler can also consume
that capacity, and process-local locks lose their authority on leader handover.

## Design

Persist each assigned batch execution's requested resources with its durable
record. Reconstruct missing app placements and batch reservations alongside
worker report commitments, deduplicating batch reports by exact namespace and
execution identity. Use the scheduler's effective resource defaults.

Guard both batch registration and ordinary app placement commits with the
previous `DesiredState.last_applied_log`. The replicated state machine must
compare against the previous position before advancing it for the new entry.
Refusal changes no admission state. The submitting leader retries with a fresh
snapshot a bounded number of times, then returns 503. Dispatch follows a
successful durable admission only. Audit every production placement writer.

Fresh reports are a leader-side prerequisite for clustered admission. Raft
application must remain deterministic, without replica-local clock decisions.
Standalone operation retains its local capacity path.

Keep unknown dispatch, observation timeout and runtime outcomes nonterminal.
They retain reservations across leader handover and pruning. A missing report
never proves that a delayed dispatch cannot launch. Release only after positive
exit evidence or explicit durable retirement.

## Order and evidence

1. Add permanent raw API tests for missing/stale reports, simultaneous admission
   through independent API processes, unreported app placements, and an expired
   unknown runner. Correct the terminal-outcome test to require an exit code.
2. Run those tests on unchanged production and record their failures.
3. Integrate the durable execution identity from #535, then implement guarded
   admission and shared resource accounting.
4. Add state-machine CAS and handover coverage, plus report deduplication and
   positive-exit release checks.
5. Run offline `make ci` and focused cluster checks with two workers. Publish a
   child PR into the review train; the parent owns merging and compatibility.

The wire and state additions require the next coordinated generations from the
actual integrated train. Refresh before implementation and agree the counter
order with the parent; never claim a parallel child's generation independently.

Lifecycle regressions cover watcher/agent disappearance, exhausted remote dispatch, and completed/failed callbacks without exit evidence. These must retain exact-execution reservations. There is no existing batch cancel endpoint and this issue adds none; preserve current stop/retirement behavior, releasing only for positive exact-execution exit or explicit fenced retirement.

The watcher-disappearance and missing-exit callback regressions use a controlled
HTTP runner which authenticates and acknowledges the actual dispatch, then
returns no terminal status evidence. This isolates the reservation boundary
from ProcessGrill's inability to enforce CPU limits. Dispatch exhaustion keeps
its separate closed-endpoint regression.

The handover regression uses two real Council nodes. The controlled runner
acknowledges the first dispatch, node two becomes the sole voter and leader,
and its fresh empty commitment report must not free the retained reservation.
A positive control reports the exact dispatched execution with observed exit
zero and verifies that the next full-node job can be admitted.

Normally inherited the qualified #535 commit bb1da32d before this baseline,
retaining the prepared capacity tests and all execution-ownership regressions.
Only plan-index/test insertion conflicts occurred; both sides were retained.
Fixture constructors now include the required execution/digest fields and new
Harness defaults. Unknown expiry and dispatch outcomes already stay
nonterminal in #535, so those assertions are controls here; capacity retention
still requires genuine baseline evidence. Stale API admission uses the
aggregator's explicit stale_nodes provenance with a deliberately old sender
timestamp, avoiding sender-clock freshness assumptions.

For later #534 integration, active ordinary apply claims with no durable
physical owner reserve their held nonscheduled job requests conservatively on
every candidate node. Before apps_committed this includes migration jobs;
afterwards positively confirmed migrations can be omitted, while ordinary
tail jobs remain held until the whole claim clears. Double-counting a reported
job may reduce utilization. No term-to-node inference is safe across handover.
The parent will integrate these claim hooks/tests after #543 because #534 has
not entered this branch yet.

The first baseline compile stopped on a test fixture using insert on the
aggregator's Vec stale_nodes (/tmp/runtime-543-baseline.log). No behavioral red
is counted. Changed only fixture insertion to push and retained the attempt.

## Qualification receipts

Inherited actual integrated train ef1a378a (protocol 37/state 53); this change
uses protocol 38/state 54. The whole-pass placement variant is appended to the
wire enum. Existing unguarded placement requests are refused in production.
Each app/batch request uses the same request-or-zero defaults; jobs request no
GPUs because JobSpec has no GPU field. Atomic application compares the previous
entry, while last_applied_log still records the current entry for term guards.

Baseline receipts remain in /private/tmp/runtime-543-*.log. The first two
attempts were Vec fixture compile errors, not behavioural failures. Baseline3
had seven genuine failures and two invalid ProcessGrill resource fixtures;
corrected authenticated remote-runner baseline4 had four genuine failures.
The actual two-Council old-term snapshot control failed with 202 instead of
503 (epoch-baseline). Closed publisher and receive-deadline-between-ticks
controls both failed (freshness-baseline); the latter now manually polls the
paused aggregator future instead of spawning it. Actual Council hang_writes
exceeded the outer seven-second HTTP deadline (write-deadline-baseline), before
the five-second overall admission budget was implemented.

The footprint baseline first had missing fixture imports (not a red), then
ran eight controls: six genuine failures for app/batch partial request and
ordinal mismatch, current-revision retired-node Raft registration, and retired
node raw HTTP admission; two exact-complete reports passed. The delayed service
dispatch baseline initially held a metrics borrow across a Raft write and was
interrupted as a setup hang. After copying the membership log ID before await,
it genuinely failed: an exact first dispatch returned 202 after durable worker
retirement. No namespace existence restriction was introduced: valid app/job
namespace labels remain accepted without a namespace declaration.

Focused3 ran 173 tests: 171 passed and two positive fixtures lacked the new
report provenance. Corrected only the manual daemon report identity and the
allowed permission fixture's current-term report, receive deadline and owned
publisher. All denied permission assertions remain unchanged. Focused4 passed
all 173 tests in 80.683 seconds. Earlier focused1 and focused2 setup failures
are retained; no introduced failure is described as a flake.

Full portable CI and required real cluster qualification follow. Unknown
outcomes remain held across watcher loss, dispatch exhaustion and leadership
handover. Retirement guards prevent NEW assignment/first launch only; already
owned exact retries remain idempotent, with no new physical stop/cancel API.

The first full CI stopped in Clippy before portable tests: large error values
in the extracted helper/test adapter, default-field fixture assignment, two
redundant fixture bindings and a needless borrow. Boxed the two errors and
corrected those fixture expressions without changing admission behaviour.
Receipt: /private/tmp/runtime-543-ci1.log.

CI2 still stopped before portable tests: removing redundant report shadowing
also removed the needed mutability in two fixtures. Corrected those bindings;
Clippy also found the equivalent default assignment and needless borrows in
the portable harness. Receipt: /private/tmp/runtime-543-ci2.log.

CI3 passed both Clippy matrices, then ran all 5,578 portable tests: 5,575 passed
and three older positive fixtures lacked explicit report provenance. The manual
daemon now records its actual original ordinal and 600m request, the stale-node
fixture provides a current received report/deadline for its positive planned
member, and the catalogue-repair fixture supplies the actual Council term to
its owned watch snapshot. Original self-eviction, stale-member and catalogue
repair assertions remain intact; no missing/stale negative is weakened. The
full failed receipt is retained in /private/tmp/runtime-543-ci3.log. These are
introduced fixture errors, not flakes. Remaining gates did not run after the
portable failure. Expanded focus includes every orchestrator audit case.

Expanded focused5 passed all 271 tests in 107.798 seconds, including every
orchestrator case and the actual catalogue-repair control. Production is frozen
for independent parent real-cluster qualification; remaining plan/body changes
will record receipts only. Full CI runs again on the corrected fixture inputs.

CI4 passed formatting, both Clippy matrices, all 5,578 portable tests
(599.647 seconds, 75 gate-owned skips), two doctests, all 52 CI-script tests
and ignored-owner checks. This receipt precedes the gated fixture correction
below; it does not claim final-source cluster qualification.

Independent parent cluster qualification bound 960 files to the frozen source
snapshot and ran all 42 cases: 39 passed, three failed in 229.905 seconds. Two
leased-placement setup loops still sent the deliberately refused legacy request;
they now construct current-revision SchedulingDecisions without changing any
lease-retirement, decommission or handover assertions. The unrelated reporting
TCP bind19542 failure passed a correctly gated isolated unchanged-source diagnostic
in 43.780 seconds. Its cause remains unknown; the plausible fixed-port overlap
is inference only and is recorded separately in docs/flakes.md under #555.
Parent evidence: /tmp/reliaburger-review2-543-parent-cluster-evidence/.
No production source changed after the passing portable run. Final gated
qualification and static checks now cover the corrected setup requests.

Final current-source cluster qualification passed all 42 cases in 351.978
seconds, with no retries or exclusions. Both final Clippy matrices passed
(2.87/1.11 seconds), formatting and diff checks passed. Preserved final JUnit
and receipt under /private/tmp/runtime-543-evidence/. SHA256 comparison of every
src file against the parent frozen snapshot found zero production differences.
The passing portable CI4 predates only the two corrected gated setups and later
qualification prose; final static and full cluster checks cover those changes.
The earlier independent 39/42 result and unknown bind diagnostic remain intact.

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

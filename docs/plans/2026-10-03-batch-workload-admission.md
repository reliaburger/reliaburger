# Apply workload admission to batch submission

Issue: #530.

Reproduce a scoped deployer dispatching a forbidden batch job first. Validate each job, then enforce the original principal's scope and current Deploy/HostExec grants before registering or dispatching anything. Refuse test namespaces, lease headers and test-owned repositories because batch has no ownership registration protocol. Forward Authorization/Cookie unchanged rather than substituting the cluster service credential, and re-admit on the leader.

Cover scoped tokens, namespace grants, scripts/binaries and test ownership through real routers, explain admission in Chapter 8, and run portable CI plus the cluster gate.

No wire or durable-state change.

Validate job Config and reject unowned test namespaces/image references on the authenticated internal runner entry point too. Its raw HTTP regression must fail before implementation and leave the agent command channel empty on refusal.

The six permanent regressions failed on unchanged production at train
`975edfe7`: restricted and malformed requests returned 202 instead of refusal,
and a real follower forwarded the cluster service credential instead of the
caller's bearer credential. A fixture type-inference error was repaired before
that behavioral run; the compile failure isn't counted as regression evidence.

Verification: the focused batch/token API suites pass, as do formatting and
both Clippy matrices. The full portable run completed without retries or
exclusions; it failed in the unchanged upgrade-answer test (ten-second answer
deadline) and six quickstart lifecycle tests (five-second Lima deadlines).
The two previously recorded lifecycle failures remain tracked in #555; the
cause of these observations is not established here. Doctests, CI-script and
ignored-owner checks, and the real cluster gate pass. The full local `make ci`
result is therefore not green.

Remote verification of original head `0f4ef1ee` failed in one portable Linux
case: `graceful_restart_does_not_reingest_a_retired_instances_capture_file`
reported "follow ended early" at `process.rs:1568`; the other 5,478 cases passed.
The original failed job log is retained, and #555 records it. The fixture is
file-backed, so the separate memory-reader capture repair is not claimed to
resolve it. The branch inherited train `7b7575b7` through a normal merge without
rewriting its verified admission commit; republishing waits for qualification
of the separately identified final-file-rescan boundary.

Normally refreshed through qualified live namespace validation 9cf9eeb2 and
inherited the qualified file-backed capture rescan repair 3335b71c. The new
focused run passed all 64 batch, token API and ProcessGrill cases in
/tmp/runtime-530-refreshed-focused.log; formatting and diff checks passed.
Original local full-suite deadline failures and remote EOF failure remain
recorded; the refresh is not claimed as a new full local CI pass. This child
depends on #551 and #572; they must integrate into the train first.

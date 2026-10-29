# Validate namespace references at live admission

Issue: #551. Keep field validation shared while separating intrinsic semantics from namespace catalogue lookup. CLI apply/deploy/rerun must reach the cluster for permission/build references created earlier. The leader validates the union of inline and committed namespaces before writes. Standalone validation and offline lint retain complete-file checks. Build-only manifests also route through leader admission; apply does not execute builds.

Tests first: on unchanged train `7b7575b7`, all four regressions failed for unknown existing namespaces: CLI permission and build loading, actual leader HTTP apply, and actual three-council follower HTTP apply. Log: `/tmp/reliaburger-review2-24-baseline.log`. The earlier combined audit baseline is retained separately. Compilation failures are not regression evidence.

Verify focused configuration/CLI/API cases, formatting, both Clippy feature matrices, full portable CI and the cluster gate. Controls preserve existing namespace quotas, refuse ghost references without writes, and retain syntax checks and standalone/offline behavior. No wire or durable format changes.

Qualification: 78 focused configuration/CLI/API tests passed. Full local
`make ci` passed formatting and both Clippy matrices, then completed 5,419
portable passes, one unchanged Apple CLI fixture failure and one unchanged
join-token enrolment timeout (5,421 selected, 75 provisioned skips, zero retries).
The failed run is retained, with both exact diagnostic passes recorded in
`docs/flakes.md` under #555. The remaining gates passed: 42 real cluster cases,
two doctests, all 52 CI script tests and ignored-owner validation. Full local
`make ci` is not claimed green; exact-head remote release CI remains required.

Normal integration with the dry-run child retained both regression sets.
The refreshed source passed formatting, both Clippy matrices and all 145
combined validation/CLI/HTTP/preview regressions with zero retries. Publication
is held for the independently reproduced #532 file capture repair so the same
known Linux failure does not trigger another avoidable CI run.

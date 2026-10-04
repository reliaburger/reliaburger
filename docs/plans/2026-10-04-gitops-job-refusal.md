# Explicit GitOps job refusal

Resolve [#549](https://github.com/reliaburger/reliaburger/issues/549) after the shared configuration tree child #548. Preserve the two existing baseline job refusal regressions and add a real Git/Raft runner regression before changing production.

Validate the whole tree, then refuse any job before diffing or producing desired-state writes. Name unsupported jobs and direct operators to manual apply or batch submission. Preserve the prior applied SHA and supported resource state while recording the failed candidate in history. Do not introduce job dispatch without a durable Git revision-to-run identity.

Cover signed and unsigned commits, repeated reconciliation, cron-only and job-only trees, canonical migration prerequisites, and the supported app/namespace/permission control. Update the book, operations manual and whitepaper. Run focused regressions and complete portable CI with zero retries; required remote checks must pass before merging. No wire or durable-state fields change.

Qualification retained all three expected baseline failures, then 79 focused passes. The complete portable run finished 5,421 passes and one old desired-state parity fixture failure: that fixture still declared an unsupported job while checking only app/namespace/permission state. Removing its unsupported execution declarations preserved all convergence assertions; 80 focused cases and all remaining static, doctest and CI-policy checks passed afterward. The initial complete-run failure remains evidence, and exact-head remote full CI is required before integration.

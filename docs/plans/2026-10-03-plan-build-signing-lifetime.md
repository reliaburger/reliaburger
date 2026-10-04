# Build signing lifetime (#529)

An artefact signature must not inherit a running workload's one-hour mTLS lifetime. We will keep current-time chain verification and revocation checks, with a dedicated long-lived code-signing leaf capped by its issuer's expiry. Operators must re-sign artefacts before their signing authority expires; unsigned historical timestamps cannot extend that authority.

1. Add a clock-injected two-hour signature regression, a shortened-issuer validity test, and an expired-cache replacement test. Run them against the baseline before the implementation.
2. Separate code-signing and mTLS lifetimes, clamp code-signing validity to the issuer, and renew cached authority before expiry.
3. Explain the maintenance policy in chapter 10 and the security manual. Run portable CI and relevant signing/build acceptance.

The wire and durable structures stay unchanged. Certificates issued under the old policy still require re-signing.


Baseline evidence: `cargo test --lib cluster_artifact_signature_remains_valid_after_two_hours`
failed before production changes with `ChainVerifyFailed("certificate has expired")`.
The added upper-bound and cache-renewal regressions cover actual certificate fields
and both expired and soon-expiring cache entries.

Validation: formatting and both Clippy configurations passed. The complete
portable suite reached 3,653 passes before two unrelated quickstart Lima fixture
5-second deadline failures, with 1,713 tests left unrun. All 205 selected signing,
identity and build-runner tests passed with two workers and no retries. An
explicit run passed each new two-hour, CA-cap and cache-renewal regression. Both
doctests, all 52 CI script tests and ignored-test ownership checks passed. Real
Buildah/runc acceptance needs provisioned Linux CI; this Darwin host has neither
executable. Current-time validity of every certificate, including the trusted
root, remains the deployment policy.

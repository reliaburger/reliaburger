# Registry verification admission (#539)

Pickle must use Sesame's existing process-wide token-verification bound. The API and registry share Tokio workers, so registry authentication cannot hash synchronously on one of them.

1. Add router regressions for authenticated reads and writes, using Bearer and TLS Basic. Hold Sesame's verification permits and require those requests to wait. Malformed credentials must be refused before waiting.
2. Run the admission regression against the baseline and record its failure before changing production code.
3. Route registry user-token verification through `authenticate_off_lock`, preserving bootstrap, service-token, role and repository-scope decisions.
4. Update chapter 5, run portable CI and the standard-client gate where its prerequisites are available, and open a focused PR into the review train.

There is no durable-state or wire-format change.

Baseline evidence: the router regression failed with `GET registry verification bypassed the shared admission` before the production verifier changed.

Validation on the review train base: formatting and both Clippy configurations
passed. Full `make ci` reached 3,692 passes before the unrelated quickstart
`a_vm_that_boots_is_left_alone` fixture failed; 1,674 later tests were not run.
A focused run passed all 349 Pickle and portable standard-client tests with two
workers and no retries. Both doctests, all 52 CI script tests and ignored-test
ownership checks passed. The explicit standard-client gate started its TLS
listener but failed because `crane` is absent from PATH. Provisioned CI must
complete the full portable suite and real crane gate before merge.

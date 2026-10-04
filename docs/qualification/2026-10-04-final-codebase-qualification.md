# Final review2 codebase qualification, 4 October 2026

**Status: integration qualification in progress.** This record separates local
source/control evidence from the final committed CI owners. It does not approve
merging the outer train or publishing a release.

The main weakness is **consistency across paths**. The final child for #555
consolidates workload authorization, preserves lifecycle ownership during
cancellation and uncertain persistence, and requires current, complete execution
for every named contract case.

## Candidate and ownership

All 27 concrete audit children are merged into `release-0-1-5-review2` at
`42647cbdb37bd022a879136b4bcb6c111f73ff2e`. The final child inherits that train
through `cf118a805de372e317e295d02252686b524f0ef3`. The release parent remains
`4f1bab55adbae0fd2c76b5aa1e1e25b8a3b0171c` on `release-1-1-5`.

The final durable-state generation is 57; protocol remains 40. New metrics
publication and log checkpoint metadata change restart/ownership interpretation.
The capture identity fields retain the previous device/inode shape; capture
positions and offsets remain in-process types. This is not a wire-format bump.
Pre-1.0 nodes refuse the previous state generation; no migration is claimed.

The [generated matrix](../testing/qualification-matrix.md) contains 563 concrete
cases under 62 contracts and eleven CI owners. It preserves the original 245
ignored classifications and adds one direct OCI capacity case: 229 CI bindings,
17 manual/subprocess classifications and exactly twenty finite legacy aliases.
Source enumeration and hashes do not establish compiled discovery or execution.
Current CI seals bind the final checkout, run, attempt, host, tools, manifest,
complete owner command and successful producer outputs.

Local tools: Rust/Cargo 1.98.1, nextest 0.9.145, LLVM-cov 0.8.7, macOS. CI pins
Rust 1.98.0 and Linux LLVM-cov 0.9.1. The local older coverage adapter is not
accepted as Linux coverage proof. The 78.65% aggregate floor and zero retries
are unchanged. Real crane, Linux OCI, kernel/root-storage, manual hardware and
cloud/power-cut qualifications require their declared owners.

## Tests first and focused results

| Change | Actual baseline | Repaired local evidence |
|---|---|---|
| Mayo ownership and publication | Ten original assertion failures; independent fresh-reopen failure; two additional before-descent failures | All 233 current Mayo cases covered across a 232-case run and one added public control; all twelve lifecycle controls included |
| Ketchup checkpoint/capture identity | Seven original assertion failures and two legacy parent-confirmation failures | Eleven controls and all 109 adjacent cases passed |
| Registry ancestors and warm cache | Four intended failures; corrected forced-unpack fixture separately fails original implementation on corrupt gzip bytes | Seven controls and all 209 adjacent cases passed |
| Startup diagnostics | Four real exited-child missing/invalid-log assertions failed before helper | All four passed afterward; original child/readiness bounds retained |
| Shared workload authorization | Four source-policy architecture assertions failed; paired real HTTP authority control already passed the existing implementation | Four architecture controls and all 247 authorization/apply/batch cases passed; original principal and refusal effects observed |
| Static permission guardian | Existing matrix guardian and actual shared-Deploy control failed after the helper extraction | Six source controls and all 23 adjacent authorization/permission tests passed; exact helper, propagated error and unconditional Deploy required |
| Optimized cron owner | Real Cargo metadata refused two transitive thiserror versions | Direct root production dependency lookup verified equal actual versions/features; all eleven optimized production-module tests passed |
| Evidence front doors | Original incomplete/completion, context, source, ownership and whole-command refusal controls retained | 310 Python controls passed in the complete current-source snapshot |

Compiler/fixture setup failures are retained separately: Mayo read-guard borrow
correction, capture test-hook Debug derive, authoritative EOF checkpoint fixture
migration, rootfs completion-marker fixture correction and owner mock-metadata
schema extension. A disconnected directory fault injector is wiring evidence,
not an additional product defect. Initial full integration stopped at seven
Clippy findings before behavioral execution; comments, equivalent slice
assertions and a cfg(test) callback type alias repaired them. The next whole
portable run passed 5,684 of 5,685 cases and failed only the existing static
permission guardian, which expected the Deploy marker literally inside the
handler. The narrow helper-aware repair above preserves the route matrix and
checked counts; the full final-source run is repeated after that repair.

## Representative harness sensitivity

Each variant compiled, reached the intended behavior assertion, then restored
exact healthy source and passed the same selected control. No mutation remains.

| Temporary behavior break | Actual observed failure | Restored outcome |
|---|---|---|
| Omit workload token scope | Paired HTTP path returned 200 instead of 403 | Same HTTP control passed |
| Settle by logical job name | Previous terminal report completed the new batch: completed 1 instead of 0 | Same run-generation control passed |
| Drop store-owned pending metrics | Canceled writer left zero queryable samples instead of one | Same cancellation control passed |
| Skip typed directory defaults | Watched Git and CLI/Git controls lost literal expected image defaults | Both controls and no-default namespace positive passed |

The fake actor acknowledgements in the paired authorization fixture establish
transport/admission authority, not a real child exit. The direct OCI capacity
control retains its own real runtime owner and controlled capacity reports.
Three Smoker setting controls exercise temporary file I/O, not kernel pressure.
Directory fault tests establish ordering, propagation and retry; they are not
power-cut experiments. Unpublished in-memory data is not process-death durable.
Capture identity assumes append-only files and does not guarantee arbitrary
same-inode rewriting or eventual inode reuse.

## Whole final-source qualification

The final Rust source passed all 5,690 portable tests in 201.696s, both
Clippy configurations, formatting and both doctests. The matching cluster gate
passed all 42 cases in 348.329s on the same production inputs before the final
test-only guardian addition; all eleven optimized cron cases passed.

That `make ci` invocation then stopped in the Python controls: the local
`CARGO_TARGET_DIR` setting leaked into the mock OCI driver fixture, producing
25 setup errors and one mismatched refusal assertion. This remains a failed
whole invocation. The test-only fixture isolation preserves all existing negative controls and
the actual owner override guard. The combined final source subsequently passed
all 310 Python controls in 23.287s with that local build setting present outside
the fixture, plus the ignored-test ownership check. No Rust input changed;
there was no redundant Rust rerun or claim that the failed whole invocation
became successful.
The source-only inventory was refreshed during initial Clippy compilation;
no Rust, Cargo, build or nextest inputs changed during execution.

Compiled Linux/macOS inventory, actual Linux coverage and changed-line/family
coverage, provisioned OCI and current committed workflow aggregation remain
**PENDING** until the PR checks and artifacts qualify their exact checkout. Local source receipts remain separate from
trusted current committed CI artifacts. The final child PR and outer train PR
will identify the exact qualified head, run and artifact results before merge.

CI integration controls also caught cached empty evidence directories and a
root-owned private storage report directory. One bounded reset after each cache
restore retains owner exclusivity within a job. A separate always-run step
restores runner ownership of only the completed privileged report directory
before upload, retaining the original producer outcome. Four tests-first source
controls and a real stale-directory cleanup fixture qualify that wiring; hosted
cache restoration and privileged upload remain current-CI obligations.

## Failure ledger

The combined local startup/process/upgrade fixture focus completed 91 cases:
90 passed, one existing upgrade stress test exceeded its unchanged 10s verified
compatibility-query bound. One unchanged diagnostic subsequently passed in
4.388s. Phase labels reached probe completion around 9.93–10.27s; cause remains
**UNKNOWN**. This is not a whole-focus pass or proof of a repaired timing cause.
See the same-day [flake register](../flakes.md). Earlier full-run failures from
individual children remain recorded there; a later diagnostic never replaces
those results. Retries, deadlines and coverage requirements are not weakened.

**Maintainer review pending:** outer PR #556 remains draft. Agents do not merge
that PR, tag or promote. Automatic issue closing applies when the release reaches
the default branch; child merging into the review train alone does not close them.

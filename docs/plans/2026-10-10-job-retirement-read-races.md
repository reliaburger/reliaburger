# Finish the job-runtime review races

[#669](https://github.com/reliaburger/reliaburger/pull/669) is merged into the
feature branch for [#654](https://github.com/reliaburger/reliaburger/pull/654).
Keep #654 open until its two remaining races are fixed and tested.

First, prove that filesystem cleanup still owns its slot and reservation after
retirement times out. Retain one cleanup handle across retries; release capacity
only after that same operation finishes. Test another executor's progress while
cleanup stalls, and safe reuse after completion.

Second, force atomic record replacement between the preliminary link-count
inspection and validation. Retry on an unlinked descriptor using the same metadata
inspection that validates privacy. Keep the bounded retry count and rejection of
symlinks, hard links, wrong modes, malformed records and oversized records.

Update chapter 12 with both ownership lessons. Run the regressions against the
unfixed logic, then run portable CI and the Linux executor gate on the fixed
feature branch. Commit and push the fixes to #654, without merging it into main.

The merged-head GitHub rootless gate also exposed a fixture dependency on
`OCI_CRASH_ROOT` in runc/ip wrappers. Derive their fixture root from the wrapper
path, keeping Bun's admission injector environment private. Prove the wrappers
work with a cleared environment and run `make test-rootless-runc` as well.

Both race regressions failed against the old logic and pass with the fixes.
Portable `make ci` passed on macOS: 6,212 tests, two doctests and 373 script
tests, plus formatting, strict lint and ignored-test ownership checks. Linux
strict lint passed with all features and with no default features; all 30
focused pool, durable-record and mixed-runtime tests passed with eBPF enabled.
All 11 rootless runtime tests passed, including the corrected OCI crash wrapper.

The test VM needed its inactive Rust recovery cache reclaimed and the same
AppArmor namespace setting used by CI. The first full Linux attempt stopped on
a full disk; the rootless attempt also hit that space limit and the restrictive
namespace setting. These attempts are not passing qualification. The full
privileged Linux gate then passed 101 tests before a new container collided with
an address still routed to a retained benchmark workload. Qualification is
being rerun serially in private mount and hostname namespaces, with a separate
`/etc/hostname` giving the test runtime a different node subnet. The retained
workload and VM hostname are unchanged.

The isolated gate passed 142 tests, then exposed another fixture's late PATH
override. The cancellation fixture now sets its fake `ip` in an isolated
caller's environment before recording runtime commands, rather than overriding
PATH in the owner wrapper. Its cancellation and positive-retirement assertions
remain. The interrupted gate was completed by rerunning all 24 selected
owned-runc/storage tests and all 18 task-array/host-helper/public catalogue tests:
both groups passed. Both strict Linux lint configurations were repeated and
passed after the fixture correction. Formatting and ignored-test ownership
checks also passed. Implementation and local qualification are complete; #654
remains open for review and its fresh GitHub checks.

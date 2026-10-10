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

The first pushed head's CI policy check caught a missed source-pin refresh for
`tests/owned_runc.rs`: the script suite had run before the final fixture edit.
Reproduce that refusal, refresh only that reviewed file's 17 registry pins after
its runtime suite passes, and rerun the full portable checks. Preserve the test
identities, ownership mappings and rejection of later unreviewed source changes.


The actual OCI interruption gate also exposed a production recovery bug:
`ProcessControl::load` regenerated host defaults from its current caller rather
than validating the recorded preparation snapshot. The new regression fails
against the old logic. Preserve inherited values from the private record, require
all explicit workload overrides (last value wins), and reject unrequested private
variables. Requalify automatic restart interruption and the full portable checks.


Full portable `make ci` passes after both CI fixes: 6,214 tests, two doctests,
373 script tests, formatting, both strict lint configurations and ignored-test
ownership checks. All 54 Linux process/backend/owner/control tests pass with
eBPF enabled. The actual OCI interruption binaries are being rebuilt for local
isolated execution; GitHub must produce its own fresh source-bound receipts.


The isolated owner suites passed 20 owned-runc and five owned-network cases.
A temporary disk-image setup first blocked the non-root capacity payload with
mode 0700 on its parent; mode 1777, matching `/tmp`, restored the same case.
The actual Bun interruption run then passed seven of eight cases, including
the original automatic-restart failure and three-node upgrade/rollback. The
single-node upgrade hit the eight-reopen record budget on the disk image and
passed on private Linux tmpfs. Retain that failed attempt rather than claiming
a clean full-gate pass.

Deterministic regressions expose the reader policy problem: nonexclusive reads
needlessly follow every replacement instead of accepting their valid atomic
snapshot. Apply the link/reopen requirement only to `Exclusive`; keep the same
privacy checks for the already-open `Regular`/`OwnerOnly` snapshot. Both new
regressions fail before this correction. Repeat portable CI and all three actual
OCI binary groups, including the previously interrupted upgrade case, afterward.


The first portable repeat after the snapshot fix stopped on the unchanged
invalid-UTF8 diagnostic control, whose `bun --compatibility` child exited
unsuccessfully. Its path and Linux Bun resolved to the same inode and ELF bytes.
The underlying cause was a macOS `debug` directory symlink to the Linux cache;
removing individual entries did not separate publication. The reverse Mach-O
replacement invalidated an OCI attempt (two fixtures passed, eighteen runtime
cases failed). Retain those failures as rig errors, not qualification.

The subsequent portable run completed with Linux compilation stopped: all
6,216 tests, two doctests, 373 script tests, formatting, both strict lint
configurations and ignored-test ownership checks passed. After it finished,
replace the macOS `debug` symlink with an independent APFS clone of its validated
Mach-O artifacts, preserving the original link for recovery. Restore ELF Linux
outputs and repeat all actual OCI cases. Record both rig and reader failures in
`docs/flakes.md`; no per-test deadline or retry-policy increase addresses them.


Final Linux qualification passed all 20 owned-runc and five owned-network cases,
then seven Bun OCI interruption cases, including the original automatic-restart
failure and the previously interrupted single-node upgrade. The last three-node
case hit a separate initial voter-promotion timeout: three live gossip members,
two Raft voters. Its unchanged final-source isolated run passed on private Linux
tmpfs in 89.13 seconds. Track the unexplained promotion/teardown flake in #673
(0.2.1), retaining the failed receipt; the disk-image run is not a clean full-gate
pass. All runtime cases are covered through these resumed executions. The local
outer Bun-suite watchdog was 600 seconds for large debug copies, versus CI's
420; no per-test deadline or automatic retry policy was changed.

#672 merged into #654 during qualification (remote head `1cf7e4f7`), adding the
release-job soak and landing-page commands. It changes no Rust source or tests.
Rebase these CI repairs onto that head and repeat both CI/release Python suites,
formatting and ignored-test ownership checks. Preserve the full portable and
runtime results above; fresh GitHub checks must produce their own receipts.

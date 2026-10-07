# Common job lifecycle: qualification

PR #642 implements #638 on main after merged #266 and the 0.1.6 release updates
(86c022e7). Protocol/state are **48/65**: use matching binaries and fresh cluster
data. This qualifies lifecycle correctness, not the 100m/day throughput target.

## Environment and commands

Portable, cluster and upgrade gates use an Apple-silicon macOS host, Rust 1.98,
nextest 0.9.145, one Cargo build job, no incremental compilation and debug-free
development/test artifacts. The provisioned Linux runtime gate uses the existing
4-vCPU / 8-GiB aarch64 Lima VM, rootful runc, eBPF and the pinned image mirror.
Its development artifacts retain line tables. All tests use the `ci` nextest
profile with retries disabled.

```sh
make ci test-slow NEXTEST_PROFILE=ci
make test-cluster test-upgrade NEXTEST_PROFILE=ci
make test-linux NEXTEST_PROFILE=ci
make test-rootless-runc NEXTEST_PROFILE=ci
```

The local Linux invocation uses an external Cargo target directory, its actual
built `bun` path and the mirror on `127.0.0.1:5199`; otherwise it uses the complete
`test-linux` selection and required runtime flags. Current and obsolete build
profiles stay separate; clearing only this task's obsolete artifacts preserved
the storage tests' free-disk reserve. The rootless invocation uses the same Make
filter and builds only its containing `owned_rootless` and `oci_crash` binaries;
all 11 selected cases run as the unprivileged VM user.

## Results

| Gate | Result |
|---|---|
| Portable CI | Passed: formatting, both Clippy configurations, 6,131 portable tests, two doctests, 318 CI-script tests and ignored-test ownership |
| Wall-clock acceptance | Passed: all four cases |
| Real multi-node cluster | Passed: all 46 cases |
| Real-binary upgrade | Passed: all 18 cases |
| Provisioned Linux runtime | Passed: all 156 cases |
| Rootless runc | Passed: all 11 cases |
| Hook fixture repetition | Passed: all 30 cases in ten iterations |

The cluster gate exercises an actual UTC cron firing, a real Raft election and
accepted completion under the replacement leader, then replays the occurrence
and proves the same run remains. It also loses a singleton worker, keeps its
outcome unknown, rejects a stale acknowledgement and permits only exact,
explicit operator replay through a follower. Existing bulk-worker-loss and
100,000-task invariants run in the same gate.

Real-container cases preserve encrypted environment and script execution, keep
apps gated until hooks have accepted successes, and share actual CPU, memory and
FIFO admission between apps, singleton jobs and arrays. Portable recovery covers
crashes before acceptance, completed worker receipts, conservative unknown
ownership, storage refusal and publication fencing. Run-bound log readers and
fresh capture identities prevent a reused physical slot from serving another
run's output.

## Failures addressed during qualification

The first portable run passed 6,130/6,131; the existing #555 compatibility-probe
case exceeded its unchanged 10-second deadline. The unchanged diagnostic passed
in 4.528 seconds. See [the retained failure and diagnostic](upgrade-probe-timeout.txt)
and the [existing flake entry](../../flakes.md). No production deadline or retry
was changed.

A subsequent portable run found shared initialisation storage in the three hook
fixtures. Private `TempDir` handles fixed that race; all 30 repeated cases and
the complete portable gate then passed. The first cron handover run reached a
real election and successful completion, but expected the wrong response to an
already-processed internal tick. The corrected test asserts its no-op
acknowledgement and unchanged accepted run identity; the full cluster gate passed.

The initial Linux run passed 154/156. Its assumed dead TEST-NET DNS upstream
returned a successful answer in the VM; an owned silent loopback socket now
proves timeout. The namespace-loss test expected a pre-launch failure after it
had actually started work. It now establishes a running owner before fault
injection and requires `Unknown` plus positive process-tree retirement. These
fixture changes preserve production resolver and execution behaviour.

The rootless gate initially passed 2/11: the VM still had
`kernel.apparmor_restrict_unprivileged_userns=1`, and namespace creation returned
`Operation not permitted`. Applying the existing CI prerequisite (`=0`) reached
9/11; two remaining cases then failed with `ENOSPC` while the VM disk had only
366 MiB free. Removing only the two failed fixture directories identified in
this task's logs reclaimed 3.2 GiB. The complete unchanged selection passed
11/11, and the original AppArmor setting was restored. No product behaviour,
deadline or retry policy changed.

Reusable executors (#639), throughput qualification and the landing-page demo
(#640), resident model workers (#641) and GPU placement (#359) remain separate.
No sustained daily-throughput, runtime-cost or million-job demo claim is made.

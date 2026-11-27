# V02 macOS upgrade-node loop on a quiet Mac Studio, 5f21eef6 (26 Nov 2026)

The rerun the [24 November record](2026-11-24-v02-loops-local.md#rerun) asked
for: lane D of the V02 qualification
([#287](https://github.com/reliaburger/reliaburger/issues/287), second bullet),
the macOS `make test-upgrade-node` lane alone, on a quiet host, on the 0.3.3
candidate, which carries the process-owner fix
([#2236](https://github.com/reliaburger/reliaburger/issues/2236),
[PR #2249](https://github.com/reliaburger/reliaburger/pull/2249)).

**Result: PASS, 115 of 115 iterations.** Each iteration ran all eight
`tests/self_upgrade.rs` tests, so every test passed 115 times in a row, which
bounds each one's failure rate below 2.61% at 95% confidence (rule of three,
3/115). That includes
`isolated_replacement_reverts_without_restarting_its_workload`, which failed 76
times in 142 on the loaded host of 23–24 November. Zero failures here fit the
24 November record's reading that its failures came from #2236 under load,
but don't prove it: the earlier failures kept no `/v1/status` body.

## Environment

| Item | Value |
|---|---|
| Source | [`5f21eef6`](https://github.com/reliaburger/reliaburger/commit/5f21eef6abc384d7e1c3cb8ff633aa7c94074a7f), the 0.3.3 candidate (build [45869980780](https://github.com/reliaburger/reliaburger/actions/runs/45869980780)), a clean detached worktree `~/github/rb-wt/v02-node-033` with its own `CARGO_TARGET_DIR` and `CARGO_INCREMENTAL=0` |
| Command | `make test-upgrade-node`, that is `RELIABURGER_UPGRADE_TESTS=1 cargo nextest run --profile default --no-tests=fail --run-ignored=only -E 'binary(self_upgrade)'`, in a loop for 5 h 30 min |
| Host | Mac Studio, Apple M5 Max, 18 cores, 128 GiB, macOS 27.2; rustc 1.99.1, cargo-nextest 0.9.151 |
| Time | 2026-11-26 17:43 GMT to 23:13 GMT. The first iteration built the test binaries (3 min 53 s) and took 410 s; the other 114 took 170 to 173 s each |
| Quiet | The loop held `~/.reliaburger/quiet.lock` for its whole run, and the 0.3.3 gate chain waited for it to finish before starting ([closure](2026-11-27-v0.3.3-release-closure.md)). No host load log was kept |
| Evidence | [`loop.log`](2026-11-26-v02-upgrade-node-macos/loop.log), one line per iteration, kept here. The per-iteration nextest logs stay on the Studio under `~/github/rb-wt/gate-033/qual/v02-node/` |

The loop was a short Python script rather than
`qualify-v02-loops-local.sh`, because only this one lane needed rerunning. It
took the quiet lock, then ran `make test-upgrade-node` until 5 h 30 min had
passed, writing each iteration's exit status, duration and running pass count
to `loop.log`, and each iteration's output to `iter-NNN.log`.

## Results

| Test | Runs | Failures | 95% bound (rule of three) |
|---|---|---|---|
| `single_node_upgrade_preserves_running_containers` | 115 | 0 | < 2.61% |
| `single_node_upgrade_adopts_an_app_that_uses_a_secret_provider` | 115 | 0 | < 2.61% |
| `single_node_rollback_reverts_to_previous_version` | 115 | 0 | < 2.61% |
| `failed_upgrade_triggers_automatic_rollback` | 115 | 0 | < 2.61% |
| `version_retention_gc_keeps_last_three` | 115 | 0 | < 2.61% |
| `upgrade_rejects_bad_external_signature` | 115 | 0 | < 2.61% |
| `same_version_upgrade_never_swaps_silently` | 115 | 0 | < 2.61% |
| `isolated_replacement_reverts_without_restarting_its_workload` | 115 | 0 | < 2.61% |

Every iteration's nextest summary reads "8 tests run: 8 passed, 2 skipped";
the two skipped are the file's two ordinary `#[test]`s for the `workload_pid`
helper, which the portable suite runs. Each iteration is one nextest run, so
each row's runs are the iterations, not separate processes per test.

## What this record supports

- On macOS, a bound of 2.61% on each single-node upgrade test, on a quiet
  host, on the 0.3.3 candidate. The 12 November GitHub record gave 154 runs on
  `macos-15`; this one is on the hardware the gate uses.
- Nothing on the other lanes. The 24 November record's macOS
  `test-upgrade-cluster` and `registry-new-leader` failures were port
  collisions on a shared host, and those lanes didn't rerun here. Nor does
  this bound anything on x86-64.

#287 stays open: its first bullet (the snapshot uploader under power cuts)
and the lanes above still need records.

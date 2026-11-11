# 0.2.8 candidate 2 (`e43417a8`)

11 November 2026. The second 0.2.8 candidate's gate records: candidate 1
plus the fixes for #1849 and #1853, and the soak harness's fix. The extra V02
fast run, on the seed that caught #1849, found
[#1857](https://github.com/reliaburger/reliaburger/issues/1857), and candidate
1's final had found [#1855](https://github.com/reliaburger/reliaburger/issues/1855).
Candidate 3 carries the fixes for both, so these records are kept as
evidence. The [release closure
record](../2026-11-11-v0.2.8-release-closure.md) gives the whole gate.

## The candidate

| Item | Value |
|---|---|
| Commit | [`e43417a8`](https://github.com/reliaburger/reliaburger/commit/e43417a8a2a147910667ba9a075af03e0fcb6a8d): main after [#1850](https://github.com/reliaburger/reliaburger/pull/1850) (the soak harness reads `relish apps --instances`), [#1851](https://github.com/reliaburger/reliaburger/pull/1851) (#1849) and [#1854](https://github.com/reliaburger/reliaburger/pull/1854) (#1853) |
| Build | [run 42927004563](https://github.com/reliaburger/reliaburger/actions/runs/42927004563), attempt 1; it reused main's push CI run 42919895170 |
| Stage | [run 42932278337](https://github.com/reliaburger/reliaburger/actions/runs/42932278337) |
| Staged base URL | <https://github.com/reliaburger/reliaburger/releases/download/staging-v0.2.8-42927004563-1> |
| `candidate.json` SHA-256 | `700071d536ef66354ee701e6b09d5a0233beeadcd8ef14bcddf2e1e15b9b8ff8` |
| Soak build | `bun-v0.2.8-soak.1` (SHA-256 `413aadcf93225af75e25fac041339094a5c7d2546591a517aba16620cf8dd3fb`), built from the candidate's commit and signed with the release key |
| Host | Mac Studio, Apple M5 Max, 128 GiB; three quickstart Lima VMs, 2 vCPU / 2 GiB; nothing else built or tested on it |

## Records

| Gate | Result | Record |
|---|---|---|
| Power cut and reboot, 25 per lane | PASS: every lane 25 of 25, 00:48 to 01:33 GMT | [record](2026-11-11-power-cut-reboot.md) |
| Staged install, Apple silicon | PASS, 66 s to a ready cluster | [record](2026-11-11-v0.2.8-staged-install-apple-silicon.md) |
| Staged install, second cold install | PASS, 61 s | [record](2026-11-11-v0.2.8-staged-install-apple-silicon-2.md) |
| V02 soak, fast tier | PASS, fast tier clean: 1 h 08 min, seed 352556864; council history linearizable; no turn over budget | [record](2026-11-11-v0.2.8-sustained-v02-fast.md), [jobs](2026-11-11-v0.2.8-sustained-v02-fast.jobs.json) |
| Job qualification, fast tier | PASS, fast tier clean: 1 h 03 min, seed 1310215243; 8,803.5 accepted a second against the 1,390 target; council history linearizable; no turn over budget | [record](2026-11-11-v0.2.8-job-qualification-fast.md), [jobs](2026-11-11-v0.2.8-job-qualification-fast.jobs.json) |
| V02 soak, fast tier, seed 3139726783 | FAIL on [#1857](https://github.com/reliaburger/reliaburger/issues/1857): one `health_probe_result` turn of 1,152 ms on node 3. Both deploy-kill-then-power-off sequences on node 2 recovered, so #1849 is fixed | [record](2026-11-11-v0.2.8-sustained-v02-fast-seed-3139726783.md), [jobs](2026-11-11-v0.2.8-sustained-v02-fast-seed-3139726783.jobs.json) |
| Job qualification, final tier, stopped at 7 h 33 min | Stopped on purpose for candidate 3, so incomplete. No failures: cycles 0 to 6 and four upgrade walks, no turn over budget, 9,285.6 accepted a second | [record](2026-11-11-v0.2.8-job-qualification-stopped.md), [jobs](2026-11-11-v0.2.8-job-qualification-stopped.jobs.json) |

## The seeded V02 run

This run repeats the fault and node choices of candidate 1's V02 fast tier,
the run that found #1849. Its deploy-kill then power-off of node 2 ran twice,
at 04:05 and 04:29 GMT, and Bun came back both times. Its only failure was
on node 3 at 04:44:27 GMT, 20 seconds into a chaos-dns fault and just after
an upgrade walk with a fault mid-walk:

```
bun: agent loop turn took 1152 ms in deploy_op (health_probe_result): journal write 1152 ms
```

Unlike #1853's turn, the watchdog's thread snapshot showed the thread
running the agent loop itself in D state in `lseek` (`ext4_llseek`), with
other workers in `write` and `fdatasync`. A blocking file call ran on a
runtime worker, so the turn deadline couldn't cut it short.
[#1858](https://github.com/reliaburger/reliaburger/pull/1858) moves blocking
file calls off the runtime's workers.

## The final, stopped

Candidate 3 was staged at 12:44 GMT. The final was stopped at 12:45, the
last step having settled at 12:38:25, as cycle 7 began its node fault. In
7 h 33 min it ran cycles 0 to 6, four upgrade walks to the soak build and
back, and 34 fault and node choices from seed 1024744514, with no failure.
Bun's worst steady-state RSS was 215.16 MiB on node 2, against 384 MiB. The
record's execution inventory is incomplete, and its leak-checker exposure is
short, because the quiet tail and the drain never ran.

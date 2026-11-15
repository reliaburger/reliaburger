# 0.2.8 candidate 1 (`08618555`)

10 and 11 November 2026. The first 0.2.8 candidate's gate records. Its V02
and jobs fast tiers each found a product bug, so it was never a release
candidate; candidate 2 replaced it, built after both fixes. These records are
kept as evidence. The [release closure
record](../2026-11-11-v0.2.8-release-closure.md) gives the whole gate.

## The candidate

| Item | Value |
|---|---|
| Commit | [`08618555`](https://github.com/reliaburger/reliaburger/commit/08618555ffe3112f2bd67842cadedfdd61d8071c) (main after the 0.2.8 merge train, [#1706](https://github.com/reliaburger/reliaburger/pull/1706)) |
| Build | [run 42847458141](https://github.com/reliaburger/reliaburger/actions/runs/42847458141), attempt 3. It reused main's push CI run 42837235674. Attempts 1 and 2 failed only because `notarise-macos` never got a hosted macOS arm64 runner ("not acquired by Runner of type hosted"); every build job passed each time |
| Stage | [run 42863527190](https://github.com/reliaburger/reliaburger/actions/runs/42863527190) |
| Staged base URL | <https://github.com/reliaburger/reliaburger/releases/download/staging-v0.2.8-42847458141-3> |
| `candidate.json` SHA-256 | `f3a34fdee0a866c1a288fc66e056baac4dc8bd7d43d6a631d33cb01acfeb3e81` |
| Soak build | `bun-v0.2.8-soak.1` (SHA-256 `8c3ec0aa66fdb30bbabf7aa2633ea852876e2fd2826e3ed506769dc160c35d75`), built from the candidate's commit and signed with the release key |
| Host | Mac Studio, Apple M5 Max, 128 GiB; three quickstart Lima VMs, 2 vCPU / 2 GiB |

The macOS `relish` in this build isn't notarised: the `release` environment
had none of the Apple secrets, so `notarise-macos` left it as built.

## Records

| Gate | Result | Record |
|---|---|---|
| Staged install, Apple silicon | PASS, 65 s to a ready cluster | [record](2026-11-10-v0.2.8-staged-install-apple-silicon.md) |
| Staged install, second cold install | PASS, 59 s | [record](2026-11-10-v0.2.8-staged-install-apple-silicon-2.md) |
| Power cut and reboot, 25 per lane | PASS: every lane 25 of 25, 17:10 to 17:50 GMT | [record](2026-11-10-power-cut-reboot.md) |
| V02 soak, fast tier | FAIL on [#1849](https://github.com/reliaburger/reliaburger/issues/1849): node 2's Bun crash-looped after a power cut that followed a deploy-kill. Council history linearizable; no turn over budget | [record](2026-11-10-v0.2.8-sustained-v02-fast.md), [jobs](2026-11-10-v0.2.8-sustained-v02-fast.jobs.json) |
| Job qualification, fast tier | FAIL on [#1853](https://github.com/reliaburger/reliaburger/issues/1853): one `deploy_op` turn of 1,593 ms on node 3. 8,517.1 accepted a second against the 1,390 target; job-start p99 32 ms; council history linearizable | [record](2026-11-10-v0.2.8-job-qualification-fast.md), [jobs](2026-11-10-v0.2.8-job-qualification-fast.jobs.json) |
| Job qualification, final tier, stopped at 3 h 54 min | Stopped on purpose for candidate 2, so incomplete. One finding: one ingress probe answered 502 outside a fault window, then recovered. No turn over budget; 9,280.0 accepted a second | [record](2026-11-10-v0.2.8-job-qualification-stopped.md), [jobs](2026-11-10-v0.2.8-job-qualification-stopped.jobs.json) |

## What each failure was

**The soak harness read the old status table.** The first V02 fast run
failed in setup: "soak workloads not running after 600 s", with all 11 apps
running. 0.2.8's `relish status -o json` is the cluster summary in the agent
envelope ([#1788](https://github.com/reliaburger/reliaburger/pull/1788)), but
`qualify-sustained.sh` still read it as a list of instances. The fix reads
`relish apps --instances` instead
([#1850](https://github.com/reliaburger/reliaburger/pull/1850)), and the soaks
above ran with it. It changed the harness, not the candidate.

**Bun crash-looped after a power cut (#1849).** In cycle 4 (seed
3139726783), a deploy-kill SIGKILLed Bun on node 2 one second into
`soak-identity`'s third generation. Bun restarted and adopted its instances.
Ninety seconds later the node was powered off for 25 s. From the next boot,
every start exited with:

```
Error: container default__soak-identity-g2-1 state unavailable: adoption log identity conflicts with runtime generation
```

That's over 500 systemd restarts, and node 2 never rejoined. Adoption treated
a record that the power cut left out of step with its runtime generation as
fatal. [#1851](https://github.com/reliaburger/reliaburger/pull/1851) retires
such a generation instead of refusing to start, and
[#1854](https://github.com/reliaburger/reliaburger/pull/1854) keys each
adoption record write by its launch, not its instance. Nothing lost data: the
writer checks passed and the council history was linearizable.

**A deploy_op turn waited for an fsync (#1853).** During the graceful
whole-cluster stop and start, with no fault on node 3:

```
bun: agent loop turn took 1593 ms in deploy_op (finish_fresh_instance): journal write 46 ms, kernel lock wait 0 ms, firewall sync 0 ms
```

The watchdog's thread snapshot showed the loop awaiting while a worker sat
in `fdatasync`: the instance record's write ran inside the turn.
[#1854](https://github.com/reliaburger/reliaburger/pull/1854) moves the
record and identity fsyncs off the loop.

**One 502 in the final.** At 23:23:40 GMT, five minutes after node 2's
176-second power-off settled, one light check's ingress probe got a 502 in
1 ms. The checks before and after got 200. It's unexplained. Node 2's Bun
was activating a discovery consumer step at the time (a 1,136 ms journal
write, handed off). The evidence is in the final's `failures/1/`.

## The final, stopped

Candidate 2 was staged at 00:35 GMT on 11 November, and the final was
stopped at 00:38:51, just after a node-delay fault settled. In 3 h 54 min it
ran 4 cycles, 2 upgrade walks and 17 fault and node choices from seed
3986718550. Bun's worst steady-state RSS was 213.55 MiB on node 2, against
384 MiB. The record's execution inventory is incomplete, and its job
campaign drain never ran, both because the run was stopped.

## Host

The power-cut lanes before about 17:18 GMT ran beside a stray one-node
patch cluster ([#1847](https://github.com/reliaburger/reliaburger/issues/1847)),
the idle development VM and Ollama, all stopped then. Nothing else was built
or tested on the Studio during the soaks.

# Plan: power-cut endurance

[#2067](https://github.com/reliaburger/reliaburger/issues/2067). Edge sites
lose power often, the whole site at once, on small boxes with cheap flash. We
want one long-running test that does to a small cluster what a bad site does
to it, for days, and proves after every cut that the cluster came back by
itself with nothing acknowledged lost.

This plan says how big the run is, what the harness looks like, the order we
build it in and which decisions it rests on. It comes from a design draft of
17 November 2026 (prior art, fault models and invariants); the parts of that
draft this plan needs are summarised in [Background](#background). The
maintainer took the draft's recommendations for D-3 to D-11 provisionally on
17 November; D-1 (a dedicated Linux box) and D-2 (smart plugs) are open, so
everything below runs on the Studio under the quiet lock until they're
settled, and the hardware tier is a later step.

It's new test infrastructure, not a 0.3.0 fix: its PRs go into `main` and say
"Part of #2067".

## Background

The release gate already cuts power twice. Step 5, "Pull the plug"
([`qualify-power-cut-reboot.sh`](../../scripts/release/qualify-power-cut-reboot.sh)),
runs five lanes of 10–25 `limactl stop --force` cuts against library fixtures
on one disposable VM, and its first run found the `object_store` `file://`
no-fsync bug ([record](../qualification/2026-09-25-v02-power-cut.md)). The
sustained soak ([`qualify-sustained.sh`](../../scripts/release/qualify-sustained.sh))
powers off nodes of a three-node cluster every cycle, and its 0.2.1 gate found
redb bricking a Raft store after a cut
([#1024](https://github.com/reliaburger/reliaburger/issues/1024)).

What neither has is volume. "Zero failures in 25 power cuts rules out failure
rates above about 12%" ([testing.md](../testing.md#before-a-release)) is
honest and weak.

Three fault models, in increasing strength:

1. **Lose the page cache.** Everything that reached the virtual disk survives.
   A hypervisor kill gives this on our hosts: the guest's page cache dies, but
   what the VZ process handed to the host kernel sits in the host's page cache,
   which never loses power. That's a disk with no volatile write cache.
2. **Lose the disk's write cache too.** Everything after the last flush may be
   gone, or any prefix of it. We build this in the guest with `dm-log-writes`
   replayed over a thin snapshot to the last flush (or a prefix after it).
3. **The hardware lies.** Flushed data lost or torn. Only real hardware with a
   real power cut shows it.

Step 0 measures model 1 rather than assuming it, since nobody has checked
whether VZ holds writes in its own process before the host sees them.

The invariants each cut must pass (numbered as in #2067's design):

| # | Invariant | Pass means |
|---|---|---|
| I1 | Acknowledged volume writes | `/data/seq` holds 1..N with no gap or duplicate, and N ≥ the highest `ACK` the host saw before the cut |
| I2 | Redis counter | Counter ≥ the last ACKed value; never decreases across cuts |
| I3 | Acknowledged changes | Every change the driver's ledger acknowledged before the cut is in desired state after it |
| I4 | Stores open cleanly | Bun starts first time on every node; no redb or Raft open error, no panic, no crash loop |
| I5 | Raft commit index | Committed index after recovery ≥ the highest read before the cut; leases stay linearizable |
| I6 | Healthy in time | Bun healthy ≤ 120 s after boot; quorum ≤ 120 s after a majority is back; `relish wtf` clean ≤ 300 s after the last node |
| I7 | Desired = running, no duplicates | Every desired app has exactly its replica count running; a singleton never runs twice |
| I8 | Jobs exactly once | Every accepted job id finishes exactly once |
| I9 | No orphans | Containers, mounts, cgroups, netns and eBPF pins match Bun's instances |
| I10 | Disk bounded | Growth slope over the last 200 cuts below a threshold; free space never below 15% |
| I11 | Agent loop healthy | No turn over one second in the 10 minutes after recovery |
| I12 | Logs and metrics | Queries work; log lines lost per cut stated and within one flush interval |
| I13 | Certificates and identity | Workload identity still issues; no rotation stuck |
| I14 | Appliance | After a cut during an update the node boots on old or new, or falls back by itself within 10 min |

Recovery is power-on and nothing else. After a cut the harness may run
`limactl start` (with the one known VZ retry) and read-only commands; the
checker uses a read-only token, so its role, not our good intentions, stops
it fixing anything.

## Sizing

### Topologies

Quickstart VMs are already 2 vCPU / 2 GiB
([`provision.rs`](../../src/relish/quickstart/provision.rs)), a Wyse 3040's
memory. We use them with an 8 GiB disk to match small eMMC (D-4):

| Cluster | Nodes | Memory | vCPUs | Disk |
|---|---|---|---|---|
| `pcut-1` | 1 | 2 GiB | 2 | 8 GiB (+1 GiB log disk for the write-cache cut) |
| `pcut-3` | 3 | 6 GiB | 6 | 3 × 9 GiB |
| `pcut-5` | 5 | 10 GiB | 10 | 5 × 9 GiB |
| All three | 9 | 18 GiB guest, about 22 GiB with VZ overhead | 18 (overcommitted) | ~80 GiB |

Steps 0 and 1 use `pcut-1` only. Running all three at once on the Studio
needs more than 16 GiB of VM memory, so it waits for the maintainer's say-so
(or for D-1's box).

### Cut rate

A cycle is work for a random 2–10 minutes, a cut, power-on, recovery and
verification (about 3 minutes on 2 vCPU). About 9 minutes per cut, so ~160
cuts a day per cluster.

| Tier | Duration | Work between cuts | Cuts per topology |
|---|---|---|---|
| Quick | ~1 h | 2–10 min (1-node: 1–3 min once the driver exists) | ~6–12 |
| Day | 24 h | 2–10 min | ~160 |
| Week | 7 days | 2–10 min | ~1,100 |
| Continuous | until stopped | 2–10 min | rolling |

On the Studio the run pauses for every quiet-host window, so a "day" there is
nearer 12 hours of cutting (option A below).

### What "0 violations in N cuts" bounds

With no failure in *n* independent trials, the 95% upper bound on the per-trial
failure rate is 1 − 0.05^(1/*n*), about 3/*n* (the
[rule of three](https://en.wikipedia.org/wiki/Rule_of_three_(statistics))).

| Cuts, 0 violations | 95% bound per cut | 99% bound |
|---|---|---|
| 25 (step 5 today) | 12% | 18% |
| 300 | 1% | 1.5% |
| 1,000 | 0.3% | 0.46% |
| 3,000 | 0.1% | 0.15% |
| 10,000 | 0.03% | 0.046% |

A site that loses power weekly sees 52 cuts a year; at a true rate of 0.1%,
one bad recovery a year is about 5% likely. The bound doesn't cover
dependence between cuts on one cluster (state accumulates; we restart each
topology every few days and report per-run counts), rare windows a random cut
almost never hits (timed cuts and the enumeration lane cover those), model 3
(only hardware), or per-operation rates (timed cuts split the count nine ways).

### The Studio and the gate

Memory fits (~22 GiB for all three beside the agents' ~75 GB), but nine VMs
booting every few minutes would fake slow turns in a soak, as background builds
did in the A/B run ([#1587](https://github.com/reliaburger/reliaburger/issues/1587)).
So, until D-1 is settled, the run lives on the Studio under **option A**: a
host-wide quiet lock (`$HOME/.reliaburger/quiet.lock`) that the gate,
`nightly-qualify.sh` and the soak scripts take. The endurance harness checks
it before every cut; when it's held, the harness finishes the cycle, verifies,
powers every VM off (itself a cut) and waits. On release it powers on and
verifies that long-outage cut.

Step 1 checks the lock and the gate's `chain.log` before starting and before
each cut, and stops cleanly if either says a gate is running; the full pause
protocol is step 5.

## Harness shape

### The script

```sh
scripts/release/qualify-power-cut-endurance.sh \
  --tier quick|day|week|continuous \
  --release 0.2.8 | --base-url URL --qualified-digest SHA \
  --topologies 1,3,5 \
  --evidence DIR [--seed N] [--write-cache-share 10] [--timed-share 20]
```

- **What runs:** a published release installed by the official `curl | sh`
  into a private `RELIABURGER_HOME`, so the evidence is about the bytes users
  get; or a staged train build checked against its digest, as the soak and the
  nightly do (D-8).
- **Layout:** `scripts/release/power_cut/` holds the endurance pieces
  (`workloads.toml`, `check.py` for the invariants and the summary,
  `calibrate.sh` and its guest writer, later `guest-watcher.sh` and the replay
  unit). The soak's `power_off`, `power_on` and `settle` move into
  `scripts/release/sustained/power.sh`, sourced by both scripts, with hooks
  for what each needs around them.
- **Safety rails** copied from step 5: never touches `reliaburger-test` or
  `~/.reliaburger`; refuses a home it didn't make; keeps every VM off and
  untouched on a violation.
- **Resumable:** the seed position, counts and ledgers live in the evidence
  directory, so a host reboot or a pause resumes the same run.
- **Tests** in `make test-ci-scripts`, against a fake `limactl` and a fake
  `relish` as `test_power_cut_reboot.py` does: the schedule from a seed, the
  cut kinds, the pause protocol, the "no write command after a cut" rule, the
  summary arithmetic and bounds, refusal paths.

### Evidence and records

```
EVIDENCE/
  run.json         seed, release, host, Lima, topologies, tier
  cuts.jsonl       one line per cut: id, topology, kind, operation, timing,
                   dropped writes, recovery times, invariant verdicts
  ledgers/         host-side ACK captures and the change ledger
  cuts/NNNNN/      per cut: before and after snapshots, guest journals, checker
                   output (the last 200 passes kept; every failure kept)
  disk.csv         I10 samples
  summary.json     reliaburger.power-cut-endurance/1
```

The summary JSON carries the release, commit and binary digest, the host, the
start, end and paused seconds, the seed, and per topology the cut count,
violations, the 95% bound, counts by kind, dropped writes and recovery-time
percentiles, then the pooled count and bound, environment events (VZ start
retries) and a verdict. Records go in `docs/qualification/` as
`DATE-power-cut-endurance-vX.Y.Z.md` plus the JSON.

### Docs touch points

As each step lands: `docs/testing.md` (a "Power-cut endurance" section after
"Pulling the plug"), `docs/releasing.md` (the quiet lock),
`docs/design/test-harness.md` (the new scripts), the book's
[chapter 15](../book/15-ready-for-production.md) ("Pulling the plug a thousand
times"), and with the first published week a manual chapter "Running at the
edge", a section in the agent guide (which still says "zero failures in 25
power cuts"), `features.toml`, the website, `README.md`, `docs/README.md` and
the roadmap.

## Implementation steps

Each step is one PR into `main`, tests first. Steps 0–3 give a usable endurance
run; 4–7 make it strong.

| Step | What | Tests first | Done when |
|---|---|---|---|
| **0. Calibrate the cut** | `scripts/release/power_cut/calibrate.sh` and a guest writer: on Lima VZ, writes with and without `fsync`, then `limactl stop -f`, a SIGKILL of the VZ process and in-guest `sysrq b`, 50 cuts each; whether VZ holds writes in its own process; whether `sysrq b` reboots the guest or stops the VM; `write_cache` in the guest; `dm-log-writes` replay to a mark in the Ubuntu 26.04 guest kernel | The tally and verdict logic against recorded fixtures | A record in `docs/qualification/` saying which fault model each mechanism gives |
| **1. Harness skeleton, 1 node, kills only** | `sustained/power.sh` shared with the soak (soak unchanged); `qualify-power-cut-endurance.sh` creating a 1-node cluster with the soak's writer and Redis, whole-site kills at random 2–10 minute intervals, I1, I2, I4, I6 and I7 after each cut, resumable state and the summary JSON | `test_power_cut_endurance.py`: the schedule from a seed, the invariant checks on good and bad fixtures, summary arithmetic and bounds, refusal paths, the quiet-host check | A 1-hour quick run on the Studio passes and writes a summary |
| **2. Three and five nodes; full invariants; driver** | Multi-topology (fix quickstart if it can't run three clusters side by side); the operation driver and change ledger; I3, I5, I8–I13; the read-only checker token; staggered returns and brownouts; charts | A unit test per new checker subcommand on good and bad fixtures | A day run on 1, 3 and 5 nodes passes (needs the maintainer's yes on memory) |
| **3. Timed cuts** | `guest-watcher.sh` firing `sysrq b` 0–500 ms into a named operation (Raft snapshot, backup, image unpack, volume snapshot, rollout, job ledger write, self-upgrade) | Watcher logic on a fake file system tree; the checker counts each operation's minimum | Every operation reached its minimum in a day run |
| **4. Write-cache cut** | Extra Lima disks, an LVM thin pool, `dm-log-writes` under Bun's data, a boot unit that replays to the last flush (or a prefix after it) over the epoch's snapshot | A gated test that writes, marks, flushes, writes more, replays and finds exactly the flushed data | 100 write-cache cuts pass on 1 node, or a bug is found and fixed with its own failing test |
| **5. Quiet lock and continuous mode** | The lock taken by the gate, `nightly-qualify.sh` and the soak scripts; the pause protocol; `--tier continuous` with rolling records | Lock protocol tests in each script's Python tests; the pause path in the fake-limactl tests | The run pauses under a nightly, resumes, and verifies the long-outage cut |
| **6. Crash-state enumeration lane** | `qualify-crash-states.sh`: one operation per store recorded under `dm-log-writes`, every flush point and sampled prefixes replayed, each opened and checked | Unit tests for the replay-point generator | Every store passes; joins step 5 of the gate as a sixth lane (D-10) |
| **7. Appliance lane and hardware tier** | On the 0.3.x train: cuts during `systemd-sysupdate`, bless and counted boots (KVM); a smart-plug driver if D-2 says yes | Fake-plug and fake-QEMU tests | A Linux/KVM appliance run and, if D-2 is yes, a Wyse run, each with a record |
| **8. Docs and publication** | Manual chapter, agent guide section, testing.md, `features.toml`, website, book | `doc_snippets.rs`; `test_features_page.py` | The first week-long record on a published release is linked from the website and `relish agent guide` |

Each bug the run finds gets its own issue, a failing test at the lowest level
that reproduces it, and a fix, as `tests/power_cut.rs` did for the exporter.

## Decisions

| | Decision | Status |
|---|---|---|
| **D-1** | Where the continuous run lives: a dedicated Linux/KVM mini PC (~£300), which also hosts the appliance lane, or the Studio pausing under the quiet lock | **Open.** Until it's settled the run is on the Studio under the quiet lock |
| **D-2** | A hardware tier: smart plugs on the Wyse 3040 lab, ~£60 for three | **Open.** A later step (7) either way |
| **D-3** | Recovery bounds: Bun healthy ≤ 120 s after boot; quorum ≤ 120 s after a majority is back; `relish wtf` clean ≤ 300 s after the last node. Measure for a week, then fix them at the worst seen plus a margin | Provisional (maintainer, 17 Nov) |
| **D-4** | VM shape: quickstart's 2 vCPU / 2 GiB with an 8 GiB disk; a 1 GiB variant later if we claim to fit there | Provisional (maintainer, 17 Nov) |
| **D-5** | The public claim: "≥ 1,000 cuts per topology, ≥ 300 with unflushed writes dropped, 0 violations" per release, with per-topology and pooled bounds, refreshed continuously | Provisional (maintainer, 17 Nov) |
| **D-6** | Script name: `qualify-power-cut-endurance.sh` | Provisional (maintainer, 17 Nov) |
| **D-7** | Logs and metrics after a cut: allow and state loss up to one flush interval; zero corruption | Provisional (maintainer, 17 Nov) |
| **D-8** | Binary under test: published releases for the public record; the train tip in a nightly-style run | Provisional (maintainer, 17 Nov) |
| **D-9** | `relish exec` is a read, not intervention; but the checker reads guest files as root so the token stays strictly read-only | Provisional (maintainer, 17 Nov) |
| **D-10** | Gate membership: the enumeration lane joins step 5; the endurance run stays outside the gate, and `freeze-check` notes the newest endurance record | Provisional (maintainer, 17 Nov) |
| **D-11** | Which release ships it: harness steps 0–5 alongside 0.3.x on `main` (the issue sits in the 0.3.1 milestone); the appliance lane with the appliance | Provisional (maintainer, 17 Nov) |

## Sources

- The design draft behind this plan, 17 November 2026 (prior art, fault
  models, invariants): summarised in #2067
- Linux `dm-log-writes`: https://docs.kernel.org/admin-guide/device-mapper/log-writes.html
- Linux magic SysRq: https://docs.kernel.org/admin-guide/sysrq.html
- Lima VZ driver disk attachment: https://github.com/lima-vm/lima/blob/master/pkg/driver/vz/vm_darwin.go
- Lima forced stop: https://github.com/lima-vm/lima/blob/master/pkg/instance/stop.go
- Apple `VZDiskImageSynchronizationMode`: https://developer.apple.com/documentation/virtualization/vzdiskimagesynchronizationmode
- CrashMonkey and ACE, OSDI 2018: https://www.usenix.org/conference/osdi18/presentation/mohan
- Zheng et al., "Torturing Databases for Fun and Profit", OSDI 2014: https://www.usenix.org/conference/osdi14/technical-sessions/presentation/zheng_mai
- LazyFS and "When Amnesia Strikes", VLDB 2024: https://www.vldb.org/pvldb/vol17/p3017-ramos.pdf
- Rule of three (statistics): https://en.wikipedia.org/wiki/Rule_of_three_(statistics)

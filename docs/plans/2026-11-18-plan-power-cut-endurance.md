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
17 November, and decided D-1 and D-2 the same day: for the next couple of
months the continuous run uses spare capacity on the Mac Studio, pausing
whenever something more important runs, and after that a hardware tier cuts
real power to three Wyse 3040s on smart plugs.

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
   An in-guest `sysrq b` gives this on our hosts, and a hypervisor kill
   should, though step 0 couldn't yet show it: the guest's page cache dies, but
   what the VZ process handed to the host kernel sits in the host's page cache,
   which never loses power. That's a disk with no volatile write cache.
2. **Lose the disk's write cache too.** Everything after the last flush may be
   gone, or any prefix of it. We build this in the guest with `dm-log-writes`:
   the log copied at the cut and replayed over a thin snapshot (step 4).
3. **The hardware lies.** Flushed data lost or torn. Only real hardware with a
   real power cut shows it.

Step 0 set out to measure model 1 rather than assume it, since nobody had
checked whether VZ holds writes in its own process before the host sees
them. On Lima VZ, `limactl stop --force`, a `SIGKILL` of the VZ process and
an in-guest `sysrq b` each lost the guest's page cache and kept every write a
flush covered, in 50 cuts each
([calibration record](../qualification/2026-11-18-power-cut-calibration.md)).
`sysrq b` reboots the guest in place; the VM keeps running, so it's model 1.
For a stop or a kill, whether VZ keeps completed but unflushed writes is
still open: the first run's unflushed records nearly always had another
thread's flush after them. `calibrate.sh --raw` tests it and hasn't run yet.

The invariants each cut must pass (numbered as in #2067's design):

| # | Invariant | Pass means |
|---|---|---|
| I1 | Acknowledged volume writes | `/data/seq` holds 1..N with no gap or duplicate, and the writer, started again after the cut, resumed at or past the highest `ACK` the host saw before the cut; a start between two cuts resumed at or past the previous cut's `ACK`. (N ≥ that `ACK` alone proves nothing: by the time the cluster settles, a writer that lost writes has appended past them again, as the step 1 smoke run showed) |
| I2 | Redis counter | Never decreases across cuts, and the client, started again after the cut, found it at or past the last ACKed value before its first `INCR`; a start between two cuts found it at or past the previous cut's value |
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
The run lives on the Studio's spare capacity (D-1), so the pause is the main
path, not a fallback: a host-wide quiet lock (`$HOME/.reliaburger/quiet.lock`) that the gate,
`nightly-qualify.sh` and the soak scripts take. The endurance harness checks
it before every cut; when it's held, the harness finishes the cycle, verifies,
powers every VM off (itself a cut) and waits. On release it powers on and
verifies that long-outage cut like any other: a pause is a cut lasting hours,
with clocks jumping and leases and certificates ageing, and it's counted and
checked as one.

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
| **4. Write-cache cut** | Extra Lima disks, an LVM thin pool, `dm-log-writes` under Bun's data, a boot unit that rebuilds the data volume from the epoch's snapshot. Step 0 found that `dm-log-writes` logs a plain write only once a later flush completes (a FUA write at once), so a copy of the log taken at the cut is already "the disk as of the last flush" (model 2): the unit replays that whole copy, with no search for the last flush. Prefix states come from the batch the last flush logged: replay up to a random entry inside it, a cache that wrote back in order and lost power partway. Step 4's first question is how to take that copy at the cut: the log's super block counts its entries only at a mark or a FUA write, so the log device as a hypervisor kill leaves it may undercount; `logwrites.py --walk` now reads entries past the count until they stop parsing (unit-tested on fixtures, not yet against a live log). `mkfs.ext4` needs `-E lazy_itable_init=0,lazy_journal_init=0`, or background zeroing flushes on its own | A gated test that writes, marks, flushes, writes more, copies the log, replays it and finds exactly the flushed data (step 0's `logwrites-proof.sh` is the shape) | 100 write-cache cuts pass on 1 node, or a bug is found and fixed with its own failing test |
| **5. Quiet lock and continuous mode (the main path, D-1)** | The lock taken by the gate, `nightly-qualify.sh` and the soak scripts; the pause protocol; `--tier continuous` with rolling records | Lock protocol tests in each script's Python tests; the pause path in the fake-limactl tests | The run pauses under a nightly, resumes, and verifies the long-outage cut |
| **6. Crash-state enumeration lane** | `qualify-crash-states.sh`: one operation per store recorded under `dm-log-writes`, every flush point and sampled prefixes replayed, each opened and checked | Unit tests for the replay-point generator | Every store passes; joins step 5 of the gate as a sixth lane (D-10) |
| **7a. Hardware tier (D-2)** | Three of the lab's ten or so Wyse 3040s, each on a smart plug. First find plugs with a local HTTP API and no cloud dependency (Shelly Gen2+ and Tasmota-flashed plugs are the candidates to check). A plug driver (`--hardware fleet.json`); the netboot and claim flow resetting the three units to a clean install between runs; the same loop at one cut every 10–20 minutes, the same invariants, and the same `cuts.jsonl` and summary format, with the ACK ledgers kept on the Mac, not the devices | Fake-plug tests for the driver; the reset flow against a fake netboot server | A week on the three Wyses with a record; the only evidence for flash that lies (model 3) |
| **7b. Appliance lane** | On the 0.3.x train: cuts during `systemd-sysupdate`, bless and counted boots. It needs KVM, and there's no Linux box (D-1): it runs on the Wyse tier's real hardware, a GitHub-hosted runner with `/dev/kvm` (6 h job cap), or waits | Fake-QEMU tests | An appliance run with a record, on whichever host it gets |
| **8. Docs and publication** | Manual chapter, agent guide section, testing.md, `features.toml`, website, book | `doc_snippets.rs`; `test_features_page.py` | The first week-long record on a published release is linked from the website and `relish agent guide` |

Each bug the run finds gets its own issue, a failing test at the lowest level
that reproduces it, and a fix, as `tests/power_cut.rs` did for the exporter.

## Decisions

| | Decision | Status |
|---|---|---|
| **D-1** | Where the continuous run lives | **Decided (maintainer, 17 November 2026):** for the next couple of months, spare capacity on the Mac Studio, pausing whenever something more important runs (release gates, nightlies, soaks). The quiet lock and the pause/resume design (pause = power its VMs off; resume = check the long outage as a cut) is the main path. No dedicated Linux box for now, so the appliance lane needs another answer for KVM, or waits for D-2 |
| **D-2** | A hardware tier with real power cuts | **Decided (maintainer, 17 November 2026):** after the Studio period, smart plugs and 3 of the lab's ~10 Wyse 3040s dedicated to it. Planned as step 7a: plug control (plugs with a local API and no cloud), the netboot and claim flow to reset the lab between runs, the same invariants and evidence format |
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

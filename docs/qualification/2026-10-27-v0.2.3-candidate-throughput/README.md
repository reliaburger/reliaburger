# Host-job throughput of the 0.2.3 candidate (26 Oct 2026)

Steps 2 and 3 of the 0.2.3 throughput goal, measured on the staged release
candidate rather than on builds made for the purpose:

- [#1030](https://github.com/reliaburger/reliaburger/issues/1030): at least
  8,000 host jobs a second on one 4-vCPU node, as a five-round median, with no
  terminal failures and cleanup verified.
- [#1031](https://github.com/reliaburger/reliaburger/issues/1031): throughput
  on one, two and three nodes, broken down per node. No node may run more than
  20% over its fair share. The three-node total must reach 23,148/s (two
  billion a day), with 11,574/s (one billion) as the floor.

**#1030: met. 14,916.7/s (14,400.0–14,933.3) on one node, up from 7,500.0/s in
the [26 October record](../2026-10-26-host-job-throughput/README.md) and
5,100.0/s in v0.2.2.** The unchanged release script, which runs the image's
BusyBox, measures 9,650.0/s (9,183.3–11,450.0). Both are over 8,000/s.

**#1031: met. Three nodes run 29,350.0/s (28,416.7–30,766.7), every window
balanced.** That is 27% over the two-billion target. Two nodes already run
27,400.0/s. Three nodes don't scale as well as two, but the evidence says that
is this Mac, not the scheduler: three 4-vCPU VMs fill its twelve cores.

Every window in both claims completed, with no terminal failures or retries
in host jobs, and cleanup was verified in each.

The candidate is the one `stage.yml` published:

| Item | Value |
|---|---|
| Commit | [`d387d569`](https://github.com/reliaburger/reliaburger/commit/d387d569bb1f18264382ffcffebe3bbf45fb7ed3) |
| Build run | [40376399802](https://github.com/reliaburger/reliaburger/actions/runs/40376399802), attempt 1 |
| Qualification digest | `64c0fe5b06b61d25184ab44267a72ac2b0ad9195eb564112a39f409e3c9c5769`, the SHA-256 of `candidate.json` |
| Staged release | `staging-v0.2.3-40376399802-1` |
| `bun-linux-aarch64` | `3d15e567…f4ba`, listed in `candidate.json` and `SHA256SUMS` |
| `relish-linux-aarch64` | `71d2e3cc…a582`, listed in both |
| `relish-macos-aarch64` | listed in `candidate.json`; it ran `relish dev create` on the Mac |
| Baseline | the v0.2.2 release's `bun-linux-aarch64` `c002ec75…6bb0` and `relish-linux-aarch64` `eb641079…504a`, checked against its `SHA256SUMS` |

No binary was compiled for this record. The Mac ran no compiler at any point.

## The rig

| Item | Value |
|---|---|
| Host | Apple M2 Max (Mac14,5), 12 cores, 32 GiB, macOS 26.3.1, Lima 2.1.0 (VZ). The same Mac as the 26 October record |
| One-node VM (#1030) | `rb-bench-1030`, the 26 October record's VM: 4 vCPU, 8 GiB, Ubuntu 24.04 aarch64, Linux 6.8.0-147-generic, runc 1.4.0. Rebooted before every session |
| Scale-out VMs (#1031) | `reliaburger-scale-1` to `-3`: 4 vCPU and 6 GiB each, from `relish dev create`, on Lima's `user-v2` network, Linux 6.8.0-147-generic, runc 1.4.0. All three run on the same Mac |
| BusyBox, cap sessions and scale-out | Ubuntu's `busybox-static`, SHA-256 `52151e7f…e933`, the same file as the 26 October record and the profile |
| BusyBox, release script | the pinned image's `/bin/busybox`, `f1947045…c0a5`, as before |
| Scenario | `relish bench` defaults: 60 s windows, concurrency 27 per node, 25m CPU request, one-core limit, 32 MiB, 1,000-task chunks, beside a 500m BusyBox web app |

I reused the 26 October record's VM rather than making a fresh one, so that
this record compares directly with that one: same kernel, same packages, same
BusyBox at the same path. Every session still began on a fresh boot with an
empty `/var/lib/reliaburger`. I disabled the guest's `dpkg-db-backup`,
`logrotate` and `sysstat` timers up front (apt's were already off since the
26 October run), so nothing in the guest ran on a schedule.

### Host load

[`host-load.log`](host-load.log) samples the Mac every 10 s from 20:42 to
22:27 UTC: the 1-minute load, the number of `rustc` and `cargo` processes, the
CPU of the Lima VMs (100% is one core) and the busiest other process.

- **No compiler ran on the Mac**: `rustc=0 cargo=0` in all 624 samples.
- The Mac was in use. Chrome, Activity Monitor and Photos' media analysis
  took up to 1.7 cores at times during the single-node sessions, and Chrome
  took up to 2.5 cores at one point during the two-node block. The windows
  barely moved: the candidate's five host windows span 14,400.0–14,933.3/s, and
  the five two-node windows span 27,133.3–27,550.0/s.
- An old `debug/bun __process-owner` test fixture from another session was
  still running, at about 2% of a core, as in the 26 October record.
- The strict "discard above load 5" rule would discard 3 of 11 windows in the
  candidate's cap session, 2 of 11 in v0.2.2's and 13 of 20 in the release
  script, as in earlier records, because a busy 4-vCPU VM adds about 4 to the
  load. I kept them, for the same reason as before. The three-node windows are
  a different matter: see [What limits three nodes](#what-limits-three-nodes).

## #1030: one node

### Cap 27, with CPU accounting

[`tools/run023.sh`](tools/run023.sh)` caps cand|022`, which runs the 26 October
record's [`session.sh`](../2026-10-26-host-job-throughput/tools/session.sh):
one probe window, then five rounds of host and raw windows, alternating order.
The per-window table is in each directory's `summary.txt`.

| Build | Host jobs, median (range) | Steady 15–58 s | VM CPU busy | CPU per process | Raw, median (range) | Bun CPU in a raw window |
|---|---:|---:|---:|---:|---:|---:|
| v0.2.2 | 5,100.0/s (5,100.0–5,100.0) | 5,161/s | 60% | 0.453 ms | 23,347.3/s (23,254.5–23,439.9) | 60.7 s of 62 |
| 26 Oct, `259b6061` | 7,500.0/s (7,483.3–7,500.0) | 7,921/s | 39% | 0.204 ms | 29,604.6/s (29,540.7–29,682.4) | 0.5 s |
| **Candidate `d387d569`** | **14,916.7/s (14,400.0–14,933.3)** | **15,831/s** | 72% | 0.194 ms | 29,606.2/s (29,118.0–29,627.2) | 0.5 s |

The 26 October row is that record's, from builds made in the VM, so read it
as context. The v0.2.2 and candidate rows are the released binaries, measured
here tonight. v0.2.2 reproduces the earlier baseline exactly: 5,100.0/s in
every window, and the GitOps loop still burns a core with no tasks to run.

The candidate's per-node breakdown (#1362) shows one node with all the
successes in every window, as it should on one node, and `balanced`.

### The node still waits for grants, but half as often

The probe window ([`caps-cand/c27-probe-host.timeline.txt`](caps-cand/c27-probe-host.timeline.txt))
shows what the cap change bought. The node holds 32,000 tasks, 32 chunks, where
v0.2.2 holds 16,000 ([`caps-022/c27-probe-host.timeline.txt`](caps-022/c27-probe-host.timeline.txt)).
Accepted successes still move in steps, one per leader tick, about 1 s apart:

```
   t(s)   held  accepted  started  started/s  active
   0.63  32000     96000    98001     108835       0
   1.58  32000    113000   113000      95255       0
   2.69  32000    128000   128000      95110       0
   3.64  32000    145000   145000     108557       0
```

Each tick accepts 15,000 to 17,000 tasks. The two-tick round trip from the
26 October record is still there, so the ceiling is now 32 chunks × 1,000
tasks ÷ 2 s = **16,000/s**, and the steady rate, 15,831/s, is 99% of it. The
VM is 72% busy, up from 39%. One more doubling would meet the CPU. Sending
grants in the tick that commits them
([#1360](https://github.com/reliaburger/reliaburger/issues/1360), 0.2.4) would
lift the same ceiling without the node holding more work.

### The unchanged release script

[`scripts/release/job_bench_rounds.sh`](../../../scripts/release/job_bench_rounds.sh)
`--rounds 5` on the candidate, at the candidate's own commit, with the pinned
image's BusyBox:

| Scenario | 26 Oct, `259b6061`, median (range) | Candidate, median (range) |
|---|---:|---:|
| Raw VM processes | 16,912.2/s (13,711.2–17,484.6) | 14,872.4/s (12,993.5–17,160.6) |
| Fresh containers | 13.4/s (13.2–13.6) | 13.4/s (13.3–13.5) |
| Shared containers | 6,483.3/s (6,266.7–6,683.3) | 6,550.0/s (5,983.3–7,366.7) |
| Host jobs | 7,500.0/s (7,383.3–7,500.0) | **9,650.0/s (9,183.3–11,450.0)** |

Every window completed with no terminal failures and cleanup verified. One
fresh-container window accepted a retry, which later succeeded. The other
scenarios had none.

Host jobs run slower here than in the cap sessions, 9,650.0/s against
14,916.7/s, and that is the BusyBox, not the script. The image's BusyBox costs
about twice as much per process as Ubuntu's: raw processes run 14,872.4/s with
it and 29,606.2/s with Ubuntu's, on the same VM. On 26 October grant cycles
capped both sessions at 7,500.0/s, so the difference didn't show. Now the
release script's host jobs are bound by CPU, and they follow the raw rate.

The raw rate falls through the session again, from 17,160.6/s in round 1 to
about 13,000/s in rounds 4 and 5, as it did in both builds on 26 October. The
release script records no CPU accounting, so this record still can't say
what grows.

## #1031: one, two and three nodes

### How the cluster was built

`relish dev create scale --nodes 3 --cpus 4 --memory 6GiB` with the
candidate's macOS `relish`, installing the candidate's Linux `bun` and
`relish` on three VMs. It hung after starting Bun on the first node: the
`limactl shell` that launches it never returned. Bun was running and the
council had formed, so I stopped the command and did its remaining steps by
hand, the same way it does them: a join token per node from the first node,
`relish join` with the root CA's fingerprint, then Bun on each. The council
then had three voters with quorum.

[`tools/scale.sh`](tools/scale.sh) then gave each node the bench's node
settings (the BusyBox in `[process_workloads] allowed_binaries`,
`mount_isolation = false`, eBPF on) and restarted its Bun. To measure fewer
nodes, it names a different binary in the other nodes' allowlists. Those
nodes answer every array with zero slots, so the leader grants them nothing,
and the council stays three voters throughout. The 500m web app ran on the
cluster during every window. Each window is `relish bench --scenario
jobs-host-processes --concurrency 27` from the first node, five windows per
node count, with the council's state and each node's CPU recorded either side
([`tools/scale_analyse.py`](tools/scale_analyse.py)).

### Rates

| Eligible nodes | Total, median (range) | Per node, median of each window | Against one node | Every window balanced | Failures and retries |
|---|---:|---:|---:|---|---|
| 1 | 14,566.7/s (14,083.3–14,666.7) | 14,567/s | 1.00× | yes | none |
| 2 | 27,400.0/s (27,133.3–27,550.0) | 13,533 and 13,959/s | 1.88× | yes | none |
| 3 | **29,350.0/s (28,416.7–30,766.7)** | 9,671, 10,159 and 9,649/s | 2.01× | yes | none |

The per-node rates come from [#1362](https://github.com/reliaburger/reliaburger/pull/1362)'s
breakdown in each report. No node took more than 34.8% of the work on three
nodes, against a fair share of 33.3% and a limit of 40%. On two nodes the
largest share was 50.8%. The one-node figure matches #1030's cap sessions on
the bench VM (14,916.7/s) within 2.4%, on different VMs.

### The control plane isn't the limit

| Eligible nodes | Raft entries a second, median | Leader | Leader's Bun CPU | Leader's memory |
|---|---:|---|---:|---:|
| 1 | 1.0 | `scale-1`, also running jobs | 0.62 cores | 125 MiB |
| 2 | 1.0 | `scale-3`, running no jobs | **0.02 cores** | 96 MiB |
| 3 | 1.0 | `scale-1`, also running jobs | 0.64 cores | 125 MiB |

The leader commits one `Tick` entry a second, however many nodes run jobs and
however many tasks they finish. Leadership moved to `scale-3` when the
two-node block restarted the other nodes, which turned out useful: a leader
that runs no jobs spends 0.02 cores leading two nodes at 27,400 tasks a
second. Each node running jobs spends about 0.65 cores of Bun, leader or not.

The issue asks for chunk acknowledgement latency too. The probe window on
three nodes ([`scale/n3-probe-host.timeline.txt`](scale/n3-probe-host.timeline.txt))
shows how acknowledgement works rather than a latency distribution. Every node's
accepted count moves once per leader tick, about 1.0 s apart, all three in
the same tick, with no node a tick behind:

```
   t(s)  node 1 accepted (step)  node 2 accepted (step)  node 3 accepted (step)
   0.84       90855 (+10174)      108164 (+11748)       99688 (+11080)
   1.84      101354 (+10499)      120041 (+11877)      110397 (+10709)
   2.85      111918 (+10564)      131689 (+11648)      121220 (+10823)
```

So a finished chunk is acknowledged at the next tick, at most a second later,
on three nodes as on one.

### What limits three nodes

The Mac does. Two nodes run 94% of the one-node rate each, and three run 67%.

- Each VM was 85–89% busy in the three-node windows, against 76–77% for one
  node alone.
- The Lima VMs used 717% of the Mac's 1,200% on average across the
  three-node block, peaking at 1,105%, against 530% for two nodes and 227% for
  one.
- The Mac's 1-minute load reached 37 in the three-node windows, and 8 in the
  two-node ones.

Twelve vCPUs on twelve host cores leave nothing for the host itself, so each
VM gets less than its four cores. The scheduler spreads the work evenly, and
the leader is idle. On real machines, or VMs that don't share a host, three
nodes should come closer to three times one node. On this rig 29,350/s is a
lower bound, and it still clears two billion a day by 27%.

For the day run (#1032), which needs 23,149/s for 24 hours, this rig at three
4-vCPU nodes has that 27% of headroom over the steady requirement. **The day
tier doesn't run on this rig, though.** `qualify-jobs-sustained.sh --tier day`
builds on the soak's three-node quickstart cluster, and quickstart VMs have 2
vCPU and 2 GiB each (`src/relish/quickstart/provision.rs`): half the CPU
measured here. Before the day run starts, either measure a quickstart cluster
the same way or give the day tier 4-vCPU nodes, as #1031 asks the maintainer
to decide.

## Discarded and repeated runs

The first three-node probe window is kept as `scale/discarded-n1-probe-host.*`
but not used. Its script failed to load the function that reconfigures the
nodes, so only node 1 was eligible and the window measured one node
(14,366.7/s). The repeated window, with all three eligible, is the one
above. Nothing else was discarded or repeated.

## Commands

```sh
# Download and verify (any directory)
gh release download staging-v0.2.3-40376399802-1 -p bun-linux-aarch64 -p relish-linux-aarch64 \
    -p relish-macos-aarch64 -p candidate.json -p SHA256SUMS
shasum -a 256 candidate.json        # must print the qualification digest
grep linux-aarch64 SHA256SUMS | shasum -a 256 -c -

# #1030, inside rb-bench-1030 as root, one per fresh boot
/home/miko.guest/run023.sh caps cand
/home/miko.guest/run023.sh caps 022
/home/miko.guest/run023.sh rounds cand

# #1031, on the Mac
relish dev create scale --nodes 3 --cpus 4 --memory 6GiB --bun bun-linux-aarch64 --relish relish-linux-aarch64
tools/scale.sh scale          # 3, 2 and 1 eligible nodes, five windows each
tools/probe3.sh               # one three-node probe window
tools/scale_analyse.py scale > scale/summary.txt
tools/probe_nodes.py scale/n3-probe-host.probe.jsonl > scale/n3-probe-host.timeline.txt
```

[`tools/single.sh`](tools/single.sh) reboots the VM before each of the three
single-node sessions, and [`tools/hostmon.sh`](tools/hostmon.sh) writes
`host-load.log`. The 26 October record's `analyse.py`, `probe.py` and
`loadcheck.py` produced the summaries, timelines and load checks.

## Files

| Path | What |
|---|---|
| `caps-cand/`, `caps-022/` | Cap-27 sessions: each window's report, `vmstat`, CPU snapshots and status; `summary.txt`; the probe timeline |
| `rounds-cand/` | The release script's five rounds on the candidate |
| `scale/` | Each scale-out window's report, council state and per-node CPU either side; `summary.txt`; the three-node probe timeline; each node's environment and Bun log |
| `host-load.log` | The Mac, every 10 s |
| `tools/` | The scripts above |

The raw probe samples aren't kept, only the timelines made from them, as in
the 26 October record. Bun's logs from the scale-out show the expected
refused connections while nodes restarted between blocks, and no panics.

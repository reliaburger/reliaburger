# Hollow nodes: a real council in front of 100, 500 and 1,000 agents (28 October 2026)

First record of the hollow-node harness ([#1401](https://github.com/reliaburger/reliaburger/issues/1401),
plan: `docs/plans/2026-10-28-research-kubernetes-release-testing.md`, item 4).
The numbers are measurements, not a gate. Nothing here changes a published
claim; the [claims section](#what-the-measurements-say-about-our-claims) lists
the ones the numbers bear on, for the maintainer to decide.

## Setup

- **Machine:** Marek's MacBook Pro, Apple M1 Max, 10 cores, 64 GB, macOS 15
  (Darwin 24.6.0). Shared with other agents' builds the whole time: the
  one-minute load average before each run was between 3 and 25, and the
  harness would have stopped at 40 (it never did).
- **Code:** branch `0-2-4/hollow-nodes` at `04399820` (on `release-0-2-4` at
  `9944c22b`), built with the `hollow` Cargo profile (release optimisation,
  16 codegen units).
- **Council:** three fully wired members (gossip, Raft, reporting, HTTP API,
  leader scheduler, placement reconciler, an agent each) in one process,
  plaintext transports, council held at three voters with
  `council_size_cap`. Bun's defaults otherwise.
- **Fleet:** N hollow nodes in a second process (tokio, 10 worker threads):
  real gossip, Raft endpoint, reporting worker (every 5 s, Bun's default),
  `BunAgent` and placement reconciler; a runtime that runs nothing. Each
  reports 8,000 millicores and 16 GiB and carries `role=hollow`. Started 25 at
  a time, all seeded with council member 0.
- **Load:** one app with a replica per hollow node, then 20 two-replica apps
  applied one after another, all `required = ["role=hollow"]`; 60 s steady;
  then 10% of the fleet loses all traffic at once (`NodeTransportGate`, no
  gossip goodbye) and shuts down, and the same nodes start again.
- **Waits:** each step waits at most 60 s + 0.3 s per node (90 s at 100,
  210 s at 500, 360 s at 1,000). "Not reached" means not within that.

Command: `scripts/release/qualify-hollow-nodes.sh --sizes 100,500` and then
`--sizes 1000`, one size at a time. JSON reports:
[`2026-10-28-hollow-nodes/`](2026-10-28-hollow-nodes/) (a first 1,000-node
run, with the same code minus the peer counts, is `hollow-1000-first-run.json`).

## Results

| | 100 | 500 | 1,000 (run 2) | 1,000 (run 1) |
|---|---|---|---|---|
| Start every hollow node | 5.5 s | 26.8 s | 58.5 s | 57.7 s |
| Council lists every node alive, after the last started | at once | at once | at once | at once |
| Every hollow node sees every member alive | 16.4 s | 139.3 s | not reached (fewest 459, median 650 of 1,002) | not reached (0 nodes complete) |
| Apply of the big app returns | 32 ms | 38 ms | 1.6 s | 3.3 s |
| Big apply → first replica running | 3.9 s | 1.7 s | 4.7 s | 5.8 s |
| Big apply → every replica running | 5.9 s | 12.0 s | 124.3 s | 150.1 s |
| Small applies, p50 / p99 | 32 / 60 ms | 34 / 75 ms | 2,621 / 3,269 ms | 2,690 / 3,366 ms |
| Raft commit (no-op write), p50 / p99, steady | 25 / 45 ms | 24 / 49 ms | 25 / 84 ms | 24 / 71 ms |
| Raft commit p50 / p99, council alone | 31 / 36 ms | 23 / 26 ms | 24 / 31 ms | 30 / 37 ms |
| Council CPU, steady (100% = one core) | 5% | 52% | 58% | 58% |
| Council RSS, steady | 136 MB | 517 MB | 1,000 MB | 1,009 MB |
| Council RSS, alone | 39 MB | 39 MB | 40 MB | 41 MB |
| Fleet CPU, steady | 13% | 309% | 721% | 726% |
| Fleet RSS, steady | 175 MB | 2.4 GB | 7.7 GB | 7.9 GB |
| Nodes stopped / replicas lost | 10 / 30 | 50 / 80 | 100 / 69 | 100 / 368 |
| Council lists none of them alive | 3.6 s | 30.3 s | 73.9 s | 82.4 s |
| Every lost replica running elsewhere | 9.7 s | ≤ 30.3 s | not reached | ≤ 82.4 s |
| Restarted nodes listed alive again | 0.7 s | 9.1 s | not reached | not reached |
| Council voters / hollow voters | 3 / 0 | 3 / 0 | 3 / 0 | 3 / 0 |

"≤" means the replicas were already back when the council stopped listing the
nodes, which the harness checks first. An earlier 500-node run, with the
council's view timed only after gossip's wait and a busier host, saw the
small applies at 117 / 307 ms and the restarted nodes back after 88.7 s.

Signals in the fleet's own log over each whole run:

| | 100 | 500 | 1,000 (run 2) |
|---|---|---|---|
| Report admissions the leader refused | 0 | 15,310 | 879,084 (about 600 a second) |
| Agent loop turns over the 1 s budget | 0 | 298 | 142,589 |
| `retirement ... exceeded` from placement reconcilers | 0 | 0 | 3,611 |

## What we learnt

1. **Raft is not the bottleneck.** The no-op commit stays at 24–25 ms median
   from an idle council to a thousand nodes. The council process's CPU levels
   off near 60% of one core between 500 and 1,000 nodes, and its memory grows
   by about a megabyte per node.
2. **The leader refuses reports from 500 nodes up.** Every node reports
   straight to the leader, whose reporting listener serves 16 connections at
   a time and drops the rest (`MAX_REPORT_CONNECTIONS` in
   `src/reporting/transport.rs`). At 1,000 nodes the refusals ran at about
   600 a second, against a nominal 200 reports a second; refused workers try
   again. Scheduling still converges because a report eventually lands, but
   placement, failure handling and the API all slow with it. This is the
   limit the whitepaper's §8.1 predicts, and it arrives well before 10,000.
3. **API writes slow down about 75-fold between 500 and 1,000 nodes** while the
   Raft commit underneath doesn't move. A thousand placement reconcilers poll
   `/v1/placements` on the leader every 2 s (500 requests a second), beside
   the report traffic.
4. **Failure handling at 1,000 nodes didn't finish.** In run 2 the replicas
   of 100 crashed nodes weren't all running again after 6 minutes, and in
   neither 1,000-node run were the restarted nodes listed alive again within
   6 minutes. At 100 nodes the same steps take 10 s and under 1 s.
5. **A mass join converges slowly.** 16 s for 100 nodes to see each other,
   139 s for 500, incomplete after 6 minutes for 1,000.

### How much of this is the laptop

A thousand agents on ten shared cores aren't a thousand machines. The fleet
process used seven of them, and its agents broke the 1 s loop-turn budget
142,589 times: a starved hollow node answers gossip probes late, gets
suspected, and has to refute it, which slows every gossip number here and the
rejoin most of all. So the gossip and failure-handling times above are upper
bounds for real hardware, not predictions. The report refusals and the API
latency are less forgiving: the council had its own process, never went
above 63% of a core, and the refusals come from a fixed connection limit, not
from CPU. The harness can't yet split the fleet across machines; that's the
next step to separate the two.

Also not real: every hollow node's workload identity request fails with "no
leader transport for workload signing" because the harness runs plaintext
(no mTLS identities), which costs log lines, not placement.

## What the measurements say about our claims

None of these was changed; the coordinator decides.

- **Whitepaper §2, "Maximum nodes per cluster: 10,000. Target (unmeasured)"**
  and **§23's comparison matrix, "Target: 10,000 nodes (unmeasured)"**. Not
  contradicted as worded, but the first measured limits are far below it:
  report admissions refused from 500 nodes, applies at 2.6 s and failure
  handling unfinished at 1,000. Either the target needs these as known
  limits, or each needs an issue.
- **Whitepaper §2 rationale and §8.1 Layer 1: "gossip scales to tens of
  thousands", "O(log N) convergence"**. A 500-node mass join took 139 s to
  converge and a 1,000-node one didn't within 6 minutes. CPU starvation
  explains part of it (above); the claim isn't supported by a measurement at
  any size above 100.
- **Book Chapter 2, "For a 10,000-node cluster, ... 7 seconds for full
  convergence" and "about 42 rounds (21 seconds) for an update to reach every
  node"**. Those are one update's dissemination, not a join, but nothing we
  measured converged anywhere near that at 500 or 1,000.
- **Book Chapter 2, "running on every node (up to 10,000)"**. Unmeasured
  above 1,000; see the first item.
- **Whitepaper §8.1: "it comes before any 10,000-node claim: a leader taking
  a report from every node directly won't get there"**. Confirmed, and
  sharper: the direct-to-leader path refuses reports at 500 nodes.
- **README and website**: no node-count claims found
  (`README.md`, `docs/website/index.html`, `docs/website/compare/*.html`).
- **Not measured here**: leader election time, operability after leader loss
  and cold-start reconstruction (whitepaper §2); the harness never kills a
  council member.

## Not done yet

- API SLOs from [#1400](https://github.com/reliaburger/reliaburger/issues/1400)
  (done in parallel); the harness reports apply latency only.
- Task arrays across the fleet; only apps are deployed.
- Hollow nodes run no HTTP API, so leader-to-node calls (task-array syncs,
  logs, fault injection) aren't exercised.
- A fleet split across machines, to tell host limits from Reliaburger's.
- A seven-voter council (Bun's default above seven nodes); the harness takes
  `RELIABURGER_HOLLOW_COUNCIL` but no run used it.

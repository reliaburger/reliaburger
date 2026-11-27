# Hollow nodes: report admission at 500 and 1,000 nodes (24 November 2026)

The rerun that [#1411](https://github.com/reliaburger/reliaburger/issues/1411)
asked for, before and after its fix. The first record,
[2026-10-28-hollow-nodes.md](2026-10-28-hollow-nodes.md), counted 15,310 report
admissions the leader refused at 500 hollow nodes and about 600 a second at
1,000. These are measurements, not a gate.

## Setup

- **Machine:** the Mac Studio, Apple M5 Max, 18 cores, 128 GB. Shared: twelve
  power-cut endurance runs were going the whole time, and the one-minute load
  average was 11 to 29 during the 500-node runs.
- **Before:** `release-1-3-3` at `f9a8db2a`. **After:** the same plus the
  #1411 fix on branch `0-4-0/report-connections`. Both built with the `hollow`
  Cargo profile.
- **Harness:** unchanged from the first record (three council members in one
  process, the fleet in another, reports every 5 s, 25 nodes started at a
  time, 60 s steady, 10% churn). The "after" runs went through
  `scripts/release/qualify-hollow-nodes.sh --sizes 500,1000`, which now prints
  the refused-admission count from the fleet's log. The "before" run used the
  same environment on a binary built from the base commit.

## The fix

The leader's reporting listener no longer refuses work. It takes a slot before
it accepts a connection (64 slots, not 16), so excess connections wait in the
listen backlog; a report that finds the aggregator's queue full (now 256, not
16) waits for room inside the connection's 10 s deadline. The aggregator stores
every queued report before it publishes its view once, rather than copying the
whole view for every message, and checks reports against gossip's member list
with a hash set rather than a scan per entry.

## Results

| | 500, before | 500, after | 1,000, after |
|---|---|---|---|
| Report admissions the leader refused | 17,223 | 0 | 77,280, all while joining (see below) |
| Lines from the harness's own churn ("reporting is quiesced") | 17 | 6 | 67 |
| Council CPU, steady (100% = one core) | 36% | 26% | not reached |
| Big apply → every replica running | 8.3 s | 10.4 s | not reached |
| Every hollow node sees every member | not reached | 125.1 s | not reached |
| Small applies, p50 / p99 | 43 / 188 ms | 277 / 1,371 ms | not reached |
| 10% crash: lost replicas running again | 22.3 s | 23.1 s | not reached |
| Restarted nodes seen alive again | 86.5 s | 2.2 s | not reached |

Refused admissions in the "before" run were 16,451 "not acknowledged: unexpected
end of file" (the listener closed a connection over its 16), 494 resets and 278
broken pipes on the same path.

The 500-node rows other than the refusals move both ways, by more than the fix
could explain: the council's Raft commit p99 with nobody attached was 61 ms
before and 1,364 ms after, which is the host, not Reliaburger. Read them as
noise from a shared machine. The refusals are the measurement this record is
for.

### 1,000 nodes

The 1,000-node run stopped during the join, when the host's load average went
over the harness's limit of 40 (the fleet alone used 553% CPU). Until then the
fleet logged 76,959 `TCP connect ... Connection reset by peer`: the listen
backlog overflowing. Tokio asks for a backlog of 128, and macOS resets a
connection that finds it full; Linux drops the SYN instead, and the client
retries inside its 5 s connect timeout. The council process
sat at 8% CPU with the backlog full, and the aggregator barely appeared in a
`sample` of it, so the slots weren't waiting on the leader. They were waiting
on clients: a fleet process starved enough to break its agents' 1 s loop
budget writes its frame late after connecting, and each of the 64 slots waits
for it. The fix doesn't cover that case, and a measurement of it needs the
fleet split across machines ([#1413](https://github.com/reliaburger/reliaburger/issues/1413)).

## Reproducing it without the harness

`tests/reporting_tcp.rs`'s
`five_hundred_workers_report_to_one_leader_without_a_refusal` (in
`make test-hollow`) starts 500 real report workers with fake agents against
one leader over real TCP, all at once, with a 500-member gossip list, for 32 s.

| | Sends | Refused | Nodes in the leader's view |
|---|---|---|---|
| Before, without the gossip list | 6,000 | 3,255 | 499 of 500 |
| Before, with it | 6,000 | 4,684 | 451 of 500 |
| After | 6,000 | 0 | 500 of 500 |
| After, 1,000 workers | 12,000 | 0 | 1,000 of 1,000 |

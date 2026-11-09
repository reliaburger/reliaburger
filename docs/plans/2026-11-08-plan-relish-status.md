# `relish status`: the whole cluster at a glance

8 November 2026. A plan for 0.2.8, at the maintainer's request: `relish
status` stops being a list of every instance and becomes one screen that
tells you whether the cluster is all right. The lists move to `relish apps`
and `relish nodes`, which can grow filters and paging for large clusters.

## Why

Today `relish status` prints a council header, then one table row per
instance (`NODE INSTANCE APP NAMESPACE STATE PID RESTARTS`), then free-text
lines for failed, blocked and unschedulable apps. On a three-node quickstart
that's fine. On ten nodes and a hundred apps it's several hundred rows, and
the one line that matters (a voter down, an app that can't be placed) scrolls
off the top. It answers "what's running where?", which is a list question,
and it doesn't answer "is my cluster all right?", which is the question
people actually type it for.

## What it shows

One screen, the same size on three nodes or three hundred. Counts and
verdicts, never one row per thing. Each section ends in the command that
lists the detail.

```
cluster prod: DEGRADED, 2 problems                         relish 0.2.8, 14:02:11 GMT

nodes      12 alive, 1 suspect, 0 dead                     relish nodes
council    leader node-3 (term 41), 5/5 voters, quorum ok, 0 lagging
gossip     13 members, last change 4 m ago, 18,442 updates received
versions   bun 0.2.8 on 12 nodes, 0.2.7 on 1: upgrade in progress   relish upgrade status
capacity   cpu 61% allocated (147 of 240 cores), memory 54%, disk 38%, gpu 2 of 8
usage      cpu 34% (busiest node-7 81%), memory 47%, disk 38%; newest sample 6 s old
apps       104: 99 running, 3 degraded, 1 down, 1 unschedulable    relish apps --unhealthy
instances  412 of 418 running
jobs       12 running, 340 queued, 3 failed in the last hour; 9,120 started/s
logs       newest line 2 s old, slowest node 9 s (node-11)
faults     1 active: latency on api (default), 6 m left             relish fault list
alerts     1 firing: disk_high on node-4                            relish wtf
certs      soonest expiry 61 days (node CA)
gitops     synced at a1b2c3d, 3 m ago
agents     2 leases held, 1 proposal waiting for approval           relish proposal list
wtf        1 critical, 1 warning                                    relish wtf
```

The first line is the verdict: `HEALTHY`, `DEGRADED` or `DOWN`, with the
number of problems behind it, so a glance at one line is enough. Healthy
sections print one quiet line each. A section with a problem is coloured
(on a terminal), and its last column names the command to run next.

### The sections and where their data comes from

| Section | Source today | Gap to fill |
|---|---|---|
| nodes | `/v1/cluster/nodes`: alive, suspect, dead, left | Upgrade cordon (`upgrade.is_node_cordoned`) and disk pressure (≥ 85%, `wtf`'s `disk-high` threshold) as counts |
| council | `council_view::survey_nodes` → leader, voters answering, `quorum_ok`, lagging (> 100 entries) | Nothing; reuse it |
| gossip | `/v1/cluster/nodes`: member count, incarnations | **New:** Mustard counts updates received and stamps the last membership change (both `Instant`s today, unserialised) |
| versions | `/v1/capabilities/cluster` (`BuildFingerprint` per node), `/v1/upgrade/cluster` | Nothing; group by version |
| capacity | Leader-side `reporting::types::ResourceUsage` (CPU, memory, disk, GPU used and total), read by the scheduler | **New:** no `/v1` route serves it. GPU used is hard-coded to 0 in `orchestrate.rs` |
| usage | `/v1/metrics/cluster`: `node_cpu_usage_percent`, `node_memory_*`, `node_disk_*` | **New:** the newest sample's timestamp, as proof the metrics pipeline is live |
| apps, instances | `/v1/status?cluster=true` plus `/v1/diagnostics/apps`, rolled up as `agent context` does (`AppState`, `context.rs:194`) | Move the rollup out of `agent context` so both use one function |
| jobs | `/v1/jobs?cluster=true` | Failed in the last hour and the start rate, if `ClusterJobs` doesn't carry them yet |
| logs | Nothing direct | **New:** each node reports the timestamp of the newest line it ingested; status shows the newest and the slowest node |
| faults | `/v1/fault?cluster=true` | Nothing |
| alerts | `/v1/alerts` | Nothing |
| certs | `/v1/diagnostics` `certificates` | Nothing |
| gitops | `/v1/gitops` | Nothing; one line, hidden when GitOps isn't configured |
| agents | Leases (#1686) and proposals (#1682), in the leader's view | Done: counts, the first few holders and targets, and stale proposals (D5) |
| wtf | Client-side only (`relish/wtf/collect.rs`) | See decision D1 |

### One summary, served by the cluster

The summary is assembled by **a new endpoint, `GET /v1/cluster/summary`**,
not by `relish`. Any node answers it, fanning out with a deadline as
`/v1/metrics/cluster` does. That way:

- `relish status`, `relish agent context`, the MCP `context` tool, the TUI's
  dashboard and Brioche's web dashboard all read one shape instead of five
  hand-rolled ones;
- the 1 s budget for `status` (manual chapter 2) holds on a large cluster,
  because one round trip replaces `relish` fanning out to every node;
- each section carries `source: complete | partial | missing`, the same
  marking #1685's timeline uses, so a node that didn't answer shows as
  "partial (11 of 12 nodes)", never as a silently smaller number.

`relish status -o json` prints the summary inside the agent envelope, with
`next` steps pointing at the section commands. The old instance list isn't
in it; that's `relish apps -o json`.

## `relish apps` and `relish nodes`

- **`relish apps`** (new): one row per app with `NAMESPACE APP STATE
  READY RESTARTS NODES AGE`, from the same rollup. Filters: `--namespace`,
  `--unhealthy` (anything not running and ready), `--node`. `relish apps
  NAME` lists that app's instances: the old status table, for one app.
- **`relish nodes`** (extended): adds `VERSION`, `CPU`, `MEM`, `DISK` (real
  usage) and `ALLOCATED`, and `--unhealthy` for nodes not alive, cordoned
  or under disk pressure.
- Both sort problems first, so the interesting rows are at the top even when
  the list is long. Both take `-o json`.

`relish top` keeps its per-instance usage, unchanged.

## Large clusters

The summary is tested at scale with the hollow-node harness (`make
test-hollow`): 100 nodes and 1,000 apps. It has to render in the same number
of lines as on 3 nodes, and answer within the 1 s budget. A snapshot test
(`insta`) holds the text layout for a healthy cluster, a degraded one, one
with an upgrade in progress and one with a node that didn't answer.

## What else breaks

Changing the output breaks callers, and every one moves in the same PR
that changes it:

- `scripts/demo/tour.sh` (awk on status columns, a grep for `RolloutFailed`),
  `scripts/demo/gitops.sh` and `scripts/demo/lib.sh` move to `relish apps`;
- `scripts/release/qualify-ssh-setup.sh` greps the voter count in the header:
  it reads the council line instead;
- the gym's `cluster.py`, `test_gym.py`, `test_claude_code.py` and
  `score.py` read `status -o json` as an instance list: they read `relish
  apps NAME -o json`;
- the Rust tests that read the table (`missing_binary.rs`,
  `container_runtime_required.rs`, `relish_cli.rs`, `doc_snippets.rs`,
  `documentation_first_run.rs`);
- `wtf`'s advice text that says "run `relish status`";
- the manual (chapters 0–2, 4, 5, 7, 8, 10–12, 14 and 16), the quickstart's
  welcome page, the website's Markdown pages and the CLI design doc. Chapter
  11's claim that status shows each instance's image is wrong today and is
  fixed here;
- the tour is re-recorded at 0.2.8's release, as usual.

The concept budget grows by one command (`apps`) and the new flags, raised
deliberately in the PR that adds them.

## Order

1. **The summary endpoint and its gaps:** `ClusterSummary`, the capacity
   route, Mustard's update counter and last-change time, the newest-log
   timestamp per node, the newest metrics sample. Tests first, then the
   hollow-node scale test.
2. **`relish status` on the summary,** text and JSON, with its snapshots;
   the callers above move with it.
3. **`relish apps` and the wider `relish nodes`.** Can run in parallel with 2
   once 1's types are merged.
4. **The readers:** `agent context`, the TUI dashboard and Brioche read
   the summary; the manual and book chapter 19 (and chapter 11 for the
   observability counts) describe it.

All four go on the 0.2.8 train.

## Decisions

Each has a default, so the build doesn't wait:

- **D1, `wtf` in status.** Default: yes, run `wtf`'s checks in the same call
  with a 2 s budget, and show "not checked in time" past it. `--quick` skips
  them.
- **D2, exit status.** Default: `relish status` exits 0 whenever it got an
  answer, degraded or not, as every agent-facing command does: the verdict is
  in the output. `relish status --check` exits 1 on `DEGRADED` and 2 on
  `DOWN`, for scripts and monitoring.
- **D3, the instance list.** Default: it leaves `status` entirely. `relish
  apps NAME` lists one app's instances, and `relish apps --instances` lists
  all of them for anyone who wants the old view.
- **D4, logs proof.** Default: the newest-line timestamp per node, which
  costs nothing. The alternative, a canary line Bun writes every minute and
  reads back through the log query path, proves more but costs a write per
  node per minute.
- **D5, agents and the verdict.** Decided (9 November 2026): leases held
  and proposals waiting for approval are a note, never a problem. They're
  the system working as designed: an agent at work, and a change waiting
  for the person it's meant to wait for. The line names `relish proposal
  list` while a proposal waits, else `relish lease list`, and the cluster
  stays `HEALTHY`. A **stale** proposal is a problem and makes the cluster
  `DEGRADED`. Stale means approving it would fail, for any of the three
  reasons approval checks: an app it touches changed after it was made (its
  revision no longer matches); someone else's lease covers one of its apps
  (possible only once a person has broken the lease the proposal pinned);
  or it's past its TTL and the reaper hasn't expired it. In the first and
  the last, it also still pins a lease on its apps that keeps everyone else
  off them until someone withdraws or rejects it. The leader works out staleness when it builds its view, so
  every reader sees the same answer. The section never carries a lease's
  purpose or a proposal's reason: both are an agent's own words, and
  `relish agent context` already lists them as untrusted text.

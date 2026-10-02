# F07: cross-node views and log streams (#365)

Dated 1 October 2026, milestone 0.1.3. Scope comes from the F07 entry in
[the codebase completion plan](archive/2026-09-17-codebase-completion-plan.md):

> Some log streams, deployment history and dashboard views remain local;
> stdout/stderr distinction, richer querying and a complete live-metrics UX
> need explicit contracts.
>
> **Completion test:** Remote workloads, partial member failures and
> namespace scope are visible consistently; each missing view/stream is
> implemented and tested separately.

## What is still local (survey, 1 Oct 2026)

| View or stream | Today | Who sees it |
|---|---|---|
| `GET /v1/logs/{app}/{ns}?follow=true` (SSE) | cluster-wide since Z2.2, with `warning` events | `relish logs -f` |
| `GET /v1/ws/logs/{app}/{ns}` (WebSocket) | **this node only** | the TUI's live logs ("live logs from the connected node") |
| `GET /v1/deploys/history/{app}` | **this node only** (each agent records its own rollout) | `relish history`, TUI app detail, dashboard app page |
| `GET /v1/events` | **this node only** | TUI events view and its 2 s refresh, `relish wtf` |
| `GET /v1/jobs` | **this node only** | TUI jobs view |
| `/ui/node/{name}` | **this node's workloads only**; another node's page lists nothing and charts *this* node's CPU and memory under the other node's name | dashboard |
| stdout/stderr | stderr is never captured separately | everything |
| log query filters | substring grep, `--since` only | `relish logs` |
| live metrics | charts poll; no stated contract | dashboard, TUI |

## Split

The whole family is larger than one 0.1.3 PR. Two coherent parts:

**Part 1 (this PR): every view and stream answers for the whole cluster.**
One pattern, applied view by view: the node you ask fans out to every live
member with the service token and `local=true`, tags each row with the node it
came from, applies the *caller's* scope to the merged rows, and reports each
member that didn't answer as a warning rather than failing or going quiet.

1. The WebSocket log stream follows every node that runs the app, through the
   same merge the SSE follow uses. Frames become a small JSON enum
   (`{"line": …}` / `{"warning": …}`) so a client can tell a node leaving from
   a log line. The TUI shows warnings inline and drops "from the connected
   node".
2. Deploy history merges every node's records, each tagged with its node, with
   warnings. `relish history` gains a NODE column and prints warnings to
   stderr; the TUI and the dashboard app page read the merged history.
3. Recent events merge every node's store, newest last, node filled in where
   the event didn't name one, with warnings. The TUI events view and `relish
   wtf` use it.
4. Jobs: `GET /v1/jobs?cluster=true` merges every node's jobs with their node;
   the TUI jobs view uses it.
5. The dashboard node page lists any node's workloads from the cluster status
   fan-out, says when that node didn't answer, and charts CPU and memory only
   on the node's own dashboard instead of misattributing this node's.

Tests first: unit tests in `src/bun/api/cluster_view_tests.rs` against a fake peer router for
each fan-out (merge, node tags, partial failure, namespace scope), reducer and
renderer tests for the TUI, and one cluster test in `tests/placement.rs`
(`make test-cluster`) that reads the WebSocket stream, deploy history and
events through one node of three and watches a member leave.

**Part 2 (follow-up issue): richer log contracts.**

- Separate stderr capture end to end (process and runc runtimes; Apple
  Container nulls stderr today), a `stream` column that is honest, and a
  `--stream` filter.
- Query filters: `--until`, `--instance`, regex `--grep`. *(Done on
  `f07-log-filters`: `--instance` also works with `-f`; a pattern that won't
  compile is a 400, or a flag error in `relish`.)*
- A cluster-wide live event stream (`/v1/ws/events` merging peers) instead of
  Part 1's 2 s refresh of the merged history. *(Done on `f07-live-events`: each node
  offers `/v1/events?follow=true&local=true` over SSE, and the WebSocket
  merges every member's feed after a merged backlog.)*
- A stated live-metrics contract for the dashboard and TUI (refresh cadence,
  remote node charts through the metrics fan-out).

## Compatibility

`/v1/ws/logs` frames change from raw text to JSON, and peers now receive
`local=true` on history, events and jobs requests. Both are client-facing and
node-to-node HTTP shapes inside one release; no persisted state changes.
`src/compatibility.rs` gets a bump because a mixed cluster would fan out
twice or misread frames.

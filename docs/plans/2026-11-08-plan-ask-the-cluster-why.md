# Ask the cluster why: decision provenance and the incident timeline

Issue [#1685](https://github.com/reliaburger/reliaburger/issues/1685),
milestone [0.2.8](https://github.com/reliaburger/reliaburger/milestone/30), on
the `release-0-2-8` train. It pulls part of 0.4.2's incident timeline
([#1084](https://github.com/reliaburger/reliaburger/issues/1084)) forward.

## The problem

An agent can already ask `relish agent explain crashloop` and learn what a
`wtf` finding means. It can't ask the questions it actually has in an
incident: why did `web-1` land on `node-2`? Why did `web-0` restart? Why was
my deploy refused? And what changed in the ten minutes before it broke?

Today the answers are scattered, and some of them don't exist:

| Decision | Where it's made | What's recorded today |
|---|---|---|
| Placement | `plan_pass` in `src/cluster/orchestrate.rs`, scored by `score_nodes` in `src/meat/score.rs` | Only a shortfall: one `placement` event when an app starts or stops being short, with the `UnplacedReason`. A successful placement leaves nothing but the result. |
| Restart after a failed run | `retry_restart` in `src/bun/agent/restarts.rs` | A `restart` event, "instance X restarted (attempt N)". No reason, no budget. |
| Restart after failed health checks | `restart_published_unhealthy` in `src/bun/agent/health_checks.rs` | The same `restart` event, indistinguishable from the one above, plus an earlier `health` event. |
| Restart budget exhausted | `retry_restart` | A `job_failed` event with a sentence. |
| Deploy refused by policy | `policy_refusal` in `src/bun/api/apply.rs` | A durable `policy.denied` audit record naming the policies, without the app. |
| Deploy refused by an owner, capacity admission or the council | `owner_refusal`, the capacity probe and the `Refused` arm of the council write in `src/bun/api/apply.rs` | Nothing. The caller gets the HTTP answer; nobody else ever learns of it. |

So the work is mostly recording, not reading: the rule and the evidence are
in hand at the moment of the decision and gone a moment later.

## Decisions

### Record provenance where the decision is made, in the event's `details`

Every event already carries `details`, a `BTreeMap<String, String>` of
stable machine-readable facts, serialised only when non-empty. Provenance
goes there, under fixed keys:

- `rule`: a stable rule id, such as `restart.health_failed`. It names the
  rule, not the event, so two different reasons for a restart are two rules.
- the evidence the rule compared, one key each: `threshold`,
  `consecutive_failures`, `attempt`, `restart_limit`, `exit_status`,
  `candidates`, `score`, `policies` and so on, per rule (below).

No new record type, no new store, no new endpoint: the events ring, the
durable audit log and the cluster fan-out (`/v1/events`, `/v1/audit
?cluster=true`) already carry `details` across nodes. It isn't a wire or
state format change (no new field, no new `EventKind` variant: an old peer
deserialises the same shape), so `CURRENT` in `src/compatibility.rs` stays
as it is.

The rule catalogue lives in one Rust module,
`src/relish/agent/decisions.rs`, beside `wtf`'s finding catalogue: each rule
has an id, a kind (`placement`, `restart`, `refusal`), what it means, and
the evidence keys it carries. A test reads the embedded source and checks
every rule id the catalogue lists is recorded somewhere outside tests, and
every `rule` the code records is in the catalogue, the way
`every_finding_wtf_reports_is_in_the_catalogue` keeps `wtf` honest. `explain`
finds where a rule is decided by searching the source for its id, as it does
for findings.

### Which decisions get provenance in 0.2.8

| Rule | Recorded by | Evidence |
|---|---|---|
| `placement.scored` | the leader, when `plan_pass`'s placements commit | per new instance: node, its replicas already there, total score and its four parts, how many nodes passed the filter, the runner-up; replicas wanted and kept |
| `placement.daemon` | the same, for a daemon set | per instance its node; how many nodes passed the filter |
| `placement.volume_home` | the same, for an app returning to its volumes | per instance the node that holds them |
| `placement.short` | the existing shortfall event | the `UnplacedReason` (`no_ready_node`, `no_label_match`, `no_container_runtime`, `no_capable_node`, `no_room`), placed and wanted |
| `placement.quota_blocked` | the leader, when an app newly can't fit its namespace quota | the quota error |
| `restart.process_exited` | `retry_restart` | instance, attempt, restart limit, the backoff waited out |
| `restart.start_failed` | `retry_restart`, after `record_failed_restart` | the same |
| `restart.health_failed` | `restart_published_unhealthy` | the same, plus consecutive failed probes, the unhealthy threshold, the probe and its interval |
| `restart.budget_exhausted` | `retry_restart`'s `RestartLimitExceeded` arm | attempts made, restart limit |
| `restart.sidecar_exited` | `finish_sidecar_relaunch` | the instance, the sidecar, its restarts |
| `deploy.refused.policy` | `policy_refusal` | the policies and the workloads they denied; one denied workload is now the record's app |
| `deploy.refused.owner` | `record_owner_refusal` in `cluster_apply` | the workload, `not_the_caller` or `claimed` |
| `deploy.refused.council` | the `Refused` arm of the council write | the workload, the state machine's reason |

The process's exit status isn't recorded: no runtime reports it to the
restart path today. The instance carries a `restart_cause`, set where it
stops (an observed exit, a failed start) and taken by the restart that
records the decision.

The two refusals that left nothing become audit records (action
`deploy.refused`), so they go to the durable audit log as well as the ring,
attributed to the caller like every other audited action, exactly as
`policy.denied` already was. The capacity probe's refusal isn't recorded:
only a test lease sends it.

Placement records one event per app per committed change, never one per
replica and never one per pass: a pass that changes nothing records nothing.
Jobs and task arrays don't get placement provenance (below).

### Retention

Nothing new to bound, and that's the point. Restart and placement records
live in each node's events ring (the newest 1,024 per node,
`EVENT_BUFFER_CAP`); refusals live in the audit log too, bounded by
`AuditLimits`. Evidence values are capped at 200 bytes each and a record
carries at most 16 evidence keys, so one decision can't blow the ring's
memory.

When the ring has moved past a decision, the answer says so: `explain`
reports the oldest event it could see on each node and returns `state:
"unknown"` with "no record of a restart of web-0 since <time>; older events
have left the ring", never "it didn't restart". A node that didn't answer is
named the same way.

### The commands and the envelope

`relish agent explain` keeps its positional finding id and gains decision
questions as flags. With a decision flag it reads the cluster; without one,
it still reads only the binary.

```sh
relish agent explain crashloop                       # a finding, as today
relish agent explain --restart web-0                 # why this instance restarted
relish agent explain --placement web-1               # why it's on its node
relish agent explain --refusal web                   # why the last deploy of web was refused
relish agent explain --rules                         # every decision rule, no cluster
```

`--namespace` scopes the app or instance. The envelope stays `relish.agent/v1`
and the command `explain`; `ExplainData` gains a `decisions` list next to
`findings` (one is always empty). Each decision is:

```json
{
  "kind": "restart",
  "rule": "restart.health_failed",
  "meaning": "the instance failed `threshold` health probes in a row, so the node restarted it",
  "timestamp": 2000100,
  "app": "web", "namespace": "default", "instance": "web-0", "node": "node-1",
  "evidence": { "attempt": "3", "restart_limit": "5", "consecutive_failures": "3", "threshold": "3", "probe": "http" },
  "message": { "untrusted": { "origin": "event_message", "lines": ["..."] } },
  "decided_in": { "path": "src/bun/agent/health_checks.rs", "line": 260, "function": "restart_published_unhealthy" },
  "related": [ { "timestamp": 2000070, "kind": "health", "summary": "instance web-0 became unhealthy" } ]
}
```

The answer lists the newest matching decisions first (at most 10), each with
the events about the same instance in the minute before it as `related`, and
a `window` that says per node how far back it could see. `next` points at
`relish agent timeline --app web --until <timestamp>` and at the code.

`relish agent timeline` is new:

```sh
relish agent timeline --app web --since 15m
relish agent timeline --until 2000100 --since 10m    # the ten minutes before a failure
```

`relish timeline` is the same merge for a person, as a table, with `-o json`.

### The timeline's sources and window

One time-ordered list, newest first so the byte budget trims the oldest,
merged from:

1. cluster events (`/v1/events`): deploys, restarts, health, placement,
   alerts (Mayo's transitions are already `alert` events), faults, jobs and
   the audited operations still in the ring;
2. the durable audit log (`/v1/audit?cluster=true`): applies, rollbacks,
   policy changes (`policy.*`), faults injected and cleared, token changes
   and refusals, including the ones that have left the ring. When the token
   may not read audit, that source is `unknown` with the reason, and the
   rest still answers;
3. the app's deploy history (`/v1/deploys/history/<app>`), when `--app` is
   given.

Records in both the ring and the audit log are shown once (same node,
action, principal and timestamp). Each entry carries `timestamp`, `source`
(`event`, `audit`, `deploy`), `kind`, `rule` or `action` when it has one,
`app`, `namespace`, `node`, `principal`, a one-line `summary` in our words,
the original message as untrusted text, and a `ref` back to its record
(event sequence and node, or audit record id).

The window is `--since` (default 1 hour, at most 24 hours) before `--until`
(default now). `--limit` caps the entries (default 100, at most 500) before
the budget does. Each source reports `known`, `partial` (and which nodes
didn't answer) or `unknown`, and the events source reports the oldest event
each node still holds, so a window that reaches past the ring says so
instead of showing a quiet hour.

### Tests

- One unit test per rule: the code path that decides it records the rule
  and its evidence (`retry_restart`, `restart_published_unhealthy`, the
  budget, the launch failure, `plan_pass`'s commit, each refusal arm).
- `placement.scored`'s score parts add up to the score `score_nodes` sorted
  by, as a proptest over random clusters.
- The catalogue test: every rule id in the catalogue is recorded in the
  source, and every recorded rule is in the catalogue.
- Timeline merge: ordering across nodes and sources with ties broken by node
  and sequence; the window drops what's outside it; `--limit` and the budget
  keep the newest; duplicates across ring and audit collapse; an audit
  source refused stays `unknown`; a ring that doesn't reach the window start
  is stated.
- Envelope snapshots (insta) for a decision answer, an unknown decision and
  a timeline.
- `tests/suite`: `relish agent explain --restart` and `relish agent timeline`
  through the binary against the stub Bun, like the existing `agent context`
  suite tests.
- Gym: a scenario, "why did web restart?", where a health probe fails while
  the process keeps running, so the answer (health, not a crash) needs the
  decision record; with its scorer tests.

## Out of scope

- **Counterfactuals.** "Why not node-3?" is answered only as far as the
  runner-up and the filter's count go. A full per-node rejection reason for
  every node would cost a record per node per placement.
- **Jobs and task arrays.** They place through the task-array leader, at up
  to millions of runs; a record per placement would flush every other event
  out of the ring. Their scheduling stays in `relish jobs`.
- **Decisions that outlive the ring.** Restart and placement records aren't
  durable. Making them durable means a decision log with its own retention,
  which belongs with #1084's server-side timeline.
- **Other decisions.** Autoscaler steps, drain and eviction moves, rollout
  auto-rollbacks (they're already deploy events, just without a `rule`),
  disk-pressure evictions and GitOps syncs that aren't events. Each can
  join the catalogue later with one record call and a test.
- **A server-side `/v1/timeline`.** The merge runs in `relish` over the
  endpoints that already exist, so it needs no new server code. #1084 keeps
  the endpoint, log-pattern firsts and `wtf` reusing the timeline.

## Order of work

1. This plan, the comment on #1685 and the note on #1084.
2. The rule catalogue and its source test.
3. Recording: restarts, then refusals, then placement, each test first.
4. `relish agent explain` decision questions, with snapshots.
5. The timeline merge, `relish agent timeline` and `relish timeline`.
6. The `tests/suite` tests, the gym scenario, the manual's agent chapter,
   `make agent-docs`, the command reference and the book's chapter 19.

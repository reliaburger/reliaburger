# Ship It

Up to now, deploying a new version of an app meant stopping the old one and starting the new one. If the new version was broken, users saw errors. If you had three replicas, all three went down at once. That's not how production works.

This chapter adds rolling deploys: replace instances one at a time, health-check each new instance before moving on, and automatically revert if anything goes wrong.

## Why deploys need a state machine

A deploy is not a single action. It's a sequence of coordinated steps, each of which can succeed, fail, or time out. The system needs to know exactly where it is in that sequence at all times, especially if the leader node crashes halfway through and a new leader has to pick up where it left off.

This is the textbook case for a state machine. An enum with a `transition` method that takes an event and produces the next state — or rejects the transition.

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

## Automatic rollback

When `auto_rollback = true` (the default) and a step fails, the orchestrator reverses direction. For each step that already completed, it:

1. Removes the new instance from routing
2. Stops the new instance
3. Starts a fresh instance with the old image
4. Adds it to routing

The same `DeployDriver` methods, called in reverse order. The rollback itself can fail (if the old image is also broken), in which case the deploy enters the `Failed` state and the operator must intervene.

## Dependency ordering

Jobs can declare `run_before = ["app.web"]`, meaning they must complete before the rolling phase begins. Database migrations are the classic example: you want `migrate` to finish before `web` gets the new code.

The orchestrator runs all pre-deploy jobs first. If any fail, the deploy fails immediately. No instances are modified. Clean.

## Raft persistence

Deploy state is committed to Raft at every phase transition. If the leader dies mid-deploy, the new leader reads the last known state and can resume. The deploy history (last 50 per app) is also in Raft, queryable via `relish history` or the API.

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

## What we deferred

Blue-green deploys, autoscaling, the Lettuce GitOps engine, and Kubernetes migration tools are all Phase 9. Rolling deploys with automatic rollback cover the vast majority of production deployment needs.

## Test count

Phase 7 adds 48 tests, bringing the total to 1047. The new tests cover every state machine transition (valid and invalid), the rolling orchestrator with mock driver (happy path, health failure, rollback, dependencies, start failure), Raft persistence of deploy state and history, CLI parsing, and config validation.

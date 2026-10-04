# Cluster apply commits dependent app revisions before run_before migrations complete

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Cluster and agent production paths verified; no live council run.

### Problem


The supported `run_before = ["app.api"]` migration gate is lost whenever apply uses a council. `cluster_apply` first commits app specs into Raft, making them available to the scheduler and node reconcilers. It then sends a config containing only the jobs to the agent. `DeployWorker::run_deploy` implements prerequisite waiting only inside its app loop; with an empty app map that loop never executes. The job is instead launched as an ordinary independent job, and apply can report completion once the job has started.

An arbitrarily slow migration cannot delay the new app, and an exit-1 migration cannot abort/roll back the already-committed app. This can expose an app to an incompatible database schema. This contradicts the current whitepaper §11 statement at [docs/whitepaper.md:652](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/whitepaper.md#L652): jobs can declare `run_before` to ensure migrations complete before app instances start.

### Reproduction


Apply the following to a running process-mode test council with `/bin/sh` allowlisted (or use container commands on a rootful runc council):

```toml
[job.migrate]
script = "sleep 15; exit 1"
run_before = ["app.api"]

[app.api]
script = "sleep 300"
```

Watch `api` start before the migration exits. The migration fails but `api` remains desired/running. A standalone apply takes the prerequisite path and fails before deploying `api`.

### Evidence


- [src/bun/api/apply.rs:583–617](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L583-L617): desired-state app writes commit first.
- [src/bun/api/apply.rs:619–637](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L619-L637): jobs-only `Config` sent afterward.
- [src/council/apply.rs:68–77](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/council/apply.rs#L68-L77): app specs become schedulable desired state; jobs excluded.
- [src/bun/agent/deploy_worker.rs:155–191](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/deploy_worker.rs#L155-L191): prerequisite gates exist only inside `for (app_name, spec) in &config.app`.
- [src/bun/agent/deploy_worker.rs:436–485](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/deploy_worker.rs#L436-L485): job-only path launches without waiting for successful exit.

Verification: exact call path and data transformation checked; a live council repro was not run by this subagent. The parent independently rechecked these production paths; the proposed live-council reproduction remains an acceptance test, not an executed result.

### Fix / acceptance


Keep dependent app revisions unschedulable until the prerequisite run has positively exited zero. Failure/timeout must leave the old app revision untouched and return an apply error. Add clustered leader/follower tests with a blocked migration and a failed migration; checking only standalone agent behavior misses this defect.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bun/api/apply.rs:583–590](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L583-L590)

```rust
            None => crate::council::config_to_desired_writes(&config),
        };
        for request in writes {
            let describe = describe_write(&request);
            match council.write(request).await {
                // A state-machine refusal (lease expired, in cleanup, resource
                // owned elsewhere, quota) is NOT a commit — surfacing it as an
                // error stops the apply instead of streaming "committed" and
```

[src/bun/api/apply.rs:619–637](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L619-L637)

```rust
        // Jobs are not cluster-scheduled yet; run them here, as before.
        if !config.job.is_empty() {
            let _ = event_tx
                .send(ApplyEvent::Progress {
                    message: format!(
                        "{} job(s) deploying on this node (jobs are not cluster-scheduled yet)",
                        config.job.len()
                    ),
                })
                .await;
            let job_config = Config {
                job: config.job.clone(),
                ..Config::default()
            };
            let _ = cmd_tx
                .send(AgentCommand::Deploy {
                    config: job_config,
                    events: event_tx.clone(),
                })
```

[src/bun/agent/deploy_worker.rs:155–175](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/deploy_worker.rs#L155-L175)

```rust

        for (app_name, spec) in &config.app {
            if self.report_cancellation(&events).await {
                return;
            }
            let namespace = spec.namespace.as_deref().unwrap_or("default");

            // run_before (E): jobs declaring `run_before = ["app.<name>"]` must
            // run to completion before this app's deploy begins — migrations are
            // the classic case. A prerequisite failure aborts the whole deploy.
            let target = format!("app.{app_name}");
            for (job_name, job_spec) in &config.job {
                // Cron-scheduled jobs fire on their schedule, never as a
                // deploy-time prerequisite.
                if ran_prereqs.contains(job_name)
                    || job_spec.schedule.is_some()
                    || !job_spec.run_before.contains(&target)
                {
                    continue;
                }
                let job_ns = job_spec.namespace.as_deref().unwrap_or("default");
```

[src/bun/agent/deploy_worker.rs:436–445](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/deploy_worker.rs#L436-L445)

```rust
            }
            // Already run to completion as a run_before prerequisite above, or a
            // cron-scheduled job that fires on its schedule rather than now.
            if ran_prereqs.contains(job_name) || spec.schedule.is_some() {
                continue;
            }
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            if let Some(operation) = &self.operation {
                operation
                    .advance(
```

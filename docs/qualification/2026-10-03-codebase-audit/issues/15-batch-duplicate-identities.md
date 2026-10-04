# Duplicate batch job names silently drop work and leave completion tracking stuck

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Real router dispatch and tracker state reproduced.

### Problem


Batch jobs are described as unique by name, but submit never checks uniqueness. Two entries called `duplicate` return `assigned:2`, yet per-node config synthesis is a `BTreeMap` keyed by the bare name, so only one job is deployed. Assignment-to-spec matching also selects the first matching name. Completion tracking finds only the first record; every later report is a duplicate/conflict on that first record. The second record stays pending permanently, including after the watch deadline, because timeout reports again update the first record.

Same bare names in different namespaces are also broken: the tracker/report wire uses only `job_name` and config synthesis/dispatch matching omit namespace. Either reject duplicate bare names clearly, or carry namespace-qualified identities throughout.

### Verified reproduction


Current router probe submitted two same-name job entries with different scripts. Response: `202`, `assigned:2`. Fake agent received `jobs ["duplicate"]` (one map entry). Supplied a terminal status for that name; `/v1/batch/2` returned `total:2,pending:1,completed:1,done:false`. `BatchRecord::report` always finds the first matching name, so retries/deadline cannot settle the second.

### Evidence / fix


- [src/bun/batch.rs:724–745](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L724-L745): no uniqueness admission.
- [src/bun/batch.rs:784–788](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L784-L788), `820-825`: `.find` by name.
- [src/bun/batch.rs:499–500](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L499-L500): silent map replacement.
- [src/meat/batch_tracker.rs:127–130](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/batch_tracker.rs#L127-L130): reports only first same-name job.
- [src/bun/batch.rs:605–607](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L605-L607): timeout cannot repair duplicates.

Reject ambiguous duplicates before registering a batch, or add stable per-submission identities to assignment, dispatch and reports. Test duplicates in one namespace and same names across namespaces; never report more assigned work than can execute and every admitted record must reach a terminal state.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bun/batch.rs:493–505](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L493-L505)

```rust
            for job in &jobs {
                reporter.report(batch_id, &job.name, false).await;
            }
            return;
        }
    };
    for job in &jobs {
        config.job.insert(job.name.clone(), job.spec.clone());
    }

    let (event_tx, mut event_rx) = mpsc::channel(64);
    if cmd_tx
        .send(AgentCommand::Deploy {
```

[src/bun/batch.rs:820–830](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L820-L830)

```rust
    for (job_name, node_id) in &allocation.assignments {
        if let Some(submission) = jobs.iter().find(|j| &j.name == job_name) {
            by_node
                .entry(node_id.clone())
                .or_default()
                .push(submission.clone());
        }
    }

    let callback_base_url = self_callback_url(&state, &self_name).await;
    for (node_id, node_jobs) in by_node {
```

[src/meat/batch_tracker.rs:121–137](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/batch_tracker.rs#L121-L137)

```rust
        if !matches!(
            status,
            JobStatus::Running | JobStatus::Completed | JobStatus::Failed
        ) {
            return Err(ReportError::NotReportable { status });
        }
        let job = self
            .jobs
            .iter_mut()
            .find(|j| j.name == job_name)
            .ok_or_else(|| ReportError::UnknownJob {
                job: job_name.to_string(),
            })?;
        if job.status == status {
            return Ok(ReportOutcome::Duplicate);
        }
        let legal = matches!(
```

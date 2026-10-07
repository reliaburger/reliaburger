# A previous job’s terminal status can falsely complete a new batch before launch

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Real router and delayed fake agent acknowledgement reproduced.

### Problem


The leader's batch pull watcher starts immediately at submission and matches runner status solely by `(name, namespace)`. It has no batch/run/attempt generation. A completed job with the same name remains visible while the new job is queued or clearing its prior artifacts. The watcher immediately marks the **new** batch completed based on the **old** successful run. That terminal state rejects a later failure from the actual new run.

Similar misattribution occurs when separate overlapping batches reuse a job name: even a batch whose deploy is refused because another run is active can race with another run's success report. The job ledger already stores explicit run generations, but they aren't carried into `InstanceStatus`/`BatchJobRecord`.

### Verified reproduction


Current HTTP-router harness held the new `AgentCommand::Deploy`'s completion event for two seconds while Status returned a prior `duplicate/default` job's `stopped,exit_code=0`. Submitted a new batch whose script would fail. Within 100 ms, before the new deploy completed, `/v1/batch/3` returned `completed:1,done:true,elapsed_secs:0`. This demonstrates the exact watcher race without running scripts. Probe `evidence/batch.rs`.

### Evidence / fix


- [src/bun/batch.rs:575–585](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L575-L585): watcher starts independently of run acknowledgement.
- [src/bun/batch.rs:627–642](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L627-L642): polls every nonterminal assignment immediately.
- [src/bun/batch.rs:461–468](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L461-L468): name/namespace only outcome match.
- [src/meat/batch_tracker.rs:87–99](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/batch_tracker.rs#L87-L99): assignment record has no run generation.
- [src/bun/jobs.rs:67](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/jobs.rs#L67): node-local ledger already has a generation field.

Bind every batch job to a unique acknowledged run identity, expose that identity in status and callbacks, and ignore evidence from another generation. Add tests reusing terminal names, delayed dispatch/launch, overlapping batches, runner restart and delayed old callbacks. Until identities are supported, reject conflicting/reused job identities safely.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bun/batch.rs:461–475](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L461-L475)

```rust
fn job_outcome(statuses: &[InstanceStatus], name: &str, namespace: &str) -> Option<bool> {
    for status in statuses
        .iter()
        .filter(|s| s.app_name == name && s.namespace == namespace)
    {
        let outcome = match (status.state.as_str(), status.exit_code) {
            ("failed", _) => Some(false),
            ("stopped", Some(0) | None) => Some(true),
            _ => None,
        };
        if outcome.is_some() {
            return outcome;
        }
    }
    None
```

[src/bun/batch.rs:627–646](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L627-L646)

```rust
        for job in record.jobs.iter().filter(|j| !j.status.is_terminal()) {
            let Some(node) = &job.node else { continue };
            let outcome = if node.0 == self_name {
                local_statuses
                    .as_deref()
                    .and_then(|statuses| job_outcome(statuses, &job.name, &job.namespace))
            } else {
                fetch_remote_outcome(state, node, &job.name, &job.namespace).await
            };
            if let Some(completed) = outcome {
                let status = if completed {
                    JobStatus::Completed
                } else {
                    JobStatus::Failed
                };
                let _ = report_batch_job(state, batch_id, &job.name, status).await;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(PULL_INTERVAL_MS)).await;
```

[src/meat/batch_tracker.rs:87–100](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/batch_tracker.rs#L87-L100)

```rust
    pub namespace: String,
    /// Node the job was assigned to; `None` for unschedulable jobs.
    pub node: Option<NodeId>,
    /// Current status.
    pub status: JobStatus,
}

/// One tracked batch: its jobs and when it was submitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRecord {
    /// Per-job records, in allocation order.
    pub jobs: Vec<BatchJobRecord>,
    /// Submission time as seconds since the Unix epoch. Wall-clock (not
    /// `Instant`) because the record crosses the Raft wire and must
```

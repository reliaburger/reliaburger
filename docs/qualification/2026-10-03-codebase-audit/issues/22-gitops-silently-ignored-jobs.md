# GitOps reports successful sync while dropping jobs and migration prerequisites

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Production execute_sync with app and migration job reproduced.

### Problem and impact


Lettuce accepts job declarations but omits them entirely from its change set and never dispatches them. An app and its `run_before` migration in the same signed commit can be reported successfully applied while only the app is committed. Cron jobs are likewise never registered by this path.

The diff comment says jobs use a one-shot deploy path, but the production sync runner only calls `apply_changes` on the diff. It has no subsequent job dispatch. Avoiding repeated reconciliation of one-shot jobs is reasonable; silently accepting and discarding the declarations is not a complete one-shot execution policy.

### Reproduction


Commit this config to a local test Git repository:

```toml
[app.web]
image = "web:v1"
[job.migrate]
image = "migrate:v1"
run_before = ["app.web"]
```

Production `execute_sync` returns `Success`, one added resource and exactly one change, for the app. No job change exists for the runner to apply. The parent’s executable test confirms that result; the runner’s lack of any additional dispatch was source-verified.

Expected: the prerequisite positively completes before the app revision becomes schedulable, or the entire unsupported configuration is refused with a clear job-specific error. A successful GitOps sync must not imply that ignored declarations ran.

### Fix direction and acceptance


Define a durable commit/run identity for one-shot jobs so retries and leader failover do not rerun a completed migration accidentally. Register supported scheduled jobs through the appropriate durable execution path. While that is unavailable, reject GitOps job declarations, especially `run_before`, before writing dependent apps.

- A commit with an app and blocked/failed migration cannot publish the new app early.
- Job-only and scheduled-job commits either perform the promised work or fail explicitly.
- Repeated polls of the same commit and leader changes respect the chosen once-per-revision policy.
- Sync summaries/history name job outcomes; unsupported work never produces full success.

### Existing issue comparison


No matching issue was found. #305 concerns drift repair. This is separate from the manual cluster-apply migration gate defect: Lettuce drops jobs before it ever reaches agent deployment, so fixing cluster_apply alone does not repair GitOps.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/lettuce/diff.rs:186–204](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/diff.rs#L186-L204)

```rust
    // Jobs are deliberately absent from the diff. A job runs to
    // completion; it isn't reconciled desired state, so there's no job
    // map in Raft to compare against (see `config_to_desired_writes`).
    // The old code compared every git job against an always-empty set,
    // so it emitted an `Add` for every job on *every* sync — a change the
    // applier then silently dropped (`ChangePayload::Generic` maps to no
    // write) while inflating `summary.added`. That's the GIT2b bug: a job
    // "removed" from git was never in the desired state to begin with, so
    // it can't be re-added, and a job present in git is dispatched by the
    // one-shot deploy path, not by reconciliation. Emitting nothing here
    // keeps the summary honest and the applier free of no-op changes.

    let summary = DiffSummary {
        added,
        modified,
        removed,
    };

    (changes, summary)
```

[src/lettuce/runner.rs:178–191](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/runner.rs#L178-L191)

```rust
            // silently skipped the way a `None` used to drop jobs and
            // namespaces on the floor.
            //
            // Atomicity (D12): `last_applied_commit` advances only if
            // EVERY write in the sync succeeds. The old code advanced the
            // commit regardless of per-change failures, so a failed write
            // was marked "applied" and never retried — the resource just
            // vanished until the next unrelated commit. Now a failure
            // leaves the commit unadvanced, and the next tick re-applies
            // the whole set. Writes are idempotent (spec upsert / delete),
            // so re-applying an already-committed change is a harmless
            // no-op.
            let applied = match apply_changes(&council, &outcome.changes).await {
                Ok(applied) => applied,
```

[src/lettuce/sync.rs:277–284](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/sync.rs#L277-L284)

```rust
        merged.app.extend(file_config.app);
        merged.job.extend(file_config.job);
        merged.namespace.extend(file_config.namespace);
        merged.permission.extend(file_config.permission);
        merged.build.extend(file_config.build);
    }

    (merged, errors)
```

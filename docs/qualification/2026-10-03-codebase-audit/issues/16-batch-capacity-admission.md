# Cluster batches use stale or unlimited fallback capacity and do not retain admission reservations

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Capacity translation, fallback and durable tracker source path.

### Problem


The batch capacity path bypasses the cluster scheduler's fail-closed placement policy:

1. `capacities_from_reports` never filters `aggregated.stale_nodes`; expired resource commitments still look current.
2. If every report is missing/pre-capacity, even in a cluster it substitutes a local capacity of `u64::MAX / 2` CPU and memory and assigns everything to the leader.
3. Allocations reserve only a request-local `Vec<NodeCapacity>` which is dropped after submit. Durable batch records do not include resource requests/specs. Another batch submitted before reports reflect the first sees the same free capacity and can double-book the node. Cluster app pending-placement reservation fix #432 does not apply to batch's independent dispatch path.

This can dispatch resource-limited jobs to an already-overcommitted node, and startup/leader transition is exactly when capacity evidence is least trustworthy.

### Evidence / reproduction


- [src/bun/batch.rs:152–182](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L152-L182): reports mapped without freshness filtering.
- [src/bun/batch.rs:185–195](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L185-L195), `752-764`: unlimited fallback in clustered branch.
- [src/bun/batch.rs:766–778](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L766-L778): reserves only local capacity vector.
- [src/meat/batch_tracker.rs:87–109](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/batch_tracker.rs#L87-L109): durable assignment does not retain resource footprint.

Repro: on a one-node cluster with no fresh reports, submit more CPU/memory than the configured node owns; it reports assigned instead of waiting/refusing. Or submit two batches each consuming the full same fresh capacity before the first runner report updates; both are accepted/assigned. Verification here is source call-path; no live overload repro run. The parent independently rechecked the capacity path, unlimited fallback and tracker schema; the live overload case remains unexecuted.

### Fix / acceptance


Require fresh capacity in cluster mode, fail retryably when unavailable, and retain admitted resource commitments atomically until they are represented in runtime reports or terminal cleanup. Add missing/stale-capacity and concurrent-submission tests; verify totals never exceed allocatable resources. Keep the standalone fallback policy explicit.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bun/batch.rs:158–170](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L158-L170)

```rust
        let Some(report) = aggregated.reports.get(&member.node_id) else {
            continue;
        };
        let usage = &report.resource_usage;
        if usage.cpu_total_millicores == 0 {
            continue; // pre-capacity node
        }
        capacities.push(NodeCapacity {
            node_id: member.node_id.clone(),
            address: member.address,
            total: Resources::new(
                u64::from(usage.cpu_total_millicores),
                u64::from(usage.memory_total_mb) * 1024 * 1024,
```

[src/bun/batch.rs:185–197](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L185-L197)

```rust
/// Standalone fallback: one self node with effectively unlimited
/// capacity, so single-node clusters (and tests) schedule locally.
pub fn local_only_capacity(node_name: &str) -> Vec<NodeCapacity> {
    vec![NodeCapacity {
        node_id: NodeId(node_name.to_string()),
        address: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        total: Resources::new(u64::MAX / 2, u64::MAX / 2, 0),
        reserved: Resources::new(0, 0, 0),
        allocated: Resources::new(0, 0, 0),
        labels: Default::default(),
    }]
}

```

[src/bun/batch.rs:751–764](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L751-L764)

```rust

    // Capacity: the aggregated reports when clustered, self otherwise.
    let mut capacities = match (&state.aggregated_rx, &state.membership) {
        (Some(aggregated_rx), Some(membership)) => {
            let members = membership.read().await.clone();
            let capacities = capacities_from_reports(&members, &aggregated_rx.borrow());
            if capacities.is_empty() {
                local_only_capacity(&self_name)
            } else {
                capacities
            }
        }
        _ => local_only_capacity(&self_name),
    };
```

[src/bun/batch.rs:766–778](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L766-L778)

```rust
    let batch_jobs: Vec<BatchJob> = jobs
        .iter()
        .map(|job| BatchJob {
            name: job.name.clone(),
            resources: Resources::new(
                job.spec.cpu.as_ref().map(|r| r.request).unwrap_or(0),
                job.spec.memory.as_ref().map(|r| r.request).unwrap_or(0),
                0,
            ),
        })
        .collect();

    let allocation = schedule_batch(&batch_jobs, &mut capacities);
```

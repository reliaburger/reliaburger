# Ingress table rebuilds resurrect failed backends and continued probes never exclude them

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Production probe loop and table rebuild reproduced.

### Problem

Routing-table rebuilds reset every backend's local health to healthy. The long-running probe tracker reports updates only when its health state changes. If a backend was already marked unhealthy, an unrelated service catalogue update makes it routable again; continued failed probes never remove it because the tracker is already unhealthy. It can receive traffic indefinitely until it first recovers and fails again.

### Evidence

- [src/wrapper/routing.rs:425–432](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/routing.rs#L425-L432): rebuilt backend starts with `locally_healthy: true` at line 431.
- [src/wrapper/routing.rs:566–583](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/routing.rs#L566-L583): `ProbeTracker::record` only returns a verdict on state transitions.
- [src/wrapper/routing.rs:645–650](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/routing.rs#L645-L650): probe loop only applies returned verdicts.
- [src/bun/agent/consumer.rs:484–507](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/consumer.rs#L484-L507): consumer publication constructs and installs a fresh routing table; rebuild line 500 and replacement line 505.

### Reproduction

`evidence/state.rs` creates a real table, starts production `run_health_probes` against closed loopback port 9 with unhealthy threshold 1, waits for exclusion, rebuilds from the identical ServiceMap and waits for several additional failed sweeps.

Actual verified output:

```text
probe before rebuild: routable=0
probe after rebuild and more failures: routable=1
```
Expected: an unchanged failed backend remains excluded after rebuilding. A catalogue change for another application must not resurrect it.

### Suggested fix / acceptance

Carry the verdict for unchanged endpoint/execution identity through rebuilds, or continuously reapply current tracker state. Ensure a probe of an old address cannot mark a replacement backend unhealthy. Test failed backend + unrelated catalogue update, recovery, endpoint replacement and continued failures after multiple rebuilds. Distinct from #431's expiry of stale node endpoint reports: this is the ingress-local active probe state.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/wrapper/routing.rs:425–434](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/routing.rs#L425-L434)

```rust
        .map(|b| Backend {
            instance_id: b.instance_id.clone(),
            addr: SocketAddr::new(b.node_ip.into(), b.host_port),
            healthy: b.healthy,
            // Trust the service map on a fresh rebuild; the active probe loop
            // re-evaluates local reachability from here.
            locally_healthy: true,
            local: b.local,
        })
        .collect();
```

[src/wrapper/routing.rs:574–586](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/routing.rs#L574-L586)

```rust
            counters.consecutive_unhealthy = counters.consecutive_unhealthy.saturating_add(1);
            counters.consecutive_healthy = 0;
            if counters.locally_healthy
                && counters.consecutive_unhealthy >= self.threshold_unhealthy
            {
                counters.locally_healthy = false;
                return Some(false);
            }
        }
        None
    }

    /// Drop tracker state for instances no longer present in `live`.
```

[src/bun/agent/consumer.rs:498–507](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/consumer.rs#L498-L507)

```rust
            .collect();
        let mut table = crate::wrapper::routing::RoutingTable::new();
        table.rebuild(&map, &routes).map_err(failure)?;
        for entry in services {
            self.publish_backend_kernel(&ServiceId::new(&entry.namespace, &entry.app_name), &map)
                .await?;
        }
        *self.routing_table.write().await = table;
        self.service_map_tx.send_replace(map);
        Ok(())
```

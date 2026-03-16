# Code Walkthrough: Phase 2

You've read the whitepaper. You know *what* Reliaburger is trying to be. Now you want to look at the code and understand how we actually built the cluster formation layer. Where do you start? There are nine modules, hundreds of files, and 544 unit tests in the library alone.

This chapter is your map. It tells you where things live, what order to read them in, and which paths through the code are the ones that matter. Think of it as the document you wish someone had written for you before you dove into a new codebase at work.

## How to read this codebase

Open `src/lib.rs`. It's short. Eight public modules:

```rust
pub mod bun;          // Agent (the node daemon)
pub mod config;       // Configuration parsing
pub mod council;      // Raft consensus
pub mod grill;        // Container runtime
pub mod meat;         // Scheduler + shared types
pub mod mustard;      // Gossip protocol
pub mod reconstruction; // State reconstruction after failover
pub mod relish;       // CLI
pub mod reporting;    // Reporting tree
```

Start with `src/meat/types.rs`. It defines the shared vocabulary that every other module imports. Then follow the data flow: gossip discovers nodes, Raft replicates desired state, workers report actual state, the reconstruction engine diffs the two, the scheduler places workloads, and the agent runs them. That's the order we'll walk through.

## The reading order

### 1. Shared types (`src/meat/types.rs`, 471 lines)

This is the Rosetta Stone of the codebase. Every module speaks this language.

**Newtypes prevent stupid bugs.** `NodeId` wraps a `String`. `AppId` wraps a name and namespace pair. Why bother? Because if both were bare strings, the compiler would happily let you pass a node ID where an app ID was expected. Rust's type system can catch that for you, but only if you give it distinct types to work with.

```rust
#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct NodeId(pub String);

#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct AppId {
    pub name: String,
    pub namespace: String,
}
```

The derive list on these types is deliberate. `Hash` and `Eq` let them serve as `HashMap` keys. `Ord` and `PartialOrd` let them sort deterministically (important for the scheduler's tiebreak logic). `Serialize` and `Deserialize` let them cross the network.

**Resources use saturating arithmetic.** CPU is measured in millicores (1000m = 1 core), memory in bytes, GPUs as whole devices. All arithmetic saturates at zero instead of panicking on underflow. When a node is overloaded and its allocated resources exceed what's available, `saturating_sub` quietly returns zero rather than wrapping to `u64::MAX`. No panics in production.

```rust
pub fn saturating_sub(&self, other: &Resources) -> Resources {
    Resources {
        cpu_millicores: self.cpu_millicores.saturating_sub(other.cpu_millicores),
        memory_bytes: self.memory_bytes.saturating_sub(other.memory_bytes),
        gpus: self.gpus.saturating_sub(other.gpus),
    }
}
```

**NodeCapacity** is what the scheduler sees when it looks at a node. Total resources, reserved resources (for the OS and agent), currently allocated resources, and labels for placement constraints. The `allocatable()` method computes what's left: `total - reserved - allocated`, using that saturating subtraction.

**SchedulingDecision and Placement** are the output of the scheduler. "Put this app's replicas on these nodes, reserving these resources." Simple value types that get committed to the Raft log and replicated across the council.

### 2. Gossip protocol (`src/mustard/`)

This is the SWIM protocol implementation. It's how nodes discover each other and detect failures. Read the files in this order:

#### `state.rs` (263 lines) — The four-state machine

Every node in the cluster is in one of four states:

```
Alive --> Suspect --> Dead
  ^          |
  +----------+  (refutation with higher incarnation)

Any --> Left  (graceful departure)
```

The `resolve_conflict()` function is the heart of SWIM's consistency model. When two nodes disagree about a third node's state, the conflict resolution rules are:

1. Higher incarnation always wins.
2. At equal incarnation, more severe state wins (`Dead > Suspect > Alive`).
3. `Left` is terminal. Once a node departs gracefully, it stays departed. Forever.

This file also has property-based tests using `proptest`. The property under test: `resolve_conflict` is deterministic, the winner's incarnation is always one of the two inputs, and if either side is `Left`, the result is `Left`. Can you see why property tests are perfect here? The function has a small input space (4 states x 4 states x incarnation pairs) but subtle edge cases. Letting proptest explore randomly is cheaper than enumerating every combination by hand.

#### `message.rs` (205 lines) — Wire format

`GossipMessage` is the UDP datagram structure. Version byte, sender ID, incarnation, an HMAC field (zeroed until Phase 4 adds mTLS), and the payload. Three payload variants:

- **Ping**: "are you alive?" with piggybacked membership updates.
- **PingReq**: "please probe this target for me" (indirect probing).
- **Ack**: "yes, I'm alive" with piggybacked updates.

Every single message carries up to 8 piggybacked `MembershipUpdate` entries. This is how gossip spreads, without dedicated broadcast messages. You send a PING to check if someone's alive, and while you're at it, you share what you know about the rest of the cluster.

#### `membership.rs` (808 lines) — The membership table

`MembershipTable` is a `HashMap<NodeId, NodeMembership>`. Each entry tracks the node's state, incarnation, address, labels, first-seen timestamp, last ACK time, and optional resource summary. The `apply_update()` method uses `resolve_conflict()` to decide whether an incoming update should replace the current entry.

The table also stores council and leader flags per node, and exposes `snapshot()` to publish the full membership via a `watch` channel. This is how the agent learns about cluster topology changes without polling.

#### `dissemination.rs` (318 lines) — Priority queue for piggybacking

How many times should you piggyback an update? Too few and the information doesn't reach everyone. Too many and you waste bandwidth. The answer: `3 * ceil(log2(cluster_size))`. In a 1000-node cluster, that's about 30 broadcasts per update. In a 5-node dev cluster, it's 9.

The queue is a `BinaryHeap` that prioritises failure updates over join updates. Dead and Left nodes get priority 3, Suspect gets 2, Alive gets 1. This means the cluster learns about failures faster than joins, which is exactly what you want.

#### `protocol.rs` (1103 lines) — The probe cycle

This is the big one. `MustardNode::run_one_cycle()` is a single SWIM protocol period:

1. Promote any suspects whose timeout has expired to Dead.
2. Reap expired Dead/Left entries from the membership table.
3. Pick a random alive peer.
4. Send a PING with piggybacked updates.
5. Wait for an ACK within `probe_timeout` (200ms default).
6. If no ACK, send PING-REQ to up to 3 random relay nodes.
7. Wait for an indirect ACK.
8. If still nothing, mark the target as Suspect.

The `run()` method wraps this in a `tokio::select!` loop: protocol ticks on an interval, incoming messages handled immediately, shutdown via `CancellationToken`. On shutdown, it calls `leave()` to announce graceful departure before exiting.

One subtlety worth noting: `wait_for_ack()` drains the transport while waiting. Messages that arrive during the wait aren't dropped. They're processed (piggybacked updates are applied), but only an ACK from the specific target counts as success. The `wait_for_relay_ack()` variant avoids calling `handle_message()` recursively to prevent async recursion issues.

**Critical path**: `MustardNode::run()` --> `run_one_cycle()` --> picks random peer --> sends PING --> waits for ACK --> marks Suspect on timeout.

#### `transport.rs` — Network abstraction

`MustardTransport` is a trait with `send()` and `recv()` methods. Two implementations:

- `InMemoryNetwork` + `InMemoryTransport`: for tests. Supports simulated partitions via a blocklist, `try_recv()` for deterministic test driving.
- `UdpMustardTransport`: for production. Real UDP sockets, bincode serialisation.

The in-memory transport deserves a close look if you're writing tests. It's what makes the gossip tests deterministic and fast.

#### `config.rs` (49 lines) — Protocol tuning

`GossipConfig` with sensible defaults: 500ms protocol interval, 200ms probe timeout, 5s suspicion window, 3 indirect probes, 60s cleanup timeout. The tests override these with much shorter values (50ms/20ms/100ms) so they finish quickly.

### 3. Raft consensus (`src/council/`)

This is where the cluster's brain lives. Gossip tells you *who* is in the cluster. Raft tells you *what the cluster should be doing*.

#### `types.rs` (319 lines) — openraft configuration

openraft (the Raft library we use) requires a type configuration that maps its generic parameters to concrete types. The `declare_raft_types!` macro sets this up:

- Node IDs are `u64` (openraft requires `Copy`, and `String` isn't `Copy`).
- Log entries carry `RaftRequest` payloads.
- Responses are `CouncilResponse`.
- `CouncilNodeInfo` maps the `u64` Raft ID back to a human-readable name and address.

`RaftRequest` is an enum with five variants: `AppSpec` (register an app), `AppDelete` (remove it), `SchedulingDecision` (where to run it), `ConfigSet` (cluster-wide config), and `Noop` (used for leader commit after election).

`DesiredState` is what the state machine builds by applying these entries in order. It's a collection of `HashMap`s: apps, scheduling placements, and config. There's a `map_as_vec` serde workaround because JSON requires string keys, but `AppId` is a struct. So these maps serialise as arrays of key-value pairs instead. Pragmatic, not pretty.

#### `state_machine.rs` (483 lines) — Applying log entries

`CouncilStateMachine` implements openraft's `RaftStateMachine` trait. The `apply()` method takes a sequence of committed log entries and mutates the `DesiredState`:

- `AppSpec` inserts or updates the app in `state.apps`.
- `AppDelete` removes it from both `apps` and `scheduling`.
- `SchedulingDecision` updates `state.scheduling`.
- `ConfigSet` inserts into `state.config`.
- `Noop` does nothing (but still advances the log position).

Snapshots serialise the entire `DesiredState` to JSON. Followers that fall behind receive the snapshot instead of replaying the entire log. In-memory only for now. Fine for development, not for production durability.

#### `log_store.rs` — In-memory Raft log

`MemLogStore` stores the Raft log and vote state in memory. It implements openraft's `RaftLogStorage` trait. Nothing fancy. This is the thing that would need to become persistent (disk-backed) for production use.

#### `network.rs` (501 lines) — Raft RPC transport

Two implementations, mirroring the gossip layer:

- `InMemoryRaftRouter`: routes RPCs between in-process Raft nodes. Supports partition simulation via a blocklist. This is what makes the Raft tests possible without real TCP.
- `TcpRaftNetworkFactory` + `TcpRaftNetwork`: length-prefixed bincode over TCP for production. The `serve_raft_rpc` function runs the server side.

The in-memory router's partition simulation is used in the `partition_majority_continues_minority_cannot_write` test, which is one of the most satisfying tests in the codebase. It proves that a 3-of-5 majority can keep writing while the minority is isolated, and everything converges when the partition heals.

#### `node.rs` (706 lines) — Clean API wrapper

`CouncilNode` wraps the raw `openraft::Raft` handle with a human-friendly API:

- `write(RaftRequest)` --> replicate and apply.
- `is_leader()` --> check via `ensure_linearizable()`.
- `desired_state()` --> read the state machine.
- `add_learner()` + `change_membership()` --> grow the council.
- `initialize()` --> bootstrap the first node.

The `write()` method handles the `ForwardToLeader` error case, which openraft returns when you try to write on a follower. The agent uses this to redirect writes to the actual leader.

**Critical path**: `CouncilNode::write(RaftRequest)` --> openraft replicates --> `CouncilStateMachine::apply()` --> `DesiredState` updated.

#### `selection.rs` (669 lines) — Council candidate selection

`select_council_candidates()` is a pure function. No I/O, no side effects, no state mutation. It takes a membership table snapshot and returns a ranked list of candidates for council promotion.

The algorithm:

1. Clamp target size to `[min_council_size(3), max_council_size(7)]`.
2. Filter: must be Alive, not already on council, older than `min_node_age` (600s default), have reported resources, and not be overloaded (CPU < 90%, memory < 85%).
3. Sort: nodes in novel zones first (for diversity), then oldest first (stability), then lexicographic by node ID (determinism).
4. Take however many are needed.

The sorting is the interesting bit. If the council already has members in zones A and B, a candidate in zone C ranks above one in zone A. This pushes the council towards geographic diversity without requiring explicit zone configuration.

### 4. Reporting tree (`src/reporting/`)

How does the leader know what's actually running on each node? Workers tell it. But in a large cluster, having every worker report directly to the leader would overwhelm it. So we build a tree: workers report to their assigned council member, and council members aggregate.

#### `types.rs` — Report structure

`StateReport` is what a worker sends every 5 seconds. It contains the node ID, a timestamp, a list of `RunningApp` entries (with health status, uptime, resource usage), cached specs, overall resource usage, and an event log.

#### `assignment.rs` (138 lines) — Deterministic parent mapping

`assign_parent()` maps a worker to a council member using a hash. Sort the council list, hash the worker ID with `DefaultHasher`, take the index modulo council size. Every node in the cluster computes the same mapping independently. No coordination needed.

When the council changes (a member dies, a new one is promoted), the hash changes and workers automatically re-route to their new parent. The tests verify that the assignment distributes evenly, is deterministic, and is independent of input order.

#### `transport.rs` — Report delivery

`ReportingTransport` trait with in-memory and TCP implementations. Same pattern as gossip and Raft. Length-prefixed TCP for production, in-memory for tests.

#### `worker.rs` — Worker side

`ReportWorker` is a spawned task that sends `StateReport`s at a configurable interval. It watches a `watch` channel for council membership changes. When the council changes, it re-hashes to find its new parent and re-connects. It also handles `CollectSnapshotRequest` from the agent to gather current workload state.

#### `aggregator.rs` — Council side

`ReportAggregator` stores the latest report from each worker, detects stale reports (workers that stopped reporting), and publishes the aggregated view via a `watch::Sender<AggregatedState>`. The leader reads this to know what's actually happening across the cluster.

**Critical path**: Worker sends report --> parent council member aggregates --> watch channel --> leader reads.

### 5. State reconstruction (`src/reconstruction/`)

What happens when a new leader is elected? It has the desired state from Raft, but it doesn't know what's *actually* running. Maybe the old leader crashed mid-deploy. Maybe a node died and its workloads need rescheduling. The reconstruction engine figures this out.

#### `types.rs` (54 lines) — Phase machine and corrections

Five phases: `Idle` --> `Announcing` --> `Learning` --> `Reconciling` --> `Active`. Three types of corrections: `MissingApp` (should be running but isn't), `ExtraApp` (running but shouldn't be), `UnknownNode` (alive but didn't report).

#### `diff.rs` (327 lines) — Pure set difference

`compute_diff()` is another pure function. Build the set of desired `(AppId, NodeId)` placements from the Raft state. Build the set of actual placements from the aggregated reports. Missing = desired minus actual. Extra = actual minus desired. Unknown = alive nodes that didn't report.

The key subtlety: unknown nodes don't generate `MissingApp` corrections. If a node didn't report, we can't know whether it's running the right things or not. We just mark it as unknown and deal with it separately. This avoids false positives during the learning period.

#### `controller.rs` — Learning period state machine

`ReconstructionController` is method-based, not an event loop. The agent calls:

- `on_leader_elected(alive_count)` --> transitions to Learning, starts the clock.
- `on_report_received(node_id)` --> tracks which nodes have reported, checks coverage.
- `check_timeout()` --> if the learning period has expired, proceeds with whatever data is available.
- `on_leader_lost()` --> back to Idle.

The learning period ends when either 95% of alive nodes have reported, or the timeout fires (15s for small clusters, 30s for large ones). Then the controller runs `compute_diff()`, produces corrections, and transitions to Active.

**Critical path**: Leader elected --> `on_leader_elected(N)` --> receives reports --> coverage >= 95% --> `compute_diff()` --> Active.

### 6. Scheduler (`src/meat/`)

The scheduler turns "I want 3 replicas of web app" into "put them on nodes 2, 5, and 7."

#### `cluster_state.rs` — The cache

`ClusterStateCache` holds per-node `SchedulerNodeState`: allocatable resources, currently allocated resources, labels, readiness flag, and the set of running apps. The `reserve()` and `release()` methods update the allocated resources when placements are made or removed.

#### `filter.rs` — Phase 1: Filter

`filter_nodes()` is the first gate. A node must be ready, have enough capacity for the requested resources, and have all required labels. Nodes that fail any of these are eliminated. Simple boolean logic, no scoring.

#### `score.rs` (220 lines) — Phase 2: Score

`score_nodes()` ranks candidates on a 0-100 weighted scale:

| Dimension | Weight | What it measures |
|-----------|--------|-----------------|
| Bin-packing | 50% | Prefer fuller nodes (maximise density, leave empty nodes for big workloads) |
| Preferred labels | 20% | Soft constraints (zone preference, SSD preference) |
| Image locality | 15% | Prefer nodes with cached images (placeholder, returns 0 until Phase 5) |
| Spread | 10% | Penalise nodes already running the same app |
| Stability | 5% | Prefer longer-running nodes (placeholder, returns 50) |

The bin-packing dimension is the dominant factor. If two nodes have the same labels and neither is running the app, the fuller one wins. This might seem counterintuitive, but it's deliberate. By packing workloads onto fewer nodes, you leave other nodes empty and available for large workloads that need a lot of resources. Kubernetes does the opposite by default (LeastRequested), which fragments your cluster and leaves no node with enough free capacity for big jobs.

Ties are broken by NodeId, ascending. Fully deterministic. Same inputs, same output, every time.

#### `quota.rs` — Per-namespace limits

`check_quota()` enforces limits on CPU, memory, GPU, app count, and replica count per namespace. Checked before scheduling begins. Returns a `QuotaExceeded` error with a human-readable message.

#### `scheduler.rs` (457 lines) — The pipeline

`Scheduler::schedule_app()` is the entry point. For `Fixed(n)` replicas, it runs the pipeline iteratively: for each replica, filter, score, select the top candidate, reserve resources in the cache, then do the next replica. This is crucial. If you scored all replicas at once and then reserved, you'd over-commit the top-scoring node.

For `DaemonSet`, it skips scoring entirely. Filter the nodes, place one replica on each.

**Critical path**: `schedule_app()` --> for each replica: `filter_nodes()` --> `score_nodes()` --> pick top --> `cluster.reserve()`.

### 7. Agent (`src/bun/`)

The agent is the node daemon. It ties everything together.

#### `agent.rs` (1827 lines) — The event loop

`BunAgent<G: Grill>` is generic over the container runtime. The event loop is a `tokio::select!` over:

- Commands from the API (deploy, stop, status, etc.) via `mpsc` channel.
- Health check ticks on a timer.
- Snapshot requests from the reporting worker.
- Shutdown signal via `CancellationToken`.

`ClusterHandle` holds the gossip membership watch, Raft metrics, council reference, and snapshot channel. It's the agent's window into the cluster.

`AgentCommand` is the enum of things the API can ask the agent to do: Deploy, Stop, Status, Logs, FollowLogs, Exec, Nodes, Council, Join, and the chaos injection commands (Partition, Heal, ChaosStatus).

#### `supervisor.rs` (783 lines) — Workload lifecycle

`WorkloadSupervisor` manages running instances. A `HashMap<InstanceId, WorkloadInstance>` holds the primary index, with a secondary index by `(app_name, namespace)` for lookup by name. `deploy_app()` creates new instances. `stop_app()` stops all instances of an app. Each instance tracks its container state, config, PID, health status, and restart history.

#### `health.rs` (654 lines) — Health checking

`HealthChecker` uses a `BinaryHeap` priority queue to schedule probes. Each instance gets a probe scheduled after its `initial_delay`. When a probe fires, the result updates a consecutive success/failure counter. The `evaluate_result()` method determines state transitions: how many consecutive successes to go from Unhealthy to Healthy, how many failures to go the other way.

#### `api.rs` — HTTP endpoints

An axum router on port 9117:

| Method | Path | What it does |
|--------|------|-------------|
| GET | `/v1/health` | Liveness check |
| POST | `/v1/apply` | Deploy workloads (SSE streaming) |
| GET | `/v1/status` | All instance statuses |
| GET | `/v1/status/{app}/{namespace}` | Single app status |
| POST | `/v1/stop/{app}/{namespace}` | Stop an app |
| GET | `/v1/logs/{app}/{namespace}` | Get logs |
| POST | `/v1/exec/{app}/{namespace}` | Execute a command |
| GET | `/v1/cluster/nodes` | Gossip membership |
| GET | `/v1/cluster/council` | Raft council status |
| POST | `/v1/cluster/join` | Join a cluster |
| POST | `/v1/chaos/partition` | Inject network partition |
| POST | `/v1/chaos/heal` | Heal partitions |
| GET | `/v1/chaos/status` | Chaos injection status |

Handlers are thin. They construct an `AgentCommand`, send it over the `mpsc` channel, and await the `oneshot` response.

#### `probe.rs` — HTTP health probing

Uses `reqwest` to make HTTP health check requests. Returns success/failure. Simple.

**Critical path**: POST `/v1/apply` --> agent receives `AgentCommand::Deploy` --> `supervisor.deploy_app()` --> `grill.start()` --> health checks begin.

### 8. CLI (`src/relish/`)

The command-line interface that humans interact with.

- `commands.rs` — each subcommand is an async function, most calling `BunClient` methods.
- `client.rs` — `BunClient` wraps `reqwest`, handles SSE parsing for the apply endpoint, classifies HTTP errors into typed `BunError` variants.
- `chaos.rs` — `council_partition()` and `worker_isolation()` scenarios with coloured narrative output. These are the chaos engineering commands that let you test cluster resilience interactively.
- `dev.rs` — Lima VM management for running multi-node clusters on macOS. YAML generation, `limactl` wrapper, cluster state persistence in `/tmp`.
- `output.rs` — Formatting and coloured output helpers.
- `plan.rs` — Deployment plan display (dry-run mode).

### 9. Configuration (`src/config/`)

- `app.rs` — `AppSpec`: image, command, replicas, port, health check config, cpu/memory/gpu ranges, placement constraints, deploy strategy.
- `job.rs` — `JobSpec`: one-shot tasks.
- `types.rs` — `Replicas` enum (`Fixed(u32)` or `DaemonSet`), `ResourceRange` (request + limit), `VolumeSpec`, `parse_resource_value()` for human-readable strings like "512Mi".
- `node.rs` — `NodeConfig`: the node-level config file with sections for node identity, cluster settings, storage, resources, networking, reporting tree, and reconstruction.
- `namespace.rs` — Namespace-level quota configuration.
- `validate.rs` — Config validation logic.

### 10. Container runtime (`src/grill/`)

- `state.rs` (325 lines) — `ContainerState` enum: `Pending` --> `Preparing` --> `Starting` --> `HealthWait` --> `Running` --> `Unhealthy` --> `Stopping` --> `Stopped` --> `Failed`. This is the full lifecycle state machine.
- `process.rs` — `ProcessGrill`: spawns OS processes as workloads. Cross-platform fallback when you don't want a real container runtime. Uses `proc-grill:image-ignored` as a convention for process-based workloads.
- `port.rs` — `PortAllocator`: random port selection with retry cap to avoid collisions.
- `oci.rs` — OCI runtime spec generation for runc.
- `mock.rs` — `MockGrill` for testing the agent without spawning real processes.
- `apple.rs` — Apple Container support (macOS).
- `runc.rs` — runc container support (Linux).
- `rootless.rs`, `cgroup.rs`, `image.rs` — Supporting infrastructure.

## Critical data flows

### App deployment (end to end)

1. User runs `relish apply app.toml`.
2. CLI parses the TOML into a `Config` struct.
3. CLI calls `BunClient::apply()`, which POSTs to `/v1/apply`.
4. The API handler sends `AgentCommand::Deploy` over the `mpsc` channel.
5. The agent calls `supervisor.deploy_app()`, which creates a `WorkloadInstance` in `Pending` state.
6. The agent calls `grill.start()`. For `ProcessGrill`, this spawns a child process.
7. Instance transitions: `Pending` --> `Starting` --> `HealthWait`.
8. The health checker schedules the first probe after `initial_delay`.
9. On successful health check: `HealthWait` --> `Running` (healthy).
10. The API streams progress as SSE events back to the CLI.

### Cluster formation

1. Node 1 starts with an empty `join` list. It self-promotes to council leader (quorum of 1 is itself).
2. Node 2 starts with `join = ["node1:9443"]`. Gossip discovers node 1.
3. Gossip convergence: O(log N) protocol periods for all nodes to learn about each other.
4. The leader runs `select_council_candidates()` and promotes eligible nodes after they've been alive for `min_node_age` (600s default).
5. `ReportWorker` on each non-council node sends a `StateReport` every 5 seconds.
6. `ReportAggregator` on council members publishes via `watch` channel.

### Leader failure recovery

1. The leader's Raft heartbeats stop.
2. Remaining council members: election timeout (1-2s) fires. A new leader is elected.
3. The new leader calls `on_leader_elected(alive_count)`, entering the Learning phase.
4. It collects `StateReport`s until 95% of alive nodes have reported, or 15s timeout fires.
5. It runs `compute_diff(desired, actual)` to produce the corrections list.
6. It transitions to Active. Scheduling resumes.

## Test architecture

The test suite is the second codebase. You can learn almost as much from reading the tests as from reading the implementation.

**544 unit tests** in `#[cfg(test)] mod tests` at the bottom of each source file. They cover state machines, conflict resolution, scoring, filtering, serialisation round-trips, and edge cases.

**58 integration tests** across 7 files in `tests/`:

| File | Tests | What it covers |
|------|-------|---------------|
| `integration.rs` | 21 | Phase 1 lifecycle: deploy, health, stop, restart |
| `agent_cluster.rs` | 6 | Agent wiring with cluster handle |
| `scheduling.rs` | 5 | End-to-end scheduling (replicas, daemon, labels, quota) |
| `chaos.rs` | 2 | Council partition, worker isolation |
| `reconstruction.rs` | 1 | State reconstruction with real Raft |
| `reporting_tree.rs` | 2 | Reporting failover when council changes |
| `gossip_10k.rs` | 1 | 10,000-node convergence (ignored by default, slow) |

**Benchmarks** in `benches/`: gossip protocol performance via criterion.

**Property tests**: proptest in the gossip module for conflict resolution invariants.

A pattern you'll notice: most tests use in-memory transports and fast configs. The gossip tests use `InMemoryNetwork` with 20ms timeouts. The Raft tests use `InMemoryRaftRouter` with 50ms heartbeats. This keeps the test suite fast (under 3 seconds for all 544 lib tests) while still exercising the real protocol logic.

## Known limitations (deferred to later phases)

These are conscious choices, not bugs. Each one maps to a specific future phase.

- **No mTLS.** All transports use plain TCP/UDP. The HMAC field in gossip messages is zeroed. Phase 4 adds Sesame (the security layer).
- **No join token validation.** `relish join --token` parses the token but ignores it. Phase 4.
- **No deploy orchestration.** The scheduler places replicas, but there's no rolling update, canary, or blue-green logic. Phase 7.
- **No autoscaler.** The `autoscale` config field is parsed but not acted on. Phase 7.
- **No full council loss recovery.** If all council members die simultaneously, there's no pre-seeded recovery mechanism. Phase 4/8.
- **In-memory Raft storage.** Log and state machine live in memory. Restart = data loss. Persistent storage comes later.
- **Image locality scoring is a placeholder.** Returns 0 because there's no Pickle registry to check against until Phase 5.
- **Node stability scoring is a placeholder.** Returns 50 because there's no real uptime tracking yet.

Now go read `src/meat/types.rs`. Everything else will make more sense after that.

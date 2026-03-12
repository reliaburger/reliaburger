# Finding Friends

Your orchestrator runs on one machine. If that machine dies, everything dies with it. Your health checks keep containers alive on a single node, but they can't do anything about the node itself catching fire. You need more nodes.

This chapter turns Reliaburger from a single-node agent into a cluster. By the end, nodes will discover each other through a gossip protocol, elect a leader through Raft consensus, and distribute workloads across the cluster through a scheduler. It's the biggest architectural leap in the project.

We'll build three communication layers, each operating at a different scale:

1. **Mustard** — a gossip protocol based on SWIM, running on every node (up to 10,000). Handles membership discovery, failure detection, and metadata propagation.
2. **Raft** — consensus protocol running on a small council of 3–7 nodes. Handles leader election and replication of desired state (app specs, scheduling decisions).
3. **Hierarchical reporting tree** — worker nodes report runtime state to council members, who aggregate it for the leader. Keeps variable-size data off the gossip mesh.

Why three layers? Because a single protocol can't efficiently serve both 10,000 nodes and strong consistency. Gossip scales beautifully but gives you eventual consistency. Raft gives you strong consistency but only works with a handful of nodes. So we use gossip for the wide layer and Raft for the narrow one, then add the reporting tree to avoid bloating gossip messages with runtime state.

## Shared types

Before we write any protocol code, we need a shared vocabulary. The scheduler, gossip layer, and Raft module all talk about nodes, apps, and resources. If we let each module define its own types, we'll spend more time converting between them than doing useful work.

### Newtypes: NodeId vs InstanceId

In Chapter 1 we introduced `InstanceId` — a newtype wrapping `String` that identifies a container instance on a node. Now we need `NodeId` to identify nodes in the cluster. Both are strings underneath, but they mean completely different things. Rust's type system prevents us from accidentally passing one where the other is expected.

```rust
/// Unique identifier for a node in the cluster.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct NodeId(pub String);
```

Compare this to `InstanceId` from Chapter 1:

```rust
/// Unique identifier for a workload instance on this node.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct InstanceId(pub String);
```

Same pattern, different derives. `NodeId` has `Ord` and `PartialOrd` because we'll need to sort nodes for deterministic council selection. It also has `Serialize` and `Deserialize` because node identities cross the wire in gossip messages and Raft entries. `InstanceId` doesn't need any of that — it lives entirely within a single node.

This is the `Newtype(pub Inner)` pattern. The `pub` on the inner field means callers can still access the raw string when they need to (for logging, display, serialisation). But they can't accidentally use a `NodeId` where an `InstanceId` is expected. The compiler catches it.

If you've used Go, you'd get something similar with `type NodeId string` — except Go's type aliases are structurally typed, so you can still pass a plain `string` where a `NodeId` is expected without a cast. Rust won't let you.

### AppId: namespace-qualified identity

Apps need identifiers too, but a bare name isn't enough. Two teams might both deploy an app called "web" in different namespaces. So `AppId` combines the name and namespace:

```rust
#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct AppId {
    pub name: String,
    pub namespace: String,
}
```

This is a struct rather than a newtype because identity comes from two fields, not one. We derive `Hash` and `Eq` so it can be used as a `HashMap` key, and `Ord` for deterministic iteration in `BTreeMap`.

### Resources: the currency of scheduling

The scheduler needs to reason about CPU, memory, and GPUs. We represent these as a single struct with saturating arithmetic:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Resources {
    pub cpu_millicores: u64,
    pub memory_bytes: u64,
    pub gpus: u32,
}
```

The key method is `fits()`:

```rust
impl Resources {
    pub fn fits(&self, required: &Resources) -> bool {
        self.cpu_millicores >= required.cpu_millicores
            && self.memory_bytes >= required.memory_bytes
            && self.gpus >= required.gpus
    }
}
```

All arithmetic is saturating — `saturating_sub` and `saturating_add` — because scheduling calculations should never overflow or underflow. If a node has 4000m of CPU and you subtract 5000m, you get 0, not a panic. This is a deliberate design choice: the scheduler should degrade gracefully under overcommit, not crash.

Why `Copy`? `Resources` is 20 bytes (two `u64`s and a `u32`). That's small enough to copy cheaply, and it makes the code much cleaner — no borrowing gymnastics when passing resource values around in scheduling calculations.

### NodeCapacity: what a node can offer

The scheduler doesn't just need to know what a node *has*. It needs to know what's *available*:

```rust
pub struct NodeCapacity {
    pub node_id: NodeId,
    pub address: SocketAddr,
    pub total: Resources,
    pub reserved: Resources,
    pub allocated: Resources,
    pub labels: BTreeMap<String, String>,
}

impl NodeCapacity {
    pub fn allocatable(&self) -> Resources {
        self.total
            .saturating_sub(&self.reserved)
            .saturating_sub(&self.allocated)
    }
}
```

`allocatable = total - reserved - allocated`. The `reserved` portion is for the OS and the Bun agent itself (you don't want the scheduler filling a node so completely that the agent can't function). The `allocated` portion tracks what's already been assigned to workloads. The difference is what's left for new placements.

Labels are stored in a `BTreeMap` rather than a `HashMap` because we need deterministic serialisation. When the scheduler evaluates placement constraints like `region = "us-east"`, it needs to iterate labels in a consistent order across all nodes.

## The SWIM gossip protocol

Now for the interesting part. How do 10,000 nodes discover each other, detect failures, and propagate information — without a central registry?

The answer is SWIM: Scalable Weakly-consistent Infection-style Membership. The name is a mouthful, but the idea is elegant. Each node periodically pings a random peer. If the peer responds, great. If it doesn't, ask a few other nodes to try. If nobody can reach it, mark it suspect, and eventually declare it dead. Information about these state changes piggybacks on the ping/ack messages that are already flowing, so there's no extra network overhead.

### Node states

Every node in the cluster is in one of four states:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeState {
    Alive,
    Suspect,
    Dead,
    Left,
}
```

The transitions look like this:

```text
Alive ──> Suspect ──> Dead
  ▲          │
  └──────────┘  (refutation with higher incarnation)

Any ──> Left  (graceful departure)
```

`Alive` → `Suspect` happens when a node fails to respond to probes. `Suspect` → `Dead` happens after a timeout with no refutation. And `Suspect` → `Alive` happens when the suspected node hears about the suspicion and refutes it by bumping its incarnation number.

`Left` is special — it's a graceful departure. When a node shuts down cleanly, it announces "I'm leaving" and that overrides everything. Once a node has left, even a higher incarnation number can't bring it back.

### Incarnation numbers and conflict resolution

Here's the problem with distributed failure detection: information travels at the speed of gossip, and different nodes might have different views of the world at any given moment. Node A thinks Node B is alive. Node C thinks Node B is suspect. Who's right?

SWIM solves this with incarnation numbers. Each node maintains a monotonically increasing incarnation counter. When a node hears it's been suspected, it increments its incarnation and announces "I'm alive at incarnation N+1." The higher incarnation wins.

```rust
pub fn resolve_conflict(
    old_state: NodeState,
    old_incarnation: u64,
    new_state: NodeState,
    new_incarnation: u64,
) -> (NodeState, u64) {
    // Left is terminal
    if new_state == NodeState::Left {
        return (NodeState::Left, new_incarnation);
    }
    if old_state == NodeState::Left {
        return (NodeState::Left, old_incarnation);
    }

    if new_incarnation > old_incarnation {
        (new_state, new_incarnation)
    } else if new_incarnation < old_incarnation {
        (old_state, old_incarnation)
    } else {
        // Equal incarnation: more severe state wins
        if new_state.dissemination_priority() >= old_state.dissemination_priority() {
            (new_state, new_incarnation)
        } else {
            (old_state, old_incarnation)
        }
    }
}
```

Three rules:
1. Higher incarnation always wins.
2. At equal incarnation, the more severe state wins (`Dead > Suspect > Alive`).
3. `Left` always wins — graceful departure is final.

Rule 2 is important. If two updates arrive with the same incarnation — say, one marking a node Suspect and one marking it Alive — the Suspect update wins. This biases the protocol towards detecting failures rather than missing them. A false positive (marking a healthy node as suspect) is recoverable: the node just bumps its incarnation. A false negative (thinking a dead node is alive) isn't.

### The membership table

Each node maintains a local copy of every known member's state:

```rust
pub struct MembershipTable {
    members: HashMap<NodeId, NodeMembership>,
}
```

`NodeMembership` carries everything we know about a peer:

```rust
pub struct NodeMembership {
    pub node_id: NodeId,
    pub address: SocketAddr,
    pub state: NodeState,
    pub incarnation: u64,
    pub first_seen: Instant,
    pub last_ack: Instant,
    pub resources: Option<ResourceSummary>,
    pub labels: BTreeMap<String, String>,
    pub is_council: bool,
    pub is_leader: bool,
}
```

The table provides query methods that filter by state — `alive_members()`, `active_members()` (alive + suspect), `council_members()`, `leader()`. Each is a simple filter over the `HashMap`. Dead and left nodes stick around temporarily (so their state can be disseminated to nodes that haven't heard yet), then get cleaned up by `reap_dead()`.

One subtlety: when we receive a gossip update about a node we've never seen, we only add it if the state is `Alive`. We don't add unknown dead nodes — what would be the point? This prevents the table from accumulating records of nodes that died before we joined the cluster.

### Piggyback dissemination

SWIM doesn't use dedicated broadcast messages. Instead, membership updates piggyback on the PING/ACK messages that are already flowing. Each message carries up to `MAX_PIGGYBACK_UPDATES` (8) updates, and each update is broadcast `ceil(log2(cluster_size))` times before it expires.

Why `ceil(log2(N))`? Because SWIM's mathematical properties guarantee that an update will reach every node within O(log N) protocol periods with high probability. For a 10,000-node cluster, that's about 14 broadcasts per update — spread across 500ms protocol intervals, that's 7 seconds for full convergence. Not bad.

The dissemination queue prioritises failure-related updates:

```rust
impl NodeState {
    pub fn dissemination_priority(self) -> u8 {
        match self {
            NodeState::Dead | NodeState::Left => 3,
            NodeState::Suspect => 2,
            NodeState::Alive => 1,
        }
    }
}
```

Dead and suspect updates jump the queue. If the queue is full and we have to choose between telling people about a new join and telling them about a failure, the failure wins. Failure detection is time-sensitive; join announcements can wait.

### Message types

Every gossip message is a single UDP datagram, kept under 1400 bytes to avoid IP fragmentation:

```rust
pub struct GossipMessage {
    pub version: u8,
    pub sender: NodeId,
    pub incarnation: u64,
    pub hmac: [u8; 32],
    pub payload: GossipPayload,
}

pub enum GossipPayload {
    Ping { updates: Vec<MembershipUpdate> },
    PingReq { target: NodeId, requester: NodeId, updates: Vec<MembershipUpdate> },
    Ack { updates: Vec<MembershipUpdate> },
}
```

Three message types, each carrying piggybacked updates. `Ping` is a direct probe. `PingReq` asks a third party to probe on your behalf (the indirect probe). `Ack` is the response. That's the entire protocol vocabulary.

The `hmac` field is zeroed out for now. Phase 4 will fill it in with HMAC-SHA256 computed from the node's mTLS certificate, so nodes can verify that gossip messages are authentic. The field exists from day one so the message format doesn't change later.

## The transport layer

We have the data structures. Now we need a way to send messages between nodes.

The core idea: define a trait for "something that can send and receive gossip messages", then provide two implementations. The real one sends UDP datagrams across the network. The test one routes messages between nodes in the same process. Same interface, completely different plumbing underneath. If you've used Go's interfaces or C++'s virtual classes, this is the same pattern — except Rust checks it at compile time, not runtime.

### The MustardTransport trait

```rust
pub trait MustardTransport: Send + Sync {
    fn send(
        &self,
        target: SocketAddr,
        message: &GossipMessage,
    ) -> impl std::future::Future<Output = Result<(), MustardError>> + Send;

    fn recv(&self)
    -> impl std::future::Future<Output = Option<(SocketAddr, GossipMessage)>> + Send;
}
```

Two methods: `send` a message to an address, `recv` the next inbound message. That's all.

The return types look unusual if you're coming from Go or Python. Instead of `async fn send(...)`, we write `fn send(...) -> impl Future<...> + Send`. This is called Return Position Impl Trait in Traits (RPITit), and it landed in Rust edition 2024. Before this, you needed the `async_trait` crate, which heap-allocated every returned future by wrapping it in a `Box<dyn Future>`. RPITit avoids that allocation entirely — the compiler knows the concrete future type at compile time and can inline it.

The `Send + Sync` bound on the trait itself means any transport can be shared across async tasks. `Send` means it can move between threads. `Sync` means multiple threads can hold references to it simultaneously. These bounds are the backbone of Rust's thread safety story — the compiler won't let you share something across threads unless the type proves it's safe.

### InMemoryTransport: the test double

We don't want our gossip tests touching the network. They'd be slow, flaky, and impossible to control. Instead, we build an in-memory network that routes messages between nodes in the same process:

```rust
pub struct InMemoryNetwork {
    inner: Arc<Mutex<NetworkInner>>,
}

struct NetworkInner {
    inboxes: HashMap<SocketAddr, mpsc::Sender<(SocketAddr, GossipMessage)>>,
    partitions: Vec<(SocketAddr, SocketAddr)>,
}
```

Each node registers with the network and gets a mailbox (an `mpsc::Sender`). When node A sends to node B, the network looks up B's mailbox and drops the message in. If there's a partition between A and B, the message is silently dropped — exactly what a real network partition looks like from the sender's perspective.

The `Arc<Mutex<...>>` pattern is worth understanding. `Arc` is Rust's atomically reference-counted pointer — it's how you share ownership across multiple async tasks. `Mutex` (from tokio, not std) guards the inner state so only one task accesses it at a time. If you've used Go's `sync.Mutex`, it's the same idea, except Rust enforces it at compile time: you literally cannot access the inner data without holding the lock.

Why `tokio::sync::Mutex` and not `std::sync::Mutex`? Because `std::sync::Mutex` blocks the OS thread while waiting for the lock. In an async runtime, that blocks the entire executor thread, starving other tasks. Tokio's mutex yields to the runtime while waiting, so other tasks can make progress. This is a footgun that catches every Rust async beginner at least once.

### Partition injection for chaos testing

The `InMemoryNetwork` supports injecting partitions:

```rust
pub async fn partition(&self, a: SocketAddr, b: SocketAddr) {
    let mut inner = self.inner.lock().await;
    inner.partitions.push((a, b));
    inner.partitions.push((b, a));
}

pub async fn heal(&self) {
    let mut inner = self.inner.lock().await;
    inner.partitions.clear();
}
```

Bidirectional by default. Call `partition(a, b)` and neither node can reach the other. Call `heal()` to restore connectivity. This will become essential in the chaos tests we write later.

## The SWIM probe cycle

With the transport in hand, we can now build the actual protocol driver: `MustardNode`.

### MustardNode: the protocol engine

```rust
pub struct MustardNode<T: MustardTransport> {
    pub node_id: NodeId,
    pub address: SocketAddr,
    pub incarnation: u64,
    pub membership: MembershipTable,
    pub dissemination: DisseminationQueue,
    pub config: GossipConfig,
    transport: T,
    lamport: u64,
}
```

The generic parameter `T: MustardTransport` means we can construct a `MustardNode<InMemoryTransport>` for testing and a `MustardNode<UdpTransport>` for production, using the exact same protocol code. The transport is the only thing that changes.

If you're coming from Go, this is like defining a struct with an interface field, except Rust monomorphises it — the compiler generates specialised code for each concrete transport type. No virtual dispatch, no heap allocation for the trait object. In a gossip protocol running thousands of cycles per second, that matters.

### The event loop

The main loop uses `tokio::select!` to multiplex between three events:

```rust
pub async fn run(&mut self, shutdown: CancellationToken) {
    let mut interval = tokio::time::interval(self.config.protocol_interval);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {
                self.run_one_cycle().await;
            }
            msg = self.transport.recv() => {
                if let Some((from, message)) = msg {
                    self.handle_message(from, message).await;
                }
            }
        }
    }
}
```

`tokio::select!` is Rust's answer to Go's `select {}`. It waits for whichever event fires first: the shutdown signal, the next protocol interval tick, or an incoming message. Unlike Go's `select`, Tokio's version cancels the other futures when one completes — no leaked goroutines.

The `CancellationToken` pattern is how we do graceful shutdown. Every long-lived task gets a token. When the parent calls `cancel()`, all tasks break out of their loops and clean up. It's cleaner than the `done := make(chan struct{})` pattern in Go because it composes — child tasks can create child tokens that cancel when the parent does.

### A single probe cycle

Each protocol interval, the node runs one probe cycle:

1. **Promote expired suspects.** Any node that's been suspected longer than `suspicion_timeout` without refuting gets declared dead.

2. **Pick a random target.** Select one alive or suspect peer at random (not ourselves). Random target selection is what gives SWIM its scalability: every node does O(1) work per interval regardless of cluster size.

3. **Send a PING.** The PING carries piggybacked membership updates from the dissemination queue.

4. **Wait for ACK.** If the target responds within `probe_timeout`, we're done. While waiting, we keep processing incoming messages — including PINGs from other nodes that need responses.

5. **Indirect probing.** If no direct ACK arrives, we send PING-REQ to a few random relay nodes, asking them to probe the target on our behalf. This distinguishes "the target is down" from "the network between us and the target is down."

6. **Mark as suspect.** If neither the direct nor indirect probes produce an ACK, the target is marked suspect and a Suspect update is enqueued for dissemination.

### Handling incoming messages

When a message arrives, we process it in three stages:

**First,** register the sender. If this is a node we've never seen, add it to our membership table and enqueue an Alive update for dissemination. This is how new nodes propagate through the cluster — each direct contact generates a dissemination update that ripples outward through subsequent PING/ACK exchanges.

**Second,** apply piggybacked updates. Each update goes through the incarnation-based conflict resolution we defined earlier. If a piggybacked update says *we* are suspected, we refute it immediately by bumping our incarnation and enqueuing an Alive update.

**Third,** handle the message type:
- **PING:** reply with ACK, piggybacking our own queued updates.
- **PING-REQ:** probe the specified target on behalf of the requester.
- **ACK:** update the sender's last-ack timestamp.

### Refutation

When a node discovers it's been suspected, it needs to act fast:

```rust
fn refute(&mut self) {
    self.incarnation += 1;
    self.dissemination.enqueue(
        MembershipUpdate {
            node_id: self.node_id.clone(),
            address: self.address,
            state: NodeState::Alive,
            incarnation: self.incarnation,
            ..
        },
        self.membership.len(),
    );
}
```

Bump incarnation, then broadcast "I'm alive at a higher incarnation." Since higher incarnations always win in conflict resolution, this overrides the Suspect update across the entire cluster. The refutation update jumps onto the very next outgoing ACK message, so it propagates quickly.

### Testing convergence

The most satisfying test: five nodes arranged in a ring, where each only knows its immediate neighbour. Can gossip propagate membership information to every node?

```rust
#[tokio::test(start_paused = true)]
async fn gossip_convergence_five_nodes() {
    // ...setup 5 nodes in a ring...
    tokio::time::sleep(Duration::from_secs(2)).await;
    shutdown.cancel();

    for node in &final_nodes {
        let alive_count = node.membership.active_members().len();
        assert_eq!(alive_count, 5);
    }
}
```

The `start_paused = true` annotation deserves attention. It tells Tokio to use a fake clock that only advances when all tasks are idle. When we call `tokio::time::sleep(Duration::from_secs(2))`, no real time passes — the runtime skips forward instantly. This makes the test both fast and deterministic. No more "wait 500ms and hope the gossip converges" flakiness.

The test passes because of the dissemination mechanism we built. When n0 pings n1, n1 learns about n0 and enqueues a dissemination update. When n1 later pings n2, that update piggybacks on the PING. Within a few cycles, information about every node has rippled through the ring. The `MembershipUpdate` struct carries the node's address alongside its state, so nodes discovered via gossip (not direct contact) know how to reach each other.

### What's next

We have a working gossip protocol. Nodes discover each other, detect failures, and propagate membership changes. But gossip only gives us eventual consistency — every node eventually agrees on who's alive, but there's no single source of truth. For scheduling decisions, app deployments, and configuration changes, we need something stronger. That's where Raft comes in.

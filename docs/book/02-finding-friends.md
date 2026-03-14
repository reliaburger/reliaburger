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

### Dead node cleanup

Nodes transition through Alive → Suspect → Dead, but what happens to Dead entries in the membership table? Without cleanup they accumulate forever, wasting memory and slowing down iteration.

The SWIM paper doesn't say much about this — it's an implementation detail. Our approach: track when each node last changed state via a `state_changed` timestamp on `NodeMembership`, then periodically remove Dead and Left nodes whose `state_changed` is older than `cleanup_timeout` (60 seconds by default).

Why 60 seconds? At 500ms gossip intervals, a 10,000-node cluster needs about `3 × ceil(log2(10000))` = 42 rounds (21 seconds) for an update to reach every node. 60 seconds gives nearly three times that margin. We don't want to reap a Dead node before all other nodes have learned about its death, because a late-arriving node might add it back as Alive from stale gossip.

The reap runs every probe cycle (500ms). Iterating the membership table and comparing timestamps is trivial — even at 10,000 nodes it takes microseconds.

### Graceful leave

When a node shuts down on purpose (as opposed to crashing), it's wasteful for the rest of the cluster to go through the full Suspect → Dead → reap dance. That takes `suspicion_timeout` (5s) plus `cleanup_timeout` (60s) — over a minute of uncertainty.

Instead, a departing node broadcasts a `Left` update. The `Left` state is terminal and sticky: once a node is Left, no amount of Alive updates at any incarnation can bring it back (see `resolve_conflict` in `state.rs`). This means the departure propagates cleanly through the cluster without any ambiguity.

```rust
pub async fn leave(&mut self) {
    // Mark ourselves as Left
    if let Some(member) = self.membership.get_mut(&self.node_id) {
        member.state = NodeState::Left;
        member.state_changed = now;
    }
    // Enqueue for dissemination + send to up to 10 peers
    // ...
}
```

The `run()` loop calls `leave()` automatically when the `CancellationToken` fires:

```rust
_ = shutdown.cancelled() => {
    self.leave().await;
    break;
}
```

This is fire-and-forget: the node can't stick around waiting for acknowledgement because it's shutting down. But that's fine. The dissemination queue ensures the update gets broadcast `3 × ceil(log2(N))` times, and we send an immediate burst to up to 10 peers to maximise the chance that it reaches enough of the cluster on the first try. Even if it only reaches one node, that node will re-disseminate it.

### Coming back after maintenance

So what happens when the node finishes maintenance and rejoins the cluster?

Left is terminal. No Alive update, at any incarnation number, can override it. This is deliberate: if a node said "I'm leaving," we don't want stale gossip from before the departure to accidentally resurrect it. The other nodes must agree that the node is gone before it can come back.

The mechanism that makes this work is the cleanup timeout. After 60 seconds in the Left state, `reap_expired_dead` removes the entry from every node's membership table. When the returning node sends its first PING, none of its peers recognise it — so `add_node` creates a fresh entry with state Alive. From the cluster's perspective, it's a brand new node joining for the first time.

The `add_node` method enforces this explicitly:

```rust
Entry::Occupied(mut entry) => {
    let m = entry.get_mut();
    // Left is terminal — a returning node must wait for the
    // cleanup timeout to reap the old entry before rejoining.
    if m.state == NodeState::Left {
        return false;
    }
    // ...
}
```

This means there's a minimum rejoin delay of `cleanup_timeout` (60 seconds). In practice that's fine. You're not going to reboot a server and have it back in under a minute. And if you do, the 60-second cooldown actually helps: it prevents flapping where a misconfigured node repeatedly joins and leaves, generating a storm of membership updates.

If you absolutely need faster rejoins (testing, development), reduce `cleanup_timeout`. The only constraint is that it must be long enough for the Left update to propagate to all nodes — at least a few seconds for any reasonable cluster size.

### Testing convergence

The most satisfying test: five nodes arranged in a ring, where each only knows its immediate neighbour. Can gossip propagate membership information to every node?

Our first instinct was to spawn all five nodes as concurrent tokio tasks and let them run for a while:

```rust
// The naive approach — DON'T DO THIS
#[tokio::test(start_paused = true)]
async fn gossip_convergence_five_nodes() {
    // ...spawn 5 nodes...
    tokio::time::sleep(Duration::from_secs(2)).await;
    shutdown.cancel();
    // ...check all nodes know about all others...
}
```

This looks clean. The `start_paused = true` annotation tells tokio to use a fake clock — time only advances when all tasks are idle. No real seconds pass. `sleep(2s)` completes instantly. Deterministic, right?

Wrong. It passed in isolation but failed randomly when run alongside our other 400 tests. Can you see why?

### Tokio's paused time: powerful but tricky

`start_paused = true` forces the `current_thread` runtime flavour. All spawned tasks run cooperatively on a single OS thread, only making progress when they yield at `.await` points. The runtime decides which task to poll next, and that order is non-deterministic.

Here's where it falls apart. When we call `tokio::time::sleep(2s)`, tokio auto-advances the clock to the nearest pending timer. But with five spawned tasks all using `tokio::time::interval` and `tokio::time::timeout`, the clock advances can fire timeouts before channel messages have been processed. Node A sends a PING to Node B, but the runtime polls Node A's timeout before it polls Node B's recv. Node A concludes B is dead. Oops.

This is a known issue ([tokio #3709](https://github.com/tokio-rs/tokio/issues/3709)): there's no ordering guarantee between a task that advances time and other tasks whose timers have expired. Under parallel test load (400 other tests competing for CPU), the problem gets worse because the single-threaded runtime gets less scheduling time.

It gets sneakier. Tokio uses a process-wide static flag (`DID_PAUSE_CLOCK`) to track whether any test has ever paused the clock. Once set, it stays set for the entire process lifetime, changing the code path for `Instant::now()` in every subsequent test — even ones that don't use paused time.

The lesson: `start_paused = true` works brilliantly for testing a single task's timer logic. It breaks down when you need multiple spawned tasks to interact through channels and timers simultaneously. Go's `testing` package doesn't have this problem because goroutines are preemptively scheduled. Tokio's cooperative scheduling is a different beast.

### Manual protocol driving

The fix: don't spawn concurrent tasks. Drive the protocol manually from the test, separating PING sending from message processing:

```rust
#[tokio::test]
async fn gossip_convergence_five_nodes() {
    // ...setup 5 nodes in a ring...

    for _ in 0..50 {
        // Phase 1: each node sends a PING to a random peer
        for node in &mut nodes {
            if let Some((_, target_addr)) = node.pick_probe_target() {
                let updates = node.dissemination.select_updates();
                let ping = GossipMessage::new(/* ... */);
                let _ = node.transport.send(target_addr, &ping).await;
            }
        }

        // Phase 2+3: drain messages twice (PINGs then ACKs)
        for _ in 0..2 {
            for node in &mut nodes {
                while let Some((from, msg)) = node.transport.try_recv() {
                    node.handle_message(from, msg).await;
                }
            }
        }
    }

    for node in &nodes {
        assert_eq!(node.membership.active_members().len(), 5);
    }
}
```

Each round, every node sends one PING, then we drain all inboxes twice: first to process PINGs (which generate ACKs), then to process ACKs (which apply piggybacked updates). No timers, no spawned tasks, no flakiness. The `try_recv()` method returns immediately if the channel is empty, so no clock manipulation is needed at all.

Why 50 rounds? Because gossip propagation depends on random target selection, and with a ring topology each node initially knows only one peer. Information has to hop through intermediaries. The minimum broadcast count of 3 ensures updates survive long enough during early cluster formation when the cluster is small, but random target selection means some rounds are "wasted" pinging a node that already knows the update. 50 rounds gives enough margin for even the unluckiest random sequences.

The test passes because of the dissemination mechanism. When n0 pings n1, n1 learns about n0 and enqueues a dissemination update. When n1 later pings n2, that update piggybacks on the PING. n2 receives it, re-enqueues it for further dissemination, and the ripple continues. The `MembershipUpdate` struct carries the node's address alongside its state, so nodes discovered via gossip (not direct contact) know how to reach each other.

### Benchmarking with criterion

In Go, you'd write `func BenchmarkFoo(b *testing.B)` and `go test -bench .` gives you nanoseconds-per-operation. Rust's built-in `#[bench]` exists but is nightly-only. The stable equivalent is the `criterion` crate, which goes further: statistical analysis, confidence intervals, and automatic regression detection between runs.

```rust
fn bench_single_gossip_round(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("single_gossip_round", |b| {
        b.iter_custom(|iters| {
            rt.block_on(async {
                // ...setup two nodes...
                let start = Instant::now();
                for _ in 0..iters {
                    // Send PING, process it, send ACK, process it
                }
                start.elapsed()
            })
        });
    });
}
```

A few things to notice. First, `iter_custom` lets us control the timing loop ourselves, which we need because setting up the tokio runtime and network shouldn't be measured. Second, `rt.block_on()` runs async code inside a synchronous benchmark — criterion doesn't natively understand async, so we bridge the gap manually. Third, criterion automatically determines how many iterations to run. No `b.N` equivalent to worry about.

We benchmark four things:

1. **Transport throughput** — raw InMemoryTransport send + recv. ~115ns per message, or about 8.7 million messages per second. This tells us the test infrastructure isn't the bottleneck.

2. **Single gossip round** — full PING → process → ACK → process cycle between two nodes. ~520ns. This is the per-node cost of one protocol period.

3. **Convergence scaling** — how long until all nodes in a ring know about all others, measured across cluster sizes from 5 to 250 nodes. This validates SWIM's theoretical O(log N) convergence guarantee and catches regressions in the dissemination logic.

4. **Dissemination queue** — enqueue 20 updates and drain them through the priority queue. ~38µs. The `BinaryHeap` ordering (Dead > Suspect > Alive) is the hot path for every outgoing message. This is higher than you might expect because each update gets broadcast `3 * ceil(log2(N))` times (the SWIM lambda parameter), so the queue does more work draining 20 updates with a cluster size of 100.

The convergence benchmark is the most interesting. Here's what we measured:

| Nodes | Time |
|------:|-----:|
| 5 | 35 µs |
| 10 | 133 µs |
| 25 | 1.1 ms |
| 50 | 5.7 ms |
| 100 | 30 ms |
| 250 | 337 ms |

These numbers improved dramatically when we switched from `ceil(log2(N))` to `3 * ceil(log2(N))` for the broadcast count. The SWIM paper calls this multiplier *lambda*. With lambda=1, updates expired before reaching all nodes, forcing extra gossip rounds. With lambda=3, updates survive long enough that convergence happens in fewer rounds — a case where doing more work per round means less total work. The scaling is roughly O(N² log N) in the benchmark, because we're simulating all N nodes sequentially. In a real deployment, nodes run concurrently, so the wall-clock convergence time is just O(log N) protocol intervals — about 3.5 seconds for a 1000-node cluster at 500ms intervals.

Run `cargo bench --bench gossip` to check for regressions. Criterion stores previous results in `target/criterion/` and reports whether performance changed. If you accidentally introduce an O(N²) loop where O(N) was expected, the benchmark will catch it before any user does.

For larger clusters, `cargo bench --bench gossip_large` tests 500 and 1000 nodes (~10 minutes). And for the ultimate validation — 10,000 nodes, matching the whitepaper's scalability target — there's `make bench-10k`. That one takes about an hour because we're simulating all 10k nodes sequentially on a single thread. It's not something you run every commit, but it proves the protocol converges at the scale we promised. The test prints progress every 50 rounds so you can watch membership knowledge spread through the cluster in real time.

### What's next

We have a working gossip protocol. Nodes discover each other, detect failures, and propagate membership changes. But gossip only gives us eventual consistency — every node eventually agrees on who's alive, but there's no single source of truth. For scheduling decisions, app deployments, and configuration changes, we need something stronger. That's where Raft comes in.

## Raft consensus

Gossip tells every node who's in the cluster. It doesn't tell them what should be running. If two nodes both think they're the scheduler, they'll make conflicting placement decisions — and now you have two copies of an app that should only have one, or zero copies of an app that should have two. You need a single source of truth, and you need agreement on who gets to update it.

That's consensus. Specifically, we use Raft, a protocol designed to be understandable. The original paper by Diego Ongaro and John Ousterhout ("In Search of an Understandable Consensus Algorithm", 2014) is worth reading in full — it's one of those rare academic papers that's actually pleasant to get through.

Here's the 60-second version. A Raft cluster has a leader and some followers. The leader accepts writes, appends them to a log, and replicates that log to followers. Once a majority (a quorum) acknowledges an entry, it's committed — guaranteed to survive crashes. If the leader dies, the remaining nodes hold an election. The candidate with the most up-to-date log wins. The new leader picks up where the old one left off. Clients only talk to the leader for writes; any node can serve reads.

We don't need Raft on all 10,000 nodes. Consensus is expensive: every write requires a round trip to a majority. We run Raft on a small "council" of 3 to 7 nodes, separate from the gossip layer. The council replicates desired state — which apps should run, where they should be placed, cluster-wide config. The other nodes learn about this through gossip and the reporting tree (covered later in this chapter).

### Standing on the shoulders of openraft

We're not implementing Raft from scratch. The openraft crate (v0.9) is a mature, async-native, tokio-compatible implementation with pre-vote support and a clean trait-based adapter pattern. Implementing Raft correctly is notoriously fiddly — edge cases around log compaction, split votes, and pre-vote alone would cost us weeks. openraft handles all of that and has been battle-tested by other projects.

What we do implement are three adapter traits that tell openraft how to store logs, apply entries to our state machine, and send messages between nodes:

1. **`RaftLogStorage`** — where the log entries and vote records live. We use an in-memory `BTreeMap` for now. Production would back this with disk.
2. **`RaftStateMachine`** — what happens when a committed entry gets applied. Our state machine maintains `DesiredState`: the set of apps, scheduling placements, and configuration the cluster should converge towards.
3. **`RaftNetworkFactory` + `RaftNetwork`** — how to send RPCs (append entries, vote requests, snapshot transfers) to other nodes. We start with an in-memory router for testing; TCP comes later.

The openraft API revolves around a type configuration macro:

```rust
openraft::declare_raft_types!(
    pub TypeConfig:
        D            = RaftRequest,
        R            = CouncilResponse,
        NodeId       = u64,
        Node         = CouncilNodeInfo,
        Entry        = openraft::Entry<TypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
);
```

This macro generates a type bundle that threads through the entire openraft API. `D` is the data you write to the log. `R` is what the state machine returns after applying an entry. `NodeId` and `Node` identify cluster members. `Entry` and `SnapshotData` control the wire format. If you've used associated types in Rust traits before, this is the same idea — just bundled into one declaration.

### The u64 problem

Our `NodeId` from the shared types module is a `String` newtype:

```rust
pub struct NodeId(pub String);
```

openraft requires its `NodeId` to be `Copy`. That's a hard constraint — the protocol needs to cheaply duplicate node IDs everywhere, and `Copy` guarantees this happens without allocation. `String` can't be `Copy` because it owns heap memory. Making a `String` `Copy` would mean copying the pointer without duplicating the buffer — a double-free waiting to happen. Rust won't let you.

So we use `u64` for Raft's internal node IDs and carry the human-readable name in a separate struct:

```rust
pub struct CouncilNodeInfo {
    pub addr: SocketAddr,
    pub name: String,
}
```

openraft attaches `CouncilNodeInfo` to each node in the membership. When we need to display node names in logs or map between the gossip layer (which uses `NodeId(String)`) and the Raft layer (which uses `u64`), the info struct is right there.

This is a pattern you'll hit often in Rust: a library requires a trait bound your type doesn't satisfy, so you introduce an adapter. In Go, you'd just pass an `int64` and a `string` separately and hope nobody mixes them up. In Rust, the type system makes the relationship explicit — `u64` is the identity, `CouncilNodeInfo` is the metadata, and openraft's `Node` trait ties them together.

### What goes in the log

Every mutation to the cluster's desired state is a `RaftRequest`:

```rust
pub enum RaftRequest {
    AppSpec { app_id: AppId, spec: Box<AppSpec> },
    AppDelete { app_id: AppId },
    SchedulingDecision(SchedulingDecision),
    ConfigSet { key: String, value: String },
    Noop,
}
```

`AppSpec` registers or updates an app. `AppDelete` removes one. `SchedulingDecision` records where replicas should run. `ConfigSet` handles cluster-wide key-value config. `Noop` exists for leader election — when a new leader takes over, it commits a no-op to establish its authority and advance the commit index.

Notice the `Box<AppSpec>` on the first variant. `AppSpec` is a large struct (800+ bytes with all its optional fields), while other variants are 72 bytes or less. Without the `Box`, clippy warns about `large_enum_variant` — the enum is sized for its largest variant, so every `Noop` would waste 800 bytes of stack space. Boxing the large payload puts it on the heap, so the enum itself stays small.

The state machine applies these entries in log order:

```rust
fn apply_request(&mut self, request: &RaftRequest) {
    match request {
        RaftRequest::AppSpec { app_id, spec } => {
            self.state.apps.insert(app_id.clone(), *spec.clone());
        }
        RaftRequest::AppDelete { app_id } => {
            self.state.apps.remove(app_id);
            self.state.scheduling.remove(app_id);
        }
        RaftRequest::SchedulingDecision(decision) => {
            self.state.scheduling
                .insert(decision.app_id.clone(), decision.placements.clone());
        }
        RaftRequest::ConfigSet { key, value } => {
            self.state.config.insert(key.clone(), value.clone());
        }
        RaftRequest::Noop => {}
    }
}
```

Exhaustive `match` — add a new variant and the compiler forces you to handle it everywhere. Compare that to a `switch` in Go, where a forgotten `case` silently falls through to nothing.

### Snapshots and the JSON key problem

When a follower falls far behind the leader's log, re-sending thousands of entries one by one is wasteful. Raft handles this with snapshots: the leader serialises the current state machine into a blob and sends it in one shot. The follower installs the snapshot and picks up replication from there.

We serialise `DesiredState` to JSON. Simple, debuggable, good enough for testing. One wrinkle: `DesiredState` has `HashMap<AppId, AppSpec>` fields, and JSON requires object keys to be strings. `AppId` is a struct with `name` and `namespace` fields. Serialising it as a JSON key would require a custom string representation that's also reversible. Instead, we sidestep the problem:

```rust
mod map_as_vec {
    pub fn serialize<K, V, S>(map: &HashMap<K, V>, serializer: S)
        -> Result<S::Ok, S::Error>
    where
        K: Serialize + Eq + Hash,
        V: Serialize,
        S: Serializer,
    {
        let vec: Vec<(&K, &V)> = map.iter().collect();
        vec.serialize(serializer)
    }

    pub fn deserialize<'de, K, V, D>(deserializer: D)
        -> Result<HashMap<K, V>, D::Error>
    where
        K: Deserialize<'de> + Eq + Hash,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        let vec: Vec<(K, V)> = Vec::deserialize(deserializer)?;
        Ok(vec.into_iter().collect())
    }
}
```

This serialises `HashMap<AppId, AppSpec>` as a JSON array of `[key, value]` pairs instead of a JSON object. It's applied with serde's field-level attributes:

```rust
pub struct DesiredState {
    #[serde(serialize_with = "map_as_vec::serialize",
            deserialize_with = "map_as_vec::deserialize")]
    pub apps: HashMap<AppId, AppSpec>,
    // ...
}
```

The `'de` lifetime on the deserialise function is serde's way of tracking the lifetime of the input data. If you're deserialising from a borrowed `&str`, the `K` and `V` types could borrow from that string (if they contain `&str` fields). The `'de` lifetime makes this safe. Our types all own their data (they use `String`, not `&str`), so the lifetime doesn't change runtime behaviour — but serde's trait bounds require it regardless.

### The in-memory log store

The log store is the simplest adapter. It stores entries in a `BTreeMap<u64, Entry>` keyed by log index:

```rust
struct LogStoreInner {
    vote: Option<Vote<u64>>,
    committed: Option<LogId<u64>>,
    log: BTreeMap<u64, Entry<TypeConfig>>,
    last_purged_log_id: Option<LogId<u64>>,
}
```

Why `BTreeMap` instead of `Vec`? After a snapshot purge, the log has a gap — indices 1 through 1000 get deleted, and the log starts at 1001. A `Vec` would either waste memory on empty slots or need index arithmetic. `BTreeMap` handles sparse indices naturally, gives us O(log N) lookups, and makes range queries trivial with `.range()`.

The `vote` field tracks which candidate this node voted for in the current term. Raft requires this to be durable — if a node votes for candidate A, crashes, and restarts, it must not accidentally vote for candidate B in the same term. In our in-memory store, "durable" means "until the process dies", which is fine for tests.

The `last_purged_log_id` is what you need after compaction. When the state machine takes a snapshot at log index 1000, entries 1 through 1000 can be purged. But we still need to know that the log *started* at 1001, so the field records where we left off.

### Testing consensus without a network

We tested gossip with `InMemoryNetwork` — a fake transport that routes messages between nodes in the same process. We use the same trick for Raft, but the shape is different. Gossip sends datagrams. Raft sends RPCs: request-response pairs where the caller blocks until the target responds.

The `InMemoryRaftRouter` holds a map of Raft handles:

```rust
pub struct InMemoryRaftRouter {
    rafts: Arc<Mutex<HashMap<u64, Raft<TypeConfig>>>>,
    partitions: Arc<Mutex<HashSet<(u64, u64)>>>,
}
```

When node 1 wants to send an `AppendEntries` RPC to node 3, it looks up node 3's `Raft<TypeConfig>` handle and calls its method directly. No serialisation, no TCP, no latency. The response comes back as the return value of a function call. This makes tests fast and deterministic.

The `partitions` set simulates network partitions. If `(1, 3)` is in the set, messages from node 1 to node 3 return `Unreachable`. Partitions are bidirectional — adding `(1, 3)` also adds `(3, 1)`.

One subtlety: `Raft<TypeConfig>` doesn't implement `Debug`. Rust requires `Debug` for `HashMap` values if you want to derive `Debug` on the containing struct. So we write a manual `Debug` impl:

```rust
impl fmt::Debug for InMemoryRaftRouter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InMemoryRaftRouter")
            .field("num_rafts", &"<opaque>")
            .field("partitions", &self.partitions)
            .finish()
    }
}
```

In Go, `fmt.Sprintf("%+v", router)` would just print the pointer. In Rust, you have to be explicit about what you can and can't display. It's more work upfront, but you never get surprised by accidental sensitive data in logs.

### The CouncilNode wrapper

Raw openraft is powerful but verbose. `CouncilNode` wraps it into the API the rest of Reliaburger actually needs:

```rust
pub struct CouncilNode {
    raft: Raft<TypeConfig>,
    raft_id: u64,
    state_machine: CouncilStateMachine,
}
```

The key methods:

- **`initialize(members)`** — called once on the very first node, with itself as the sole member. It becomes leader immediately (quorum of 1 = itself).
- **`write(request)`** — submits a `RaftRequest` to the leader. Returns `ForwardToLeader` if this node isn't in charge.
- **`add_learner(id, info)`** — adds a node that receives log replication but doesn't vote yet.
- **`change_membership(members)`** — promotes learners to voters, changing the quorum.
- **`desired_state()`** — reads the current state machine. Any node can serve this, not just the leader.

The `write` method shows a common pattern for wrapping library error types:

```rust
pub async fn write(&self, request: RaftRequest)
    -> Result<CouncilResponse, CouncilError>
{
    let result = self.raft.client_write(request).await;
    match result {
        Ok(resp) => Ok(resp.data),
        Err(e) => match e {
            RaftError::APIError(ClientWriteError::ForwardToLeader(fwd)) => {
                Err(CouncilError::ForwardToLeader {
                    leader: fwd.leader_id,
                })
            }
            other => Err(CouncilError::WriteFailed(other.to_string())),
        },
    }
}
```

We match on the specific error variant we care about (`ForwardToLeader`) and flatten everything else into a generic string. This is deliberate — the caller needs to know whether to retry on a different node, but doesn't need to distinguish between the dozen other failure modes openraft defines.

### Bootstrap: the loneliest node

How does a cluster start from nothing? If you need a majority to elect a leader, and you start with zero nodes, you're stuck. Raft solves this with an explicit initialisation step.

The first node starts a 1-member Raft group:

```rust
let mut members = BTreeMap::new();
members.insert(1, node_info(1));
nodes[0].initialize(members).await.unwrap();
```

With one member, quorum is 1 (itself), so it immediately becomes leader. It can accept writes — deploy an app, set config, record scheduling decisions. There's no "pre-Raft" special mode. From the very first node, all state changes go through Raft. One code path, always.

The tradeoff: a 1-node council has zero fault tolerance. If that node dies, everything is gone. But that's inherent to having one physical machine. You can't tolerate failures you don't have redundancy for. As soon as a second and third node join, the council grows and real fault tolerance kicks in.

### Growing the council

Growth happens in two steps: add a learner, then promote it to voter.

```rust
// Leader adds node 2 as a learner.
leader.add_learner(2, node_info(2)).await.unwrap();

// Node 2 catches up on the log via replication...

// Leader promotes all three to voters.
leader.change_membership(BTreeSet::from([1, 2, 3])).await.unwrap();
```

Why the learner phase? If you immediately add a new node as a voter, it has an empty log. The cluster now needs this node's vote for quorum, but the node can't meaningfully participate — it doesn't know what's been committed. Worse, the cluster might lose availability while the new node downloads gigabytes of log. The learner phase lets the new node receive replication and catch up *before* it gets a vote.

The progression is:

1. **1 node** — leader by definition. Quorum = 1. Zero fault tolerance, but you can write.
2. **2 nodes** — quorum = 2. Losing either blocks writes. Still fragile.
3. **3 nodes** — quorum = 2. Losing one node is survivable. This is the minimum production topology.
4. **5 nodes** — quorum = 3. Tolerates 2 failures.
5. **7 nodes** — quorum = 4. Tolerates 3 failures. This is our upper bound.

Beyond 7, more council members means slower commits (more nodes to wait for) without meaningful benefit. Additional nodes join the gossip layer but stay out of the council.

### Testing the full lifecycle

The integration tests exercise the complete lifecycle: bootstrap, growth, replication, failover, and partitions. Here's the pattern they all follow:

```rust
async fn create_cluster(n: u64) -> (Vec<CouncilNode>, InMemoryRaftRouter) {
    let router = InMemoryRaftRouter::new();
    let mut nodes = Vec::new();
    for id in 1..=n {
        let network = InMemoryRaftNetworkFactory::new(id, router.clone());
        let node = CouncilNode::new(id, fast_config(), network,
            MemLogStore::new(), CouncilStateMachine::new())
            .await.unwrap();
        router.register(id, node.raft().clone()).await;
        nodes.push(node);
    }
    (nodes, router)
}
```

Each test creates a cluster, initialises it, waits for a leader, and then does something interesting.

The failover test shuts down the leader and checks that a new one gets elected:

```rust
// Shut down the leader.
nodes[(leader_id - 1) as usize].shutdown().await.unwrap();

// Surviving nodes elect a new leader.
let new_leader = wait_for_leader_refs(&remaining, Duration::from_secs(5),
    Some(leader_id)).await;
assert!(new_leader.is_some());
assert_ne!(new_leader.unwrap(), leader_id);
```

The `Some(leader_id)` argument to `wait_for_leader_refs` tells the helper to ignore the old leader's ID. Without this, surviving nodes might briefly report the old leader before they notice it's gone and trigger an election. A subtle race condition that would make the test flaky.

The partition test is the most complex. It splits a 5-node cluster into a majority (3 nodes) and a minority (2 nodes), verifies the majority can still write and the minority can't, then heals the partition and verifies convergence:

```rust
// Partition: isolate nodes 4 and 5 from 1, 2, 3.
for &m in &[4, 5] {
    for &j in &[1, 2, 3] {
        router.partition(m, j).await;
    }
}

// Majority writes succeed.
ml.write(RaftRequest::ConfigSet {
    key: "after".to_string(),
    value: "partition".to_string(),
}).await.unwrap();

// Minority doesn't see the write.
let minority_state = nodes[3].desired_state().await;
assert!(!minority_state.config.contains_key("after"));

// Heal and wait for convergence.
router.heal().await;
tokio::time::sleep(Duration::from_millis(2000)).await;

// Now everyone has everything.
for node in &nodes {
    let state = node.desired_state().await;
    assert_eq!(state.config.get("after").map(String::as_str),
        Some("partition"));
}
```

This is the fundamental Raft guarantee: as long as a majority is connected, the cluster makes progress. The minority falls behind but catches up automatically when connectivity is restored. No manual intervention, no data loss.

The test configuration uses aggressive timers (50ms heartbeat, 200-400ms election timeout) so elections happen quickly. In production, you'd use longer intervals to avoid unnecessary elections during brief network hiccups.

### What we built

The council module is about 700 lines of Rust across 6 files, plus 38 tests. Here's what each file does:

| File | Lines | Purpose |
|------|------:|---------|
| `types.rs` | 225 | Type config, request/response envelopes, desired state model |
| `log_store.rs` | 200 | In-memory log and vote storage |
| `state_machine.rs` | 215 | Applies entries, builds and installs snapshots |
| `network.rs` | 145 | In-memory RPC routing with partition simulation |
| `node.rs` | 165 | High-level API wrapping openraft |
| `mod.rs` | 38 | Module root, error enum, re-exports |

Most of the complexity lives in the tests, not the implementation. The adapter code is straightforward — openraft does the hard work. What we get for our 700 lines is a replicated state machine with automatic leader election, log compaction, and partition tolerance. Try building that from scratch in a weekend.

### What's next

We have gossip for membership and Raft for consensus. The next pieces connect them: council selection decides which gossip members become Raft voters, the reporting tree gets runtime state from workers to the council, and the Patty scheduler turns desired state into placement decisions. Gossip is the nervous system, Raft is the brain. Now we need the decision-making.

## Choosing the council

The Raft council can grow from 1 to 7 members, but who gets promoted? In a small cluster you might pick nodes manually. In a 5,000-node deployment, that doesn't scale. We need an algorithm that examines the membership table and produces a ranked list of candidates.

Council selection is a pure function. It takes a snapshot of the membership table, the current council roster, a target size, and some configuration. It returns a list of node IDs, best candidates first. No Raft calls, no gossip mutations, no async. The caller — the agent integration layer we'll build later — reads the output and drives `add_learner()` and `change_membership()` on the Raft node.

### Four criteria

The algorithm evaluates candidates on four things, in priority order.

**Stability.** A node must have been alive for at least `min_node_age` (default: 10 minutes) before it's eligible. A node that joined 30 seconds ago might be flapping — restarting repeatedly, hitting a boot loop, or just barely connecting. We don't want it making quorum decisions. The age check uses `now.duration_since(node.first_seen)`, where `first_seen` is set when the node first appears in the membership table via gossip.

**Resource availability.** A council member runs the Raft engine, stores the log, applies entries to the state machine. That's not free. If a node is already at 95% CPU, adding council duties might push it over the edge. We filter out nodes where CPU usage exceeds 90% of capacity or memory exceeds 85%. These thresholds come from `ResourceSummary`, the compact resource snapshot that nodes piggyback on gossip messages.

What about nodes that haven't reported resources yet? They're excluded. No data means no guarantee they're not overloaded. A node has to have reported at least once to be eligible.

**Zone diversity.** If all your council members are in the same rack and that rack loses power, your entire consensus layer dies. The algorithm collects the zones already represented in the current council (from the `"zone"` label in each node's labels map), then ranks candidates from *unrepresented* zones higher. A candidate in zone-c when the council already has members in zone-a and zone-b is more valuable than another zone-a candidate.

Candidates without a zone label don't get the diversity bonus. No label means no diversity information, so they rank the same as candidates in an already-represented zone.

**Deterministic tiebreak.** After filtering and scoring, multiple candidates might look identical — same zone novelty, same approximate age. The sort uses node ID (lexicographic) as the final tiebreaker. This is fully deterministic: same inputs, same output, every time. No randomness, no hash seeds. If you're debugging why node "beta" was chosen over node "gamma", you can reproduce the decision exactly by replaying the same membership table.

### The sort

The implementation is a single `sort_by` call with chained comparators:

```rust
candidates.sort_by(|a, b| {
    let a_novel = a.labels.get(&config.zone_label_key)
        .is_some_and(|z| !council_zones.contains(z.as_str()));
    let b_novel = b.labels.get(&config.zone_label_key)
        .is_some_and(|z| !council_zones.contains(z.as_str()));

    b_novel.cmp(&a_novel)                           // novel zones first
        .then_with(|| a.first_seen.cmp(&b.first_seen))  // oldest first
        .then_with(|| a.node_id.cmp(&b.node_id))        // lexicographic
});
```

`then_with` is Rust's way of chaining comparison keys. If the first comparison is `Equal`, it evaluates the next one. If that's also `Equal`, the next. The chain terminates at `node_id`, which is unique, so no two candidates ever compare equal. This gives us a total order — no ambiguity, no instability in the sort.

`is_some_and` is a method on `Option<T>` that returns `false` for `None` and applies the predicate for `Some`. It replaced the `map_or(false, |x| ...)` pattern in Rust 1.70. Cleaner to read, same semantics.

Comparing `bool` values works because `false < true` in Rust. So `b_novel.cmp(&a_novel)` puts `true` (novel) before `false` (not novel).

### Making time testable

The age check needs the current time. We could call `Instant::now()` inside the function, but then we can't write deterministic tests. Instead, we pass `now: Instant` as a parameter:

```rust
pub fn select_council_candidates(
    membership: &MembershipTable,
    current_council: &[NodeId],
    target_size: usize,
    config: &CouncilSelectionConfig,
    now: Instant,
) -> Vec<NodeId>
```

In production, the caller passes `Instant::now()`. In tests, we control both `now` and `first_seen` to create precise age differences:

```rust
let now = Instant::now();
let old = Duration::from_secs(700);
// This node's first_seen is 700 seconds before `now`.
add_node(&mut table, "old", 1, now, old, None, Some(healthy_resources()));
```

The test helper calls `table.apply_update(&update, now - age)`, which sets `first_seen` to a point in the past. When the algorithm computes `now.duration_since(first_seen)`, it gets exactly 700 seconds. No sleeps, no mocking, no global time override.

This is a general Rust pattern worth internalising: when a function depends on something external (time, randomness, I/O), pass it as a parameter instead of reaching for it internally. You get testability for free and the function signature documents its dependencies.

### Testing the algorithm

The test suite exercises each filter individually. Here's the zone diversity test:

```rust
#[test]
fn prefers_novel_zones() {
    let now = Instant::now();
    let mut table = MembershipTable::new();
    let old = Duration::from_secs(700);

    // Council member already in zone-a.
    let council_id = add_node(&mut table, "council-1", 1, now, old,
        Some("zone-a"), Some(healthy_resources()));

    // Two candidates: zone-a (same) and zone-b (novel).
    let _same_zone = add_node(&mut table, "candidate-a", 2, now, old,
        Some("zone-a"), Some(healthy_resources()));
    let novel_zone = add_node(&mut table, "candidate-b", 3, now, old,
        Some("zone-b"), Some(healthy_resources()));

    let result = select_council_candidates(
        &table, &[council_id], 3, &default_config(), now);
    assert_eq!(result[0], novel_zone);
}
```

The council already has a member in zone-a. Two candidates apply — one also in zone-a, one in zone-b. The algorithm should pick zone-b first because it adds diversity. Both candidates are old enough, have healthy resources, and aren't on the council. The only differentiator is zone novelty.

The bounds tests verify clamping: asking for target size 1 gets clamped to the minimum of 3, asking for 20 gets clamped to the maximum of 7. And the empty-result tests verify that the algorithm gracefully returns nothing when the council is already full or no candidates pass the filters.

### What's next

Council selection produces a ranked list of candidates. The agent integration step will wire this into the actual Raft membership changes — calling `add_learner()` for each selected candidate, waiting for them to catch up, then promoting them with `change_membership()`. Before that, we need the reporting tree: how worker nodes get their runtime state to the council, and how the council aggregates it for the leader.

# Mustard: Gossip Protocol & Raft Consensus Layer

Design document for the Mustard gossip protocol and the Raft consensus layer in Reliaburger. Covers cluster membership, failure detection, leader election, state reconstruction, and catastrophic recovery.

**Status:** Draft
**Whitepaper reference:** Section 8 (Leader Election & State Management)
**Last updated:** 2026-02-16

---

## 1. Overview

Reliaburger coordinates a cluster of up to 10,000 homogeneous nodes without a dedicated control plane. Two protocols operate at different scales to make this work:

- **Mustard** is the gossip protocol. It is based on SWIM (Scalable Weakly-consistent Infection-style Membership) and runs on every node. Mustard handles cluster membership discovery, failure detection, leader identity broadcast, node resource summaries, and catastrophic recovery candidate distribution. It carries only lightweight, fixed-size payloads and achieves O(log N) convergence with O(1) per-node network overhead.

- **Raft** runs on a small dynamic council of 3 to 7 nodes selected from the cluster. One council member is the leader; the others are hot standbys. Raft handles leader election among council members, replication of desired state (app specs, secrets, scheduling decisions), and cluster configuration changes. Raft is explicitly not used for runtime state.

- **The hierarchical reporting tree** is the third communication layer. Each worker node reports its runtime state (running containers, health, resource usage, job completions) to its assigned council member over mTLS. Council members aggregate reports for the leader. This keeps variable-size payloads off the gossip mesh, preserving SWIM's scaling properties.

**Key design decisions:**

1. Gossip messages are fixed-size. Variable-size runtime state flows through the reporting tree, not through Mustard. This is non-negotiable for scaling to 10,000 nodes.
2. Raft membership is dynamic. Any node can join or leave the council. There are no statically assigned "server" nodes.
3. The cluster can survive total council loss via a pre-seeded deterministic recovery mechanism. No vote or election is needed -- the priority order was established before the failure.
4. The data plane is never interrupted by a control plane event. Apps keep running during leader elections, state reconstruction, and network partitions.

---

## 2. Dependencies

| Dependency | Component | Why |
|------------|-----------|-----|
| **Sesame** (Security/mTLS) | All inter-node communication | Mustard gossip messages are authenticated via HMAC derived from node certificates. Raft log entries are encrypted at rest. The reporting tree uses mTLS channels signed by the Node CA. Recovery candidate notifications are sent over mTLS. |
| **Bun** (Agent) | Every node | Bun is the host process. Mustard, Raft, and the reporting tree all run as async tasks within the Bun process. Bun owns the node lifecycle (join, leave, shutdown) that Mustard reacts to. |
| **Meat** (Scheduler) | Leader only | The leader runs Meat. Council selection criteria (resource availability, stability) are evaluated using data that Meat also consumes. The leader's scheduling decisions are replicated via Raft. |
| **Lettuce** (GitOps) | Council coordinator | When GitOps is enabled, the Lettuce engine provides the authoritative desired state. During state reconstruction, the new leader loads desired state from the git repository via Lettuce. |
| **Onion** (Service Discovery) | All nodes | Onion's eBPF-based service map depends on knowing which apps are scheduled where, information that flows through the Raft log and is disseminated by council members. |
| **Grill** (Container Runtime) | All nodes | Grill reports running container state to Bun, which feeds into StateReports sent via the reporting tree during reconstruction. |

---

## 3. Architecture

### 3.1 Two-Layer Protocol Stack

```
┌──────────────────────────────────────────────────────────────────────┐
│                         RELIABURGER CLUSTER                         │
│                                                                      │
│  Layer 3: Hierarchical Reporting Tree (variable-size runtime state)  │
│  ┌──────────────────────────────────────────────────────────────┐    │
│  │  Worker → Council Member → Leader (mTLS, per-node reports)   │    │
│  │  Interval: 5s (configurable)                                 │    │
│  │  Payload: running apps, health, resources, events            │    │
│  └──────────────────────────────────────────────────────────────┘    │
│                                                                      │
│  Layer 2: Raft Consensus (desired state, leader election)           │
│  ┌──────────────────────────────────────────────────────────────┐    │
│  │  Council members only (3-7 nodes)                            │    │
│  │  Heartbeat: 150ms    Election timeout: 1000-2000ms           │    │
│  │  Payload: app specs, secrets, scheduling decisions, config   │    │
│  └──────────────────────────────────────────────────────────────┘    │
│                                                                      │
│  Layer 1: Mustard Gossip (membership, failure detection, metadata)  │
│  ┌──────────────────────────────────────────────────────────────┐    │
│  │  All nodes (up to 10,000)                                    │    │
│  │  Protocol interval: 500ms    Probe timeout: 200ms            │    │
│  │  Payload: membership, leader ID, resource summaries,         │    │
│  │           recovery candidate blob (encrypted)                │    │
│  │  Message size: fixed, ~512 bytes per gossip round            │    │
│  └──────────────────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────────────────┘
```

### 3.2 Mustard Gossip Protocol

Mustard implements the SWIM protocol with suspicion and protocol period optimisations.

**Probe cycle (per protocol period):**

```
Node A                    Node B                    Node C
  │                         │                         │
  │──── PING ──────────────>│                         │
  │                         │                         │
  │<─── ACK ───────────────│                         │
  │   (with piggybacked     │                         │
  │    membership updates)  │                         │
  │                         │                         │
  │  [If B does not ACK within probe_timeout:]        │
  │                         │                         │
  │──── PING-REQ ──────────────────────────────────>│
  │   (ask C to probe B)    │                         │
  │                         │<──── PING ─────────────│
  │                         │──── ACK ──────────────>│
  │<──── ACK (indirect) ────────────────────────────│
  │                         │                         │
  │  [If neither direct nor indirect ACK:]            │
  │                         │                         │
  │  Mark B as SUSPECT      │                         │
  │  Disseminate SUSPECT    │                         │
  │  via piggybacked gossip │                         │
  │                         │                         │
  │  [After suspicion_timeout with no refutation:]    │
  │                         │                         │
  │  Mark B as DEAD         │                         │
  │  Disseminate DEAD       │                         │
```

Each PING/ACK message piggybacks a bounded number of membership updates (new joins, suspects, deaths, leader changes). This piggybacking is how information propagates in O(log N) rounds without dedicated broadcast messages.

**Gossip message categories carried by Mustard:**

1. **Membership events** -- node join, node suspect, node dead, node alive (refutation)
2. **Leader identity** -- which node is the current Raft leader (broadcast by leader, propagated by all)
3. **Resource summaries** -- per-node CPU/memory/GPU capacity and utilisation (~128 bytes per node)
4. **Recovery candidate blob** -- encrypted list of pre-seeded recovery candidates (~256 bytes, updated infrequently)
5. **Council membership** -- which nodes are currently in the Raft council

### 3.3 Raft Integration

The Raft consensus group operates independently from Mustard but depends on it for two things:

1. **Leader discovery.** When a new Raft leader is elected, the leader announces itself via Mustard gossip so all 10,000 nodes learn the leader identity without Raft having to communicate with them.
2. **Council member replacement.** When a council member departs, the leader uses Mustard's membership list to find a suitable replacement.

Raft is used for:

- Leader election among council members
- Log replication of desired state (app specs, secrets, scheduling decisions)
- Cluster configuration changes (council size, gossip parameters, etc.)
- Membership changes to the council itself (AddNode / RemoveNode log entries)

Raft is **not** used for:

- Runtime state (what is running where)
- Failure detection (that is Mustard's job)
- Communication with non-council nodes

### 3.4 Council Selection Algorithm

The leader selects council members from the general node pool. Selection criteria, in priority order:

1. **Stability.** Node must have been in the cluster for at least `council.min_node_age` (default 10 minutes). This prevents a freshly joined node from immediately becoming a council member before it has proven reliable.
2. **Resource availability.** Node must not be critically overloaded (CPU < 90%, memory < 85%). Council duties add modest overhead; an overloaded node would be a poor council member.
3. **Zone diversity.** If node labels include zone or region information, the leader maximizes geographic distribution among council members. A council where all members are in the same rack defeats the purpose of redundancy.
4. **Random tiebreaker.** Among otherwise-equal candidates, selection is random (seeded by the leader's node ID + current term for reproducibility).

Council replacement flow:

```
Council member C departs (detected via Raft heartbeat timeout)
     │
     ▼
Remaining council members detect departure
     │
     ├── Was C the leader?
     │     YES: Raft election among remaining members (<5s)
     │           New leader emerges
     │     NO:  Current leader continues
     │
     ▼
Leader evaluates candidate pool via Mustard membership
     │
     ▼
Leader selects replacement node R
     │
     ▼
Leader issues Raft AddNode(R) log entry
     │
     ▼
R receives Raft log snapshot + incremental entries
     │
     ▼
R catches up to current log position
     │
     ▼
Council returns to target size
```

### 3.5 Hierarchical Reporting Tree

The reporting tree assigns each non-council node to a specific council member as its parent. The assignment is deterministic: `parent = council_members[hash(node_id) % council_size]`. This distributes the reporting load evenly.

```
                    ┌────────────┐
                    │   Leader   │
                    └─────┬──────┘
              ┌───────────┼───────────┐
              ▼           ▼           ▼
        ┌──────────┐ ┌──────────┐ ┌──────────┐
        │Council-A │ │Council-B │ │Council-C │
        └────┬─────┘ └────┬─────┘ └────┬─────┘
         ┌───┼───┐    ┌───┼───┐    ┌───┼───┐
         ▼   ▼   ▼    ▼   ▼   ▼    ▼   ▼   ▼
        W1  W2  W3   W4  W5  W6   W7  W8  W9
        ...          ...          ...
```

With a council of 5 and 10,000 total nodes, each council member aggregates reports from ~2,000 worker nodes. Workers send full StateReports at the reporting interval (default 5 seconds). Council members aggregate and forward summaries to the leader.

When a council member departs, its assigned workers detect the lost connection and re-hash to the remaining council members. The new parent receives a full StateReport from each reassigned worker on the next reporting cycle.

---

## 4. Data Structures

### 4.1 Gossip Message Types

```rust
/// Top-level gossip message, always fixed-size.
/// Sent as a single UDP datagram (max 1400 bytes to avoid fragmentation).
#[derive(Serialize, Deserialize)]
struct GossipMessage {
    /// Protocol version for forward compatibility.
    version: u8,
    /// Sender's node ID.
    sender: NodeId,
    /// Monotonic incarnation number (incremented on refutation).
    incarnation: u64,
    /// HMAC-SHA256 over the serialised payload, keyed by the shared gossip key
    /// derived from the node's mTLS certificate.
    hmac: [u8; 32],
    /// The message payload.
    payload: GossipPayload,
}

#[derive(Serialize, Deserialize)]
enum GossipPayload {
    Ping {
        /// Target node being probed.
        target: NodeId,
        /// Piggybacked membership updates (bounded to MAX_PIGGYBACK_UPDATES).
        updates: ArrayVec<MembershipUpdate, MAX_PIGGYBACK_UPDATES>,
    },
    PingReq {
        /// Node that the sender wants the receiver to probe on its behalf.
        target: NodeId,
        /// Original requester, so the indirect probe result can be routed back.
        requester: NodeId,
        updates: ArrayVec<MembershipUpdate, MAX_PIGGYBACK_UPDATES>,
    },
    Ack {
        /// Responding to a PING or PING-REQ for this target.
        target: NodeId,
        updates: ArrayVec<MembershipUpdate, MAX_PIGGYBACK_UPDATES>,
    },
}

/// Maximum number of piggybacked membership updates per gossip message.
/// Bounded to keep message size constant.
const MAX_PIGGYBACK_UPDATES: usize = 8;

/// A single membership update piggybacked on gossip messages.
#[derive(Serialize, Deserialize, Clone)]
struct MembershipUpdate {
    /// Which node this update concerns.
    node: NodeId,
    /// The new state of the node.
    state: NodeState,
    /// Incarnation number of the node (for crdt-like conflict resolution).
    incarnation: u64,
    /// Lamport timestamp for ordering.
    lamport: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
enum NodeState {
    Alive,
    Suspect,
    Dead,
    Left, // graceful departure
}
```

### 4.2 Node Membership

```rust
/// Full membership record for a node, maintained locally by every Mustard participant.
struct NodeMembership {
    node_id: NodeId,
    address: SocketAddr,
    state: NodeState,
    incarnation: u64,
    /// When the node was first seen alive.
    first_seen: Instant,
    /// Last time we received a direct or indirect ACK from this node.
    last_ack: Instant,
    /// Resource summary, updated via gossip piggyback.
    resources: Option<ResourceSummary>,
    /// Node labels (zone, region, etc.), set at join time.
    labels: BTreeMap<String, String>,
    /// Whether this node is a council member.
    is_council: bool,
    /// Whether this node is the current leader.
    is_leader: bool,
}

/// Fixed-size resource summary piggybacked on gossip.
/// Approximately 64 bytes serialised.
#[derive(Serialize, Deserialize, Clone)]
struct ResourceSummary {
    cpu_capacity_millicores: u32,
    cpu_used_millicores: u32,
    memory_capacity_mb: u32,
    memory_used_mb: u32,
    gpu_count: u8,
    gpu_used: u8,
    running_app_count: u16,
    running_job_count: u16,
}
```

### 4.3 Raft Entries

```rust
/// A single entry in the Raft log.
#[derive(Serialize, Deserialize)]
struct RaftEntry {
    term: u64,
    index: u64,
    payload: RaftPayload,
}

#[derive(Serialize, Deserialize)]
enum RaftPayload {
    /// An app spec was created or updated.
    AppSpec {
        app_name: String,
        spec: AppSpec,
        /// SHA-256 of the serialised spec for cheap equality checks.
        spec_hash: [u8; 32],
    },
    /// An app was deleted.
    AppDelete {
        app_name: String,
    },
    /// A scheduling decision was made by Meat.
    SchedulingDecision {
        app_name: String,
        placements: Vec<Placement>,
    },
    /// A secret was created or updated.
    SecretSet {
        name: String,
        /// Encrypted with the council's shared key (age encryption).
        encrypted_value: Vec<u8>,
    },
    /// A secret was deleted.
    SecretDelete {
        name: String,
    },
    /// Cluster configuration change.
    ConfigChange {
        key: String,
        value: toml::Value,
    },
    /// Raft membership change.
    CouncilChange(CouncilChange),
    /// No-op entry used for leader commit confirmation.
    Noop,
}

#[derive(Serialize, Deserialize)]
enum CouncilChange {
    AddMember { node_id: NodeId, address: SocketAddr },
    RemoveMember { node_id: NodeId },
}

#[derive(Serialize, Deserialize, Clone)]
struct Placement {
    node_id: NodeId,
    instance_id: u32,
}
```

### 4.4 State Reports (Reporting Tree)

```rust
/// Sent by each worker node to its assigned council member at the reporting interval.
/// Also sent as a full report during the leader learning period.
#[derive(Serialize, Deserialize)]
struct StateReport {
    node_id: NodeId,
    /// Timestamp of this report (wall clock, for leader to detect stale reports).
    timestamp: SystemTime,
    /// All apps currently running on this node.
    running_apps: Vec<RunningApp>,
    /// Cached desired-state specs this node was last assigned.
    cached_specs: Vec<CachedSpec>,
    /// Current resource usage.
    resource_usage: ResourceUsage,
    /// Recent event log (bounded to last N events).
    event_log: Vec<NodeEvent>,
}

#[derive(Serialize, Deserialize)]
struct RunningApp {
    app_name: String,
    instance_id: u32,
    image: String,
    port: u16,
    health_status: HealthStatus,
    uptime: Duration,
    resource_usage: AppResourceUsage,
}

#[derive(Serialize, Deserialize)]
enum HealthStatus {
    Healthy,
    Unhealthy { consecutive_failures: u32 },
    Starting,
    Unknown,
}

#[derive(Serialize, Deserialize)]
struct CachedSpec {
    app_name: String,
    spec_hash: [u8; 32],
    spec: AppSpec,
}

#[derive(Serialize, Deserialize)]
struct ResourceUsage {
    cpu_used_millicores: u32,
    memory_used_mb: u32,
    disk_used_mb: u64,
    gpu_used: u8,
    allocated_ports: Vec<u16>,
}

#[derive(Serialize, Deserialize)]
struct AppResourceUsage {
    cpu_millicores: u32,
    memory_mb: u32,
}

#[derive(Serialize, Deserialize)]
struct NodeEvent {
    timestamp: SystemTime,
    kind: EventKind,
    detail: String,
}

#[derive(Serialize, Deserialize)]
enum EventKind {
    ContainerStart,
    ContainerStop,
    ContainerCrash,
    HealthCheckFail,
    HealthCheckRecover,
    ImagePull,
    SpecUpdate,
    Restart,
}
```

### 4.5 Council Member and Recovery Candidate

```rust
/// Council member record, maintained by the leader and replicated via Raft.
#[derive(Serialize, Deserialize, Clone)]
struct CouncilMember {
    node_id: NodeId,
    address: SocketAddr,
    /// When this node joined the council (for stability tracking).
    joined_council_at: SystemTime,
    /// Raft role: Leader, Follower, or Learner (catching up).
    role: RaftRole,
    /// Last known Raft log index this member has applied.
    match_index: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
enum RaftRole {
    Leader,
    Follower,
    Learner,
}

/// A pre-seeded recovery candidate for catastrophic council loss.
#[derive(Serialize, Deserialize, Clone)]
struct RecoveryCandidate {
    /// Priority rank (0 = highest priority).
    priority: u8,
    node_id: NodeId,
    address: SocketAddr,
    /// How long this node has been in the cluster (stability signal).
    uptime: Duration,
    /// Zone/region label for diversity.
    zone: Option<String>,
}

/// The encrypted recovery candidate list distributed via gossip.
/// All nodes carry this blob; only candidates can decrypt it.
#[derive(Serialize, Deserialize, Clone)]
struct EncryptedRecoveryCandidateList {
    /// Ciphertext: the serialised Vec<RecoveryCandidate>, encrypted
    /// with the council's shared age public key.
    ciphertext: Vec<u8>,
    /// Version counter, incremented when the leader updates the list.
    version: u64,
    /// SHA-256 of the plaintext, so candidates who can decrypt
    /// can verify integrity.
    plaintext_hash: [u8; 32],
}
```

### 4.6 Wire Protocol Summary

| Layer | Transport | Encoding | Max Message Size |
|-------|-----------|----------|------------------|
| Mustard gossip (PING/ACK/PING-REQ) | UDP | bincode (serde) | 1400 bytes (avoids IP fragmentation) |
| Mustard protocol metadata (joins, full state sync) | TCP | bincode | 64 KiB |
| Raft (AppendEntries, RequestVote) | TCP + mTLS | bincode | Unbounded (log entries can contain app specs) |
| Reporting tree (StateReports) | TCP + mTLS | bincode | 1 MiB (bounded by max events per report) |

All TCP connections use mTLS via Sesame's Node CA certificates.

---

## 5. Operations

### 5.1 Gossip Protocol Lifecycle

**Node join:**

```
1. New node N starts Bun with `relish join --token <token> <seed-addr>`
2. N completes mTLS handshake with the seed node (receives Node CA cert)
3. N sends a Mustard JOIN message to the seed node (TCP)
4. Seed node responds with full membership list (TCP)
5. N initializes its local membership table
6. N begins the SWIM protocol period loop:
   - Each period: pick a random node, PING it
   - Piggyback own ALIVE state on all messages
   - Within O(log N) periods, all nodes learn about N
7. N begins sending StateReports to its assigned council member
8. Leader adds N to the scheduling pool once N's first StateReport arrives
```

**Node departure (graceful):**

```
1. Bun receives SIGTERM or `relish drain` command
2. Bun marks itself as LEAVING in gossip (piggybacked on next PING/ACK)
3. Bun stops accepting new work
4. Bun waits for in-flight requests to drain (configurable timeout)
5. Bun sends LEFT state via gossip
6. Other nodes mark N as LEFT and remove from scheduling pool
7. Leader reschedules N's apps to other nodes
```

**Node failure detection (SWIM protocol):**

```
                     ┌──────────┐
        join/alive   │          │  probe succeeds
       ┌────────────>│  ALIVE   │<────────────┐
       │             │          │              │
       │             └────┬─────┘              │
       │                  │                    │
       │         probe fails                   │
       │         (direct + indirect)           │
       │                  │                    │
       │                  ▼                    │
       │             ┌──────────┐              │
       │             │          │  node refutes │
       │             │ SUSPECT  │──────────────┘
       │             │          │  (higher incarnation)
       │             └────┬─────┘
       │                  │
       │         suspicion_timeout expires
       │         without refutation
       │                  │
       │                  ▼
       │             ┌──────────┐
       │             │          │
       │             │   DEAD   │
       │             │          │
       │             └────┬─────┘
       │                  │
       │         cleanup_timeout expires
       │                  │
       │                  ▼
       │             ┌──────────┐
       │             │ REMOVED  │  (purged from membership table)
       │             └──────────┘
       │                  │
       │         if node re-appears:
       └──────────────────┘
```

**Suspicion subprotocol.** When a node is marked SUSPECT, the suspecting node begins a suspicion timer. During this period, any node can forward evidence that the suspect is alive (an ACK received from the suspect). If the suspect itself learns it is suspected (via piggybacked updates), it increments its incarnation number and broadcasts an ALIVE message, which overrides the SUSPECT state. Only if the suspicion timer expires without refutation does the node transition to DEAD. This prevents false positives from transient network blips.

### 5.2 Leader Election

Leader election uses standard Raft semantics, constrained to the council.

**State machine:**

```
                    ┌───────────────────────────────────────────────┐
                    │                                               │
           timeout  │                        receives higher term   │
           fires    │                              │                │
                    │                              │                │
              ┌─────┴──────┐  wins election  ┌────┴───────┐        │
    ┌────────>│            │────────────────>│            │        │
    │         │ CANDIDATE  │                 │   LEADER   │        │
    │         │            │<────────────────│            │        │
    │         └─────┬──────┘  loses election └────────────┘        │
    │               │         or discovers                         │
    │               │         higher term                          │
    │   election    │                                              │
    │   timeout     │  discovers current                           │
    │   fires       │  leader or higher term                       │
    │               │                                              │
    │         ┌─────┴──────┐                                       │
    │         │            │                                       │
    └─────────│  FOLLOWER  │<──────────────────────────────────────┘
              │            │
              └────────────┘
                    ▲
                    │
              initial state
              (node joins council)
```

**Election timing:**

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| Heartbeat interval | 150ms | Fast enough to detect leader failure quickly; slow enough to avoid spurious elections on a busy network. |
| Election timeout (min) | 1000ms | Must be >> heartbeat interval. Randomized to prevent split votes. |
| Election timeout (max) | 2000ms | Upper bound of the randomized election timeout range. |
| Pre-vote | Enabled | Prevents disruptive elections from partitioned nodes that have not received recent heartbeats. Nodes must get a majority of pre-vote grants before incrementing their term. |

**Election sequence:**

```
1. Follower F detects that it has not received a heartbeat from the leader
   within its randomized election timeout.

2. F increments its term and transitions to CANDIDATE.

3. F sends RequestVote RPCs to all other council members.
   - Each RequestVote includes F's last log index and term.
   - A voter grants its vote only if F's log is at least as up-to-date
     as the voter's log (Raft's election safety guarantee).

4. If F receives votes from a majority of council members (including itself):
   - F transitions to LEADER.
   - F immediately sends heartbeats to assert authority.
   - F announces its leadership via Mustard gossip.
   - F enters the LEARNING PERIOD (see Section 5.4).

5. If F does not receive a majority before a new election timeout:
   - F increments term again and retries (or steps down if it discovers
     a higher term from another node).

6. If F receives a heartbeat from a leader with a term >= F's current term:
   - F steps down to FOLLOWER.
```

With a council of 5, a majority is 3 votes. Even with one member down, 4 remaining members can elect a leader (needing 3 of 4). With two members down, 3 remaining can still elect (needing 2 of 3). This is why the minimum council size is 3 -- it tolerates exactly 1 failure.

**Leader election performance target: < 5 seconds.** This includes the election timeout (up to 2 seconds), one or two rounds of voting if the first round has a split vote (up to 4 seconds in the worst case), and the leader announcement via gossip (~1 second to propagate). In practice, elections complete in 1-3 seconds.

### 5.3 Council Selection Algorithm

```
fn select_council_candidates(
    membership: &MembershipTable,
    current_council: &[NodeId],
    target_size: usize,
    config: &CouncilConfig,
) -> Vec<NodeId> {
    let needed = target_size - current_council.len();
    if needed == 0 {
        return vec![];
    }

    let mut candidates: Vec<&NodeMembership> = membership
        .iter()
        .filter(|n| {
            // Must be alive
            n.state == NodeState::Alive
            // Must not already be on the council
            && !current_council.contains(&n.node_id)
            // Must have been in the cluster long enough
            && n.first_seen.elapsed() >= config.min_node_age
            // Must not be overloaded
            && n.resources.as_ref().map_or(false, |r| {
                r.cpu_used_millicores < (r.cpu_capacity_millicores * 9 / 10)
                && r.memory_used_mb < (r.memory_capacity_mb * 85 / 100)
            })
        })
        .collect();

    // Sort by zone diversity: prefer zones not already represented in council
    let council_zones: HashSet<&str> = current_council.iter()
        .filter_map(|id| membership.get(id))
        .filter_map(|n| n.labels.get("zone").map(|s| s.as_str()))
        .collect();

    candidates.sort_by(|a, b| {
        let a_diverse = a.labels.get("zone")
            .map_or(false, |z| !council_zones.contains(z.as_str()));
        let b_diverse = b.labels.get("zone")
            .map_or(false, |z| !council_zones.contains(z.as_str()));
        b_diverse.cmp(&a_diverse) // diverse zones first
            .then_with(|| a.first_seen.cmp(&b.first_seen)) // older nodes first
    });

    // Deterministic random tiebreak among top candidates
    // (seeded by leader node_id + current term)
    let top = candidates.iter().take(needed * 3).collect::<Vec<_>>();
    let mut rng = StdRng::seed_from_u64(hash(leader_id, current_term));
    let selected = top.choose_multiple(&mut rng, needed);

    selected.map(|n| n.node_id).collect()
}
```

### 5.4 State Reconstruction Protocol

When a new leader is elected, it must rebuild its understanding of the cluster's runtime state. The Raft log contains desired state, but runtime state (what is actually running) is distributed across all nodes.

**Learning period state machine:**

```
                    ┌─────────────────────┐
  leader elected    │                     │
 ──────────────────>│  ANNOUNCE_LEADERSHIP│
                    │  (via Mustard)      │
                    └──────────┬──────────┘
                               │
                               ▼
                    ┌─────────────────────┐
                    │                     │  receive StateReport
                    │   LEARNING          │◄────────────────────┐
                    │                     │                     │
                    │  - accept reports   │─────────────────────┘
                    │  - no scheduling    │
                    │  - no new deploys   │
                    │  - track % reported │
                    └──────────┬──────────┘
                               │
                    ┌──────────┴──────────┐
                    │                     │
          ┌────────┤  95% nodes reported  ├────────┐
          │        │  OR timeout (15s)    │        │
          │        │                     │        │
          │        └─────────────────────┘        │
          │                                        │
          ▼                                        ▼
  ┌───────────────┐                     ┌───────────────────┐
  │ LOAD DESIRED  │                     │ LOAD DESIRED      │
  │ STATE         │                     │ STATE             │
  │ (git or Raft) │                     │ (git or Raft)     │
  └───────┬───────┘                     │ + mark unreported │
          │                             │   nodes as        │
          │                             │   STATE_UNKNOWN   │
          │                             └────────┬──────────┘
          │                                      │
          └──────────────┬───────────────────────┘
                         │
                         ▼
                ┌─────────────────┐
                │ RECONCILE       │
                │                 │
                │ diff desired vs │
                │ actual state    │
                │                 │
                │ issue corrections│
                └────────┬────────┘
                         │
                         ▼
                ┌─────────────────┐
                │ ACTIVE          │
                │                 │
                │ accept deploys  │
                │ schedule work   │
                │ (only to nodes  │
                │  that reported) │
                └─────────────────┘
                         │
              unreported nodes ───► scheduled once they report
              check in later       (leader reconciles on arrival)
```

**Key invariants during the learning period:**

1. The data plane is completely unaffected. Running apps continue serving traffic.
2. New deploys are queued but not processed until the learning period ends.
3. No scheduling decisions are made with incomplete information.
4. Nodes marked STATE_UNKNOWN are not scheduled to. They become schedulable when they report in.

**Performance targets:**

| Cluster size | Typical reconstruction time | Worst case (timeout) |
|-------------|---------------------------|---------------------|
| 100 nodes | < 2 seconds | 15 seconds |
| 1,000 nodes | < 5 seconds | 15 seconds |
| 10,000 nodes | < 15 seconds | 30 seconds (configurable) |

### 5.5 Catastrophic Recovery

When the entire Raft council is lost simultaneously (all 3-7 members fail at once), the pre-seeded recovery mechanism activates.

**Normal operation (preparation):**

```
1. Leader maintains a priority-ordered list of RECOVERY_CANDIDATE_COUNT
   (default 10) stable nodes outside the current council.

2. Selection criteria:
   - Long uptime (stability signal)
   - Healthy (not resource-constrained)
   - Zone/region diversity (survive rack failures)
   - Not already a council member

3. Leader notifies each candidate of its priority directly via the
   reporting tree's mTLS channel.

4. Leader encrypts the full candidate list with the council's shared
   age public key and distributes the encrypted blob via Mustard gossip.

5. All nodes carry the encrypted blob. Non-candidates cannot read it.
   Candidates can decrypt it to verify the list's integrity and their
   own priority position.
```

**Recovery activation:**

```
1. All nodes detect via Mustard gossip: no leader heartbeat for
   CATASTROPHIC_TIMEOUT (default 30 seconds).

2. Each recovery candidate checks: am I the highest-priority surviving
   candidate?
   - Candidates know the list because they were notified individually
     and can decrypt the gossip blob.
   - A candidate waits an additional delay proportional to its priority
     rank (priority * 2 seconds) before assuming leadership. This gives
     higher-priority candidates time to act first.

3. The highest-priority surviving candidate assumes TEMPORARY LEADERSHIP.
   No vote or election is needed.

4. Temporary leader:
   a. Announces itself via Mustard gossip as temporary leader.
   b. Enters the standard learning period (Section 5.4).
   c. Collects StateReports from all surviving nodes.
   d. Loads desired state from git (if GitOps) or from cached specs.
   e. Selects new council members from the surviving node pool.
   f. Bootstraps a new Raft group with the selected council.
   g. Transitions from temporary leader to normal Raft leader.

5. Normal operations resume.
```

**If no recovery candidates survive:**

All nodes remain in read-only mode. Apps continue running. No new deploys are accepted. Manual intervention via `relish recover` bootstraps a new council from any surviving node. This is the true catastrophic case and manual intervention is appropriate.

### 5.6 Split-Brain Prevention

Split-brain can occur during a network partition where two groups each believe the council is lost.

**Raft quorum rule (normal operation):** A Raft leader must maintain contact with a majority of council members. If a partition splits a 5-member council into groups of 3 and 2, only the group of 3 can elect a leader (it has a majority). The group of 2 enters read-only mode.

**Recovery candidate rule (catastrophic recovery):** If the entire council is lost and a partition splits the remaining nodes, only the group containing the highest-priority recovery candidate can form a new council. The other group remains in read-only mode until the partition heals.

**Determinism:** There is never ambiguity about which group leads. The priority order was established before the failure. Two groups cannot independently decide they should both lead. This is the primary advantage of the pre-seeded approach over any voting-based catastrophic recovery.

**Read-only mode behaviour:**

- Apps continue running and serving traffic.
- Health checks continue; containers are restarted on failure.
- No new deploys are accepted.
- No scheduling decisions are made.
- No cluster configuration changes are allowed.
- Nodes continue gossip and failure detection within their partition.
- When the partition heals, the non-leader group discovers the real leader via gossip and sends StateReports. The leader reconciles any drift.

### 5.7 Membership Changes (Raft)

Council membership changes use the single-server-at-a-time approach (Raft's joint consensus is complex and error-prone). The leader issues one AddNode or RemoveNode Raft log entry at a time and waits for it to commit before issuing another.

```
Add member flow:
1. Leader selects candidate C via council selection algorithm.
2. Leader appends AddNode(C) to the Raft log.
3. Once committed, leader begins sending AppendEntries to C.
4. C enters LEARNER state, receiving log entries but not voting.
5. Once C's log is within a configurable threshold of the leader's log:
   C transitions to FOLLOWER and becomes a voting member.
6. Leader broadcasts updated council membership via Mustard.

Remove member flow:
1. Leader detects member M is unhealthy (Raft heartbeat timeout).
2. Leader appends RemoveNode(M) to the Raft log.
3. Once committed, M is no longer counted for quorum.
4. Leader selects a replacement (loops back to Add member flow).
5. Leader broadcasts updated council membership via Mustard.
```

---

## 6. Configuration

All configuration lives in the cluster's TOML configuration, modifiable via `relish config set` or the Brioche UI.

```toml
[mustard]
# SWIM protocol period: how often each node probes one random peer.
# Lower = faster detection, higher network overhead.
gossip_interval = "500ms"

# How long to wait for a direct PING ACK before trying indirect probing.
probe_timeout = "200ms"

# Number of indirect probe targets when direct probe fails.
indirect_probe_count = 3

# How long a node stays in SUSPECT before transitioning to DEAD.
suspicion_timeout = "5s"

# How long to keep DEAD nodes in the membership table before purging.
cleanup_timeout = "60s"

# Maximum gossip message size (UDP datagram).
max_message_size = 1400

# Maximum piggybacked membership updates per message.
max_piggyback_updates = 8

# Port for Mustard gossip (UDP + TCP).
port = 7946

[raft]
# Heartbeat interval from leader to followers.
heartbeat_interval = "150ms"

# Minimum election timeout (randomized between min and max).
election_timeout_min = "1000ms"

# Maximum election timeout.
election_timeout_max = "2000ms"

# Enable Raft pre-vote extension to prevent disruptive elections.
pre_vote = true

# Maximum entries per AppendEntries RPC.
max_append_entries_batch = 64

# Snapshot interval (number of log entries before compaction).
snapshot_interval = 10000

# Port for Raft communication (TCP + mTLS).
port = 7947

[council]
# Target number of council members (must be odd for clean majorities).
target_size = 5

# Minimum council size (below this, the cluster enters degraded mode).
min_size = 3

# Maximum council size.
max_size = 7

# Minimum time a node must be alive before it can join the council.
min_node_age = "10m"

# Maximum CPU utilisation for council eligibility (percent).
max_cpu_for_eligibility = 90

# Maximum memory utilisation for council eligibility (percent).
max_memory_for_eligibility = 85

[reporting_tree]
# How often worker nodes send StateReports to their council member.
report_interval = "5s"

# Maximum events per StateReport.
max_events_per_report = 100

# Timeout for considering a worker's report stale.
stale_report_timeout = "30s"

[reconstruction]
# Percentage of nodes that must report before the learning period ends.
report_threshold_percent = 95

# Maximum duration of the learning period.
learning_period_timeout = "15s"

# Extended timeout for clusters > 5000 nodes.
large_cluster_timeout = "30s"

# Threshold above which large_cluster_timeout is used.
large_cluster_node_count = 5000

[recovery]
# Number of pre-seeded recovery candidates.
candidate_count = 10

# Timeout before recovery candidates activate (no leader heartbeat).
catastrophic_timeout = "30s"

# Per-priority-rank delay before a candidate assumes leadership.
# Candidate with priority 0 waits 0s, priority 1 waits this value, etc.
priority_step_delay = "2s"

# How often the leader refreshes the recovery candidate list.
candidate_refresh_interval = "5m"
```

---

## 7. Failure Modes

### 7.1 Leader Failure

**Detection:** Council followers detect missing heartbeats within `election_timeout_max` (2 seconds). All cluster nodes detect missing leader via Mustard gossip within `catastrophic_timeout` (30 seconds), but Raft election completes long before that.

**Recovery:** Raft elects a new leader from the remaining council. The new leader enters the learning period, collects StateReports, and resumes scheduling. Total downtime for new deploys: < 5 seconds for the election + learning period duration.

**Data plane impact:** None. Running apps are unaffected.

### 7.2 Council Member Departure (Non-Leader)

**Detection:** Leader detects missing Raft heartbeat response within a few heartbeat intervals (~500ms).

**Recovery:** Leader issues RemoveNode, selects a replacement from the general pool, issues AddNode. The new member catches up via Raft log replication. Workers assigned to the departed member's subtree in the reporting tree re-hash to remaining council members.

**Data plane impact:** None. Workers transparently reroute their reports.

### 7.3 Full Council Loss

**Detection:** All nodes detect missing leader heartbeat via Mustard gossip after `catastrophic_timeout` (30 seconds).

**Recovery:** Pre-seeded recovery candidate assumes temporary leadership. New council is bootstrapped. See Section 5.5 for the full flow.

**Data plane impact:** None during the 30-second detection window. Apps continue running. New deploys are unavailable for ~30 seconds + learning period.

### 7.4 Network Partition

**Scenario A: Partition splits the council.**
The group with a Raft majority elects a leader and continues. The minority group enters read-only mode. Workers in the minority partition continue running their apps.

**Scenario B: Partition isolates the entire council from all workers.**
The council continues operating but has no workers to schedule to. Workers continue running their apps in read-only mode. When the partition heals, workers send StateReports and the leader reconciles.

**Scenario C: Partition occurs during catastrophic recovery.**
Only the partition containing the highest-priority recovery candidate can form a new council. See Section 5.6.

### 7.5 Split-Brain

Prevented by two mechanisms:

1. Raft quorum: cannot have two leaders in the same term without a majority.
2. Recovery candidate priority: deterministic, pre-established ordering prevents two groups from independently deciding to lead.

If split-brain somehow occurs (bug, Byzantine failure), the leader with the higher Raft term wins. Nodes always follow the highest-term leader they are aware of.

### 7.6 Gossip Message Loss

UDP is unreliable. Mustard handles this through redundancy:

- Each membership update is piggybacked on multiple messages (a dissemination count of `ceil(log(N))` ensures each update is sent enough times for probabilistic reliability).
- Indirect probing (PING-REQ) provides a second path when direct probes fail.
- The suspicion subprotocol prevents premature failure declarations from transient loss.
- Full state sync (TCP) occurs periodically between random pairs of nodes as a consistency backstop.

At 10,000 nodes with 0.1% UDP packet loss, the expected convergence time increases by less than one additional protocol period (~500ms).

---

## 8. Security Considerations

### 8.1 Gossip Authentication

Every Mustard gossip message includes an HMAC-SHA256 tag computed over the serialised payload. The HMAC key is derived from the cluster's shared gossip key, which is distributed during the node join process (encrypted with the node's mTLS certificate). A node that has not completed the `relish join` handshake cannot forge or inject gossip messages.

The gossip key is rotated when the leader rotates the cluster's CA certificates (via `relish ca rotate`). During rotation, nodes accept messages signed with either the old or new key (dual-key window) until all nodes have received the new key.

### 8.2 Encrypted Recovery Candidate List

The recovery candidate list is encrypted with the council's shared age public key. This prevents an attacker who compromises a single worker node from learning which nodes are recovery candidates (and therefore which nodes to target for maximum disruption).

Each recovery candidate is notified of its priority individually via the reporting tree's mTLS channel. The candidate can verify that the encrypted gossip blob matches by decrypting it and checking the plaintext hash.

The council's shared age keypair is generated during `relish init` and stored encrypted in the Raft log (encrypted with the Node CA's key). It is available only to council members.

### 8.3 Raft Log Encryption

Raft log entries are encrypted at rest on disk using age encryption with a key derived from the Node CA certificate. This protects secrets stored in the Raft log (encrypted environment variables, API tokens, etc.) from disk access by an attacker who gains physical or root access to a council node without valid cluster credentials.

In transit, Raft messages are protected by mTLS (all inter-council communication uses TLS 1.3 with mutual certificate authentication via Sesame).

### 8.4 Reporting Tree Authentication

All reporting tree connections use mTLS via Sesame's Node CA. A council member verifies that the reporting worker's node ID (from the certificate common name) matches the claimed `node_id` in the StateReport. This prevents a compromised worker from reporting state on behalf of another node.

---

## 9. Performance

### 9.1 Gossip Scalability

| Metric | Value | Explanation |
|--------|-------|-------------|
| Convergence time | O(log N) protocol periods | Each protocol period, each node probes one random peer and piggybacks updates. Information spreads like an epidemic. |
| Per-node network overhead | O(1) | Each node sends/receives a constant number of messages per protocol period regardless of cluster size. |
| Bandwidth per node | ~3 KB/s at 10,000 nodes | 1 PING + 1 ACK per protocol period (500ms), ~1400 bytes each. Plus occasional PING-REQ. |
| Membership table memory | ~256 bytes per node | NodeMembership struct. At 10,000 nodes: ~2.5 MB total per node. |

### 9.2 Leader Election Performance

| Metric | Target | Mechanism |
|--------|--------|-----------|
| Election time | < 5 seconds | Randomized timeout (1-2s) + one or two voting rounds. Pre-vote prevents spurious elections. |
| Leader announcement propagation | < 2 seconds | Gossip convergence at O(log 10000) ~ 13 periods * 500ms ~ 6.5s theoretical worst case, but leader aggressively pushes to all council members immediately and they amplify. Practical: < 2 seconds. |
| Steady-state heartbeat overhead | ~50 KB/s total (council) | 150ms interval * 5 members * ~100 bytes per heartbeat. |

### 9.3 State Reconstruction Performance

| Cluster size | Nodes | Expected time | Bottleneck |
|-------------|-------|---------------|------------|
| Small | 10-100 | < 2 seconds | Network RTT for StateReports. |
| Medium | 100-1,000 | < 5 seconds | Council members aggregating reports. |
| Large | 1,000-10,000 | < 15 seconds | Reporting tree fan-in at council members. Each council member receives ~2,000 reports concurrently. |

At 10,000 nodes with a council of 5, each council member receives ~2,000 StateReports. Each StateReport is ~1-10 KB (depends on running apps). Total ingest per council member: ~2-20 MB over 15 seconds, well within network and processing capacity.

### 9.4 Reporting Tree Overhead

| Metric | Value |
|--------|-------|
| Per-worker bandwidth (sending) | ~2 KB every 5 seconds = ~400 B/s |
| Per-council-member bandwidth (receiving, 2000 workers) | ~800 KB/s |
| Leader aggregation bandwidth | ~4 MB/s (5 council members forwarding summaries) |
| Report processing latency | < 1ms per report (deserialisation + state update) |

---

## 10. Testing Strategy

### 10.1 Unit Tests

- **Gossip protocol logic.** Test SWIM state machine transitions (ALIVE -> SUSPECT -> DEAD) with mocked timers. Verify incarnation number conflict resolution. Verify piggybacking dissemination count.
- **Council selection algorithm.** Test zone diversity scoring, stability filtering, tiebreaking determinism.
- **Recovery candidate selection.** Verify priority ordering, zone diversity, stability criteria.

### 10.2 Integration Tests

- **Multi-node gossip convergence.** Start N in-process Mustard instances on localhost. Verify that a membership event (join, leave, failure) propagates to all nodes within O(log N) protocol periods. Measure actual convergence time and assert it is within 2x the theoretical bound.
- **Raft leader election.** Start a 5-member council. Kill the leader. Assert a new leader is elected within 5 seconds. Assert the Raft log is consistent across surviving members.
- **State reconstruction.** Elect a leader, populate the cluster with running apps (via mock Bun agents), kill the leader, wait for a new election, and assert that the new leader's world view matches ground truth within the learning period timeout.
- **Reporting tree failover.** Remove a council member and verify that its assigned workers re-hash to surviving members and send full StateReports within one reporting interval.

### 10.3 Partition Simulation

Use network namespaces (or iptables rules on Linux, pf rules on macOS) to simulate partitions:

- **Council partition.** Split a 5-member council 3/2. Assert the majority group elects a leader. Assert the minority group enters read-only mode. Heal the partition and verify reconciliation.
- **Worker isolation.** Isolate a subset of workers from the council. Assert apps continue running on isolated workers. Assert the leader marks them as state-unknown. Heal and verify reconciliation.
- **Full council loss.** Kill all council members simultaneously. Assert the highest-priority recovery candidate assumes leadership within `catastrophic_timeout` + the candidate's priority delay. Assert a new council is formed and apps are reconciled.

### 10.4 Chaos Tests (via Smoker)

The Smoker fault injection engine (whitepaper Section 18) can be used for ongoing stress testing:

- Random node kills during gossip convergence.
- Random network delays (10-500ms) injected between gossip peers.
- Clock skew injection to test timer-dependent logic (suspicion timeout, election timeout).
- Concurrent council member departures.

### 10.5 Performance Benchmarks

- **Gossip convergence at scale.** Measure propagation time for a membership event at N = 100, 1000, 5000, 10000 nodes. Plot against O(log N) theoretical curve.
- **Leader election latency.** Measure time from leader kill to new leader accepting writes, across 1000 trials. Compute p50, p95, p99.
- **Reconstruction throughput.** Measure time from new leader announcement to learning period completion, at 100, 1000, 5000, 10000 nodes with varying app densities (1, 10, 100, 500 apps per node).

---

## 11. Prior Art

### 11.1 Consul (HashiCorp)

Consul uses **Serf** (which implements SWIM) for gossip-based membership and failure detection, and **Raft** for consensus among server nodes. This is the closest prior art to Reliaburger's architecture.

**What we borrow from Consul:**

- The two-layer architecture (gossip for all nodes, Raft for a small group) is directly inspired by Consul's server/client split.
- The SWIM protocol with suspicion subprotocol (Serf's implementation proved SWIM at scale in production).
- Piggybacking membership updates on probe messages.

**What we do differently:**

- Consul has static server/client roles assigned at installation time. Reliaburger's council is dynamic -- any node can become a council member, and the council is selected at runtime.
- Consul's gossip carries user-defined key-value data of arbitrary size. Mustard strictly limits gossip to fixed-size metadata to preserve O(1) per-node overhead at 10,000 nodes.
- Consul requires an external mechanism for catastrophic server loss. Reliaburger has the pre-seeded recovery candidate mechanism built in.

**References:**

- [Consul Architecture](https://developer.hashicorp.com/consul/docs/architecture)
- [Serf (SWIM implementation)](https://www.serf.io/)

### 11.2 etcd (Raft)

etcd is a distributed key-value store that uses Raft for consensus. Kubernetes depends on etcd as its sole state store.

**What we borrow:**

- Raft log compaction via snapshots (etcd's snapshot mechanism is well-understood).
- Pre-vote extension to prevent disruptive elections from partitioned nodes.

**What we do differently:**

- etcd is a general-purpose key-value store. Reliaburger's Raft log stores only desired state (app specs, scheduling decisions, secrets, config). Runtime state is not in the Raft log.
- etcd requires a fixed set of members configured at startup. Reliaburger's council membership is dynamic and managed by the leader.
- etcd's performance degrades at large key counts. Reliaburger avoids this by keeping the Raft log scoped to a small number of resource types.

**References:**

- [etcd Raft implementation](https://github.com/etcd-io/raft)
- [Raft paper: In Search of an Understandable Consensus Algorithm (Ongaro & Ousterhout, 2014)](https://raft.github.io/raft.pdf)

### 11.3 Kubernetes

Kubernetes depends on etcd for all cluster state (desired and runtime). The API server is the single point of access.

**What we learn from Kubernetes's limitations:**

- etcd is a scaling bottleneck. At large clusters, etcd's watch mechanism and storage requirements become problematic. Reliaburger avoids this by not storing runtime state in consensus.
- The API server is a chokepoint. Reliaburger distributes API reads across council members and routes writes to the leader.
- Kubernetes has no built-in mechanism for catastrophic etcd failure recovery. Operators must maintain etcd backups externally. Reliaburger's reconstructable state and pre-seeded recovery candidates address this.

### 11.4 Nomad (HashiCorp)

Nomad uses Raft for consensus among server nodes and a gossip protocol (Serf) for server-to-server and client-to-server communication. This is architecturally similar to Reliaburger.

**What we borrow:**

- Nomad's approach of using gossip for lightweight coordination and Raft for authoritative state replication.
- Nomad's client-server reporting model (clients report to servers) maps to our reporting tree.

**What we do differently:**

- Nomad has static server/client roles. Reliaburger's roles are dynamic.
- Nomad's servers form a fixed Raft cluster. Reliaburger's council is elastic.
- Nomad does not have a deterministic catastrophic recovery mechanism.

### 11.5 Foundational Papers

- [SWIM: Scalable Weakly-consistent Infection-style Process Group Membership Protocol (Das et al., 2002)](https://www.cs.cornell.edu/projects/Quicksilver/public_pdfs/SWIM.pdf) -- The protocol that Mustard implements. Proves O(log N) convergence and O(1) per-node overhead.
- [In Search of an Understandable Consensus Algorithm (Ongaro & Ousterhout, 2014)](https://raft.github.io/raft.pdf) -- The Raft paper. Our Raft implementation follows this specification.
- [Lifeguard: Local Health Awareness for More Accurate Failure Detection (Hashimoto, 2017)](https://arxiv.org/abs/1707.00788) -- Improvements to SWIM used in Consul's memberlist. We adopt the suspicion subprotocol and local health awareness piggybacking.

---

## 12. Libraries & Dependencies

### 12.1 Raft Implementation

| Crate | Notes |
|-------|-------|
| [`openraft`](https://crates.io/crates/openraft) | **Primary candidate.** Pure Rust, async-first (tokio-native), well-maintained, supports dynamic membership changes, pre-vote, and snapshots. Used by Databend and other production systems. |
| [`raft-rs`](https://crates.io/crates/raft) | Alternative. Port of etcd's Raft. More battle-tested algorithm but less idiomatic Rust API. Lower-level -- requires implementing your own transport and storage. |

**Recommendation:** `openraft`. Its async-native design fits Reliaburger's tokio-based architecture. Dynamic membership changes are a first-class feature, which is critical for our elastic council.

### 12.2 SWIM / Gossip

There is no production-quality Rust equivalent of HashiCorp's `memberlist` (Go). Options:

| Crate | Notes |
|-------|-------|
| [`swim-rs`](https://crates.io/crates/swim-rs) | Experimental. Not production-ready. |
| [`foca`](https://crates.io/crates/foca) | SWIM implementation in Rust. More complete than swim-rs but limited adoption. Worth evaluating. |
| Custom implementation | SWIM is a simple protocol (~1000 lines of core logic). Given our specific requirements (fixed-size messages, custom piggyback payloads, HMAC authentication), a custom implementation may be the pragmatic choice. |

**Recommendation:** Evaluate `foca` first. If it does not meet our fixed-size message constraint or is not sufficiently configurable, implement Mustard as a custom SWIM protocol. The core algorithm is well-specified in the SWIM paper and the complexity is manageable.

### 12.3 Async Runtime and Networking

| Crate | Purpose |
|-------|---------|
| [`tokio`](https://crates.io/crates/tokio) | Async runtime. All of Reliaburger uses tokio. |
| [`tokio::net::UdpSocket`](https://docs.rs/tokio/latest/tokio/net/struct.UdpSocket.html) | UDP transport for gossip. |
| [`tokio-rustls`](https://crates.io/crates/tokio-rustls) | TLS for Raft and reporting tree TCP connections. |
| [`rustls`](https://crates.io/crates/rustls) | TLS implementation (no OpenSSL dependency). |

### 12.4 Serialisation and Cryptography

| Crate | Purpose |
|-------|---------|
| [`bincode`](https://crates.io/crates/bincode) | Binary serialisation for wire protocol messages. Compact and fast. |
| [`serde`](https://crates.io/crates/serde) | Serialisation framework. All data structures derive Serialize/Deserialize. |
| [`ring`](https://crates.io/crates/ring) | HMAC-SHA256 for gossip message authentication. SHA-256 for spec hashing. |
| [`age`](https://crates.io/crates/age) | Encryption for the recovery candidate list and Raft log at-rest encryption. |
| [`arrayvec`](https://crates.io/crates/arrayvec) | Fixed-capacity vectors for piggybacked updates (no heap allocation in the hot path). |

---

## 13. Open Questions

1. **Gossip library vs. custom implementation.** Should we use `foca` or write our own SWIM implementation? The fixed-size message constraint and HMAC authentication are non-standard requirements. A custom implementation is ~1000 lines and fully under our control, but a library gives us battle-tested edge case handling. Decision needed after evaluating `foca`'s extensibility.

2. **Council size auto-tuning.** The whitepaper specifies a configurable council size of 3-7. Should the leader automatically adjust the council size based on cluster size (e.g., 3 at <50 nodes, 5 at 50-500 nodes, 7 at >500 nodes)? Or is a static default of 5 sufficient? Auto-tuning adds complexity but improves fault tolerance at scale.

3. **Recovery candidate notification timing.** Recovery candidates are notified via the reporting tree's mTLS channel. If the reporting tree itself is disrupted (e.g., the candidate's council member parent dies), the notification may not arrive. Should candidates also be able to discover their priority from the encrypted gossip blob alone (requiring them to have a decryption key)? This changes the security model.

4. **Raft log size management.** App specs, secrets, and scheduling decisions accumulate in the Raft log. Snapshot compaction bounds the log size, but the snapshot itself can grow. What is the practical upper bound on snapshot size at 10,000 nodes with 500 apps per node? Do we need an eviction policy for old scheduling decisions?

5. **Gossip protocol period scaling.** The SWIM paper suggests a fixed protocol period. At 10,000 nodes, a 500ms period means each node is probed every ~5000 seconds (83 minutes) on average. Should the protocol period decrease as cluster size grows (faster failure detection at the cost of more bandwidth)? Or is the indirect probe mechanism sufficient?

6. **Reporting tree depth.** The current design is two-level: workers report to council members, council members report to the leader. At 10,000 nodes with 5 council members, each council member handles ~2,000 workers. Should the tree support additional intermediate levels for very large clusters (e.g., "zone leaders" that aggregate before council members)?

7. **Raft read consistency.** Council members serve read-only API requests from local Raft state. This is eventually consistent (a follower's state may lag the leader by a few milliseconds). Is this acceptable for all read operations, or do some reads (e.g., "is this app deployed?") require linearizable reads routed to the leader?

8. **Clock synchronisation.** The learning period timeout and catastrophic recovery timeout depend on wall-clock time. If clocks are significantly skewed between nodes, timeouts may fire at different times. Should we require NTP or use Raft-term-based logical time for these timeouts?

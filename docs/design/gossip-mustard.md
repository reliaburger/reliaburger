# Mustard: Gossip Protocol & Raft Consensus Layer

Design document for the Mustard gossip protocol and the Raft consensus layer in Reliaburger. Covers cluster membership, failure detection, leader election, state reconstruction, and catastrophic recovery.

**Status:** Draft
**Whitepaper reference:** Section 8 (Leader Election & State Management)
**Last updated:** 2026-02-16

---

## 1. Overview

Reliaburger coordinates a cluster of up to 10,000 homogeneous nodes without a dedicated control plane. Two protocols operate at different scales to make this work:

- **Mustard** is the gossip protocol. It is based on SWIM (Scalable Weakly-consistent Infection-style Membership) and runs on every node. Mustard handles cluster membership discovery, failure detection and leader identity broadcast. Node resource summaries and catastrophic-recovery candidate distribution are planned on top of it, not sent today (§3, §5.5). It carries only lightweight, fixed-size payloads and achieves O(log N) convergence with O(1) per-node network overhead.

- **Raft** runs on a small dynamic council of 3 to 7 nodes selected from the cluster. One council member is the leader; the others are hot standbys. Raft handles leader election among council members, replication of desired state (app specs, secrets, scheduling decisions), and cluster configuration changes. Raft is explicitly not used for runtime state.

- **Reporting** is the third communication layer. Each node reports its runtime state (running containers, health, resource usage, job completions) over mTLS. Today every node reports straight to the leader; a hierarchical tree, with council members aggregating for the leader, is planned ([#364](https://github.com/reliaburger/reliaburger/issues/364)). This keeps variable-size payloads off the gossip mesh, preserving SWIM's scaling properties.

**Key design decisions:**

1. Gossip messages are fixed-size. Variable-size runtime state flows through the reporting tree, not through Mustard. This is non-negotiable for scaling to 10,000 nodes.
2. Raft membership is dynamic. Any node can join or leave the council. There are no statically assigned "server" nodes.
3. The cluster can survive total council loss. Today this is an operator-triggered restore from a sealed backup that re-bootstraps a fresh council (`relish council recover`; see §5.5). A fully-automatic pre-seeded recovery, where the priority order is established before the failure so no vote is needed, is the proposed evolution documented in §5.5, not yet shipped.
4. The data plane is never interrupted by a control plane event. Apps keep running during leader elections, state reconstruction, and network partitions.

---

## 2. Dependencies

| Dependency | Component | Why |
|------------|-----------|-----|
| **Sesame** (Security/mTLS) | All inter-node communication | Mustard gossip messages are authenticated with an HMAC-SHA256 key derived (HKDF) from the cluster master key, when one is configured. Raft log entries (not the snapshot) are encrypted at rest under a key derived from the same master key. The reporting tree uses mTLS channels signed by the Node CA. Recovery candidate notifications are sent over mTLS. |
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
  │  [If no ACK, direct or indirect, within a second  │
  │   probe_timeout (waited even with no relays):]    │
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

**A relay never blocks.** When C receives A's PING-REQ for B, it sends B a PING, records "forward B's ACK to A" with a deadline of one `probe_timeout`, and returns. B's ACK arrives through the ordinary message handler, which forwards it to A as an ACK that C sends in its own name with `relayed_for: Some(B)`. Every message a node receives while it waits for its own probe's ACK goes through the same handler, so a node in the middle of a probe still answers PINGs, merges syncs and serves relays. (The first version waited inline for B and dropped everything else that arrived meanwhile, so a busy relay stopped acknowledging its own probers and got itself suspected.)

**Byte budget.** A datagram is at most 1,400 bytes (`MAX_DATAGRAM_BYTES`). A probe is built header first, then the sender's directory extension (§4.1), then piggybacked updates in priority order until the next one doesn't fit; the rest stay queued with their broadcast counts untouched. `MAX_PIGGYBACK_UPDATES` (8) caps the count, but the byte budget is what bounds the size, so a node with 63-byte names, IPv6 addresses and a full label set sends fewer updates per probe instead of oversized datagrams the transport refuses. The budget is measured from the encoded message, not computed from constants, so a field added to the header or extension later is accounted for automatically. In the worst case (16 labels using the whole 512-byte label budget, plus the ten-byte job capability summary) a PING still carries one update; a PING-REQ or relayed ACK may carry none, and anti-entropy, which sends no extension, makes up the difference. If an extension alone would not fit, the probe goes without it. A failed send is counted and logged at most every ten seconds, never discarded.

**Refutation first.** The dissemination queue sends urgent updates ahead of everything else, then Dead/Left, Suspect and Alive about others. An urgent update is an `Alive` that overturns a failure claim: this node's own refutation (Alive about itself at a raised incarnation), and, at every other hop, a member this node held Suspect, Dead or Left that a rumour (or, unless it left, a datagram from the member itself) now says is Alive at a higher incarnation (#2441). Before the relays ranked it so, a refutation re-queued behind each relay's backlog after a correlated failure and spread only as fast as the refuting node could ping; a seeded 300-node simulation (`a_refutation_outruns_a_correlated_failures_backlog` in `tests/gossip_10k.rs`) had it reach 29 survivors in 20 rounds. A plain join (an unknown member turning up Alive) stays at normal priority, so a mass join can't starve failure news. A direct probe of a target this node holds Suspect carries the claim, as an ACK does, so the target can refute straight away. A newer update about a node supersedes any queued older one (incarnation first, then state), which is then discarded unsent. Without both rules, a falsely suspected node's refutation waited behind a rack failure's backlog of Suspect and Dead rumours, and it flapped.

**Anti-entropy push-pull.** Piggybacking alone is not enough. Each update is re-broadcast a bounded number of times (`3 * ceil(log2(N))`, minimum 6), and nothing guarantees those broadcasts reach every node: in the five-node ring simulation, 82 of 100,000 random probe schedules spent every re-broadcast about some member before one particular node heard it. Once the dissemination queues drain, PINGs carry nothing new and the gap is permanent. So, like HashiCorp's memberlist, Mustard runs a periodic push-pull:

```
Node A                                   Node B (random live peer)
  │── Sync { entries[0..8], wants_reply } ──>│  merge each entry
  │── Sync { entries[8..16] } ──────────────>│  (≤ MAX_SYNC_DATAGRAMS)
  │                                          │
  │── Sync { floors[0..8] } ────────────────>│  (≤ 2 datagrams)
  │                                          │
  │<── Sync { entries[0..8] } ───────────────│  B's table, never
  │<── Sync { entries[8..16] } ──────────────│  wants_reply
  │<── Sync { floors[0..8] } ────────────────│
  merge each entry and floor
```

- **When.** Every `push_pull_interval` (10 s, twenty probe periods) a node picks one random live peer. The schedule starts due, so a joining node syncs on its first cycle with a live peer (the join sync) instead of waiting for rumours; an isolated node stays due until it finds one.
- **What.** Every membership entry in any state: node id, address, state, incarnation. A peer that missed a `Dead` or `Left` needs it as much as one that missed a join. The directory extension is not included: it describes only the stamping node, every probe carries it, and probing reaches every member once membership has converged.
- **Merge.** Each entry goes through the same `apply_update`/`resolve_conflict` path as a piggybacked update (incarnation first, then `Dead`/`Left` > `Suspect` > `Alive`), so a stale table can never resurrect a dead node or undo a refutation, and an entry about ourselves in `Suspect`/`Dead`/`Left` triggers the usual refutation. An unknown member is only added when `Alive`. Entries that change the table are re-queued for piggybacking.
- **Floors.** After its entries, each side sends the sequence floors it keeps for reaped members (§8.1), in datagrams of their own: at most `MAX_PIGGYBACK_UPDATES` per datagram and `MAX_SYNC_FLOORS` (16) per exchange, rotating through a larger set like the table.
- **Bounds.** Sync rides the existing authenticated UDP transport (the message HMAC covers the entries). A datagram carries at most `MAX_PIGGYBACK_UPDATES` (8) entries and no directory extension, which stays under 1,400 bytes even with 63-byte names and IPv6 addresses. `[node] name` is held to one DNS label (63 bytes) at start-up, and the datagrams are built by bytes as well as by count (`sync_datagrams`), so a peer with a longer name, which no valid config produces, costs an extra datagram instead of making every sync too large to send (#2394). One side of an exchange is at most `MAX_SYNC_DATAGRAMS` (8) datagrams, so `MAX_SYNC_ENTRIES` (64) entries. A table that fits goes whole every time. A larger one goes as a rotating window over the table sorted by node id, so a 10,000-member table is covered in 157 exchanges, and `tests/gossip_10k.rs` pins both the per-exchange cap and the sweep.
- **Why UDP, not TCP.** memberlist sends the whole table over TCP and scales its interval with log N because the exchange grows with the cluster. Mustard has no gossip stream transport, and adding one means a listener, framing and authentication for a single message type. A capped exchange keeps the per-node cost flat (about 16 datagrams per 10 s, sent and answered, plus up to four of sequence floors, §8.1), so the interval needn't scale. The price is that a 10,000-member cluster sweeps its table over many exchanges, so there anti-entropy is a slow backstop and piggybacking does the real work; in a small cluster every exchange is a full resync.
- **Loss.** Only the first datagram asks for a reply, and a reply never asks for one, so an exchange is one push and one pull. A lost datagram costs that part of one exchange; the next interval tries again.

**Gossip message categories carried by Mustard:**

1. **Membership events** -- node join, node suspect, node dead, node alive (refutation)
2. **Leader identity** -- which node is the current Raft leader (broadcast by leader, propagated by all)
3. **Resource summaries** *(planned — not carried today)* -- per-node CPU/memory/GPU capacity and utilisation. The `ResourceSummary` type exists, but **gossip does not carry it**: no PING/ACK payload piggybacks resources, and `NodeMembership.resources` is always `None` for remote peers (`src/mustard/membership.rs`). This category is design intent, not current wire content.
4. **Recovery candidate blob** *(planned, see §5.5)* -- encrypted list of pre-seeded recovery candidates
5. **Council membership** -- which nodes are currently in the Raft council

### 3.3 Raft Integration

The Raft consensus group operates independently from Mustard but depends on it for two things:

1. **Leader discovery.** When a new Raft leader is elected, the leader announces itself via Mustard gossip so all 10,000 nodes learn the leader identity without Raft having to communicate with them.
2. **Council member replacement.** When a council member departs, the leader uses Mustard's membership list to find a suitable replacement.

**How a node resolves the leader** (`src/cluster/directory.rs`, `resolve_leader`). The leader stamps a `LeaderHint` (its id, term, recovery epoch and endpoints) on its gossip, and every node relays the newest one it has seen, ordered by recovery epoch and then term. A Raft member (voter or learner) trusts its own metrics, and takes a hint over them only when the hint is newer and its term is at most ten above the member's own (`MAX_LEADER_HINT_TERM_LEAD`), so an absurd term can't carry a member's reporting epoch away. The bound applies only to a hint from the recovery epoch the member's own term belongs to (`NodeDirectory::local_recovery_epoch`, from its fence): a fenced voter of a replaced council still holds its old term, which says nothing about the new council's term line. A node outside the Raft membership never hears AppendEntries and stays at term 0, so that bound would lose it the leader from the cluster's eleventh election on; it takes the hint as it is ([#1996](https://github.com/reliaburger/reliaburger/issues/1996)). Gossip is HMAC-authenticated with the cluster key, but every node holds that key, so a hint proves nothing about the node it names. The reporting epoch only gates anything on the leader, which is a member, and a seatless node fetches its security state only from a hinted leader that is a voter of the council it knows, over TLS bound to that voter (#2406, `docs/design/security-sesame.md`). **Limit:** a forged hint with a huge term still outranks the real one on a seatless node until it restarts, so it can stall that node's reporting; hints don't expire yet.

A recovery (`relish council recover`) starts a fresh term line at a new recovery epoch, so a term only compares within one epoch. Each node's gossip directory holds a recovery floor, the newest epoch its own recovery fence knows (`FenceSnapshot::newest_known_epoch`). It never adopts a hint from below the floor and drops one when the floor rises, so the replaced council's hint, still relayed by survivors, can't outrank the recovered leader. The leader-target maintainer keys the reporting epoch on `(recovery_epoch, term)`: the node's own Raft metrics always set it, even downwards, and a gossip hint only moves it forward. Before this, an old council's higher term could raise a recovered leader's reporting epoch for good, and the scheduler skips every pass while that epoch differs from the leader's term ([#1997](https://github.com/reliaburger/reliaburger/issues/1997)).

Raft is used for:

- Leader election among council members
- Log replication of desired state (app specs, secrets, scheduling decisions)
- Cluster configuration changes (council size, gossip parameters, etc.)
- Membership changes to the council itself (AddNode / RemoveNode log entries)

Raft is **not** used for:

- Runtime state (what is running where)
- Failure detection (that is Mustard's job)
- Communication with non-council nodes

#### Forwarding leader-only API calls

openraft doesn't forward client writes, so a node that can't confirm it leads (`CouncilNode::is_leader`, a quorum check) forwards leader-only API calls to the leader's API, carrying the caller's own credential. Two rules stop a forward from looping ([#1069](https://github.com/reliaburger/reliaburger/issues/1069)):

- **Never to itself.** `leader_api_url` and `upgrade::leader_base_url` return `NoLeaderRoute::ThisNode` when Raft names this node as leader (`current_leader`, or its metrics). That node is a restored leader in the startup recovery probe, or a leader cut off from its quorum. The route answers 503 "this node holds the leader role but can't confirm it with a quorum" and makes no outbound request. `NoLeaderRoute::Unknown` keeps each route's "no cluster leader known yet" answer.
- **Never twice.** Every forwarded request carries `x-reliaburger-forwarded: 1`. A node that receives a marked request and can't confirm it leads answers 503 (`NoLeaderRoute::AlreadyForwarded`) instead of forwarding again; a confirmed leader serves it. The header only ever turns a forward into a 503, so it needs no authentication of its own. The older per-route markers (`x-reliaburger-upgrade-forwarded`, `-lease-forwarded`, `-forwarded-sign`, `-desired-apps-forwarded`, `-webhook-forwarded`) still travel alongside it.
- **Never without a deadline** ([#2055](https://github.com/reliaburger/reliaburger/issues/2055)). Every forward is bounded. The shared ones (`batch::relay_leader_answer`, behind `forward_to_leader`, `forward_get_to_leader` and an app's stop, delete and release) wait at most `LEADER_FORWARD_TIMEOUT` (60 s) for the leader's whole answer and then answer 504, saying the outcome is unknown. A streamed answer (a job apply's events, through `relay_leader_stream`; an app apply in `cluster_apply`, 5 s) bounds only the wait for the leader's headers, then streams for as long as the leader writes. A job apply's header deadline (`job_apply_header_deadline`) covers the leader's work before its stream starts: 20 s for each distinct image it binds, plus the leadership proof and two council writes, at least 60 s and at most 280 s. The other forwards keep their own deadlines: upgrade calls 30 s, GitOps webhooks, test leases 5 s, node faults, summaries, desired-app reads, election holds and the token and join-token routes 20 s (longer than the leader's 3 s proof, 10 s write and 2 s replica wait). On the leader, `CouncilNode::write` gives up after `WRITE_TIMEOUT` (10 s) with `CouncilError::WriteTimedOut` ("outcome unknown"), which request handlers answer as 503. Callers that need another deadline use `write_within`: the scheduler's loop 5 s, `relish apply`'s writes 60 s (its error says re-running is safe), and a fresh council's bootstrap writes 120 s each.

#### Raft transport connections

Raft RPCs travel over TCP with mTLS (`src/council/network.rs`). Each node keeps a small pool of established connections per peer (`src/council/connection_pool.rs`, [#928](https://github.com/reliaburger/reliaburger/issues/928)), so a heartbeat or append doesn't pay for a TCP and TLS handshake. This matters because a leader confirming its leadership for a linearizable read gives each heartbeat only `heartbeat_interval` (150 ms) to come back; on a debug-build Lima cluster a handshake per RPC exceeded that (found while proving [#870](https://github.com/reliaburger/reliaburger/issues/870)). One missed round doesn't make a settled leader refuse a read: the lease routes call `CouncilNode::confirm_leadership`, which repeats the check every heartbeat interval for up to 3 s while the node's own metrics still name it leader, and answers "not leader" at once on a node that names another voter or none ([#1009](https://github.com/reliaburger/reliaburger/issues/1009)).

- **Client.** An RPC takes an idle connection for its peer (keyed by address and the node name the certificate is checked against) or dials one, sends one request, reads one response and returns the connection. A cancelled RPC drops its connection, so a connection with a request in flight is never reused. If a pooled connection fails, the RPC is sent once more on a fresh dial (Raft RPCs are idempotent at the receiver). At most 4 idle connections are kept per peer.
- **Retirement.** A pooled connection is closed instead of reused once it is 30 s old or 10 s idle, when this node's identity changed since it was dialled (renewal or new trust set, so the next handshake presents the new certificate), or when the peer's presented chain is now on the CRL.
- **Server.** A connection serves requests until the peer is idle for 20 s or the connection is 60 s old (the client's limits are shorter, checked by `const` assertions). Before each request it re-checks the node fault gate and the client's chain against the live CRL, so a revocation takes effect at the next RPC. The handshake and each request keep the 10 s deadline (CP11).
- Both ends set `TCP_NODELAY`, and a frame goes out in one write.

**Snapshot transfer** ([#2367](https://github.com/reliaburger/reliaburger/issues/2367)). openraft sends a snapshot in chunks and gives each chunk `install_snapshot_timeout` to be sent, decoded and answered; a chunk that misses it restarts the install from offset 0. openraft's defaults (200 ms and 3 MiB chunks) meant a member needing a snapshot over a slow link never caught up, so `to_openraft_config` sets both: 10 s per chunk (`SNAPSHOT_CHUNK_TIMEOUT_MS`, the transport's own RPC bound, and enough for the last chunk's decode and fsync on the follower) and 256 KiB per chunk (`SNAPSHOT_CHUNK_BYTES`). The chunk's bytes travel as base64 (`snapshot_chunk_wire` in `src/council/network.rs`), about 1.33 bytes of frame per byte of snapshot; `serde_json` would write the `Vec<u8>` as a decimal number array, about 3.6. A node keeps its latest snapshot's bytes once, behind an `Arc` the state machine and the redb write share (#2452); each send to a follower copies them, because openraft's snapshot type (`Cursor<Vec<u8>>`) owns its bytes.

**Council backups** (`src/council/backup.rs`). The leader's backup is the snapshot JSON sealed with AES-256-GCM under a key derived from the master key, wrapped in a JSON envelope: `version`, `hkdf_salt`, `hkdf_info`, `nonce` and `ciphertext`. The ciphertext is one base64 string (#2452), so a backup is about 1.33 times the state's JSON; as a number array it was 3.6 times. The envelope `version` is `CURRENT.state`, so a backup from a release with another state format is refused by name, not mis-parsed.

**AppendEntries batches** ([#2370](https://github.com/reliaburger/reliaburger/issues/2370)). openraft sends up to `max_payload_entries` (300, set explicitly as `MAX_PAYLOAD_ENTRIES`) per AppendEntries within `heartbeat_interval`, and reads them with `limited_get_log_entries`. Both log readers stop before the entry that takes the batch past `LOG_READ_BUDGET_BYTES` (4 MiB of stored JSON, about 35 ms on 1 Gbit/s) and always return at least one entry. Three size limits are ordered by `const` assertions: a council write over `MAX_ENTRY_BYTES` (32 MiB of request JSON) is refused with `CouncilError::EntryTooLarge` before it reaches the log; the sender refuses a frame over `RAFT_SEND_LIMIT_BYTES` (48 MiB) and returns `RPCError::PayloadTooLarge` with half the entries as its hint, so openraft retries with a smaller batch at once; the receiver drops a frame over `MAX_RAFT_RPC_SIZE` (64 MiB). **Limit:** a single entry still has to fit the 150 ms deadline, so a multi-megabyte entry on a slow link can still miss it.

**Committed pointer** ([#2437](https://github.com/reliaburger/reliaburger/issues/2437)). openraft calls `save_committed` before every apply. `DurableLogStore` keeps the pointer in memory and writes it inside the next append's redb transaction, so a committed batch costs one fsync, not two. A pointer no append has carried within 1 s (`COMMITTED_FLUSH_DELAY`) gets a durable commit of its own, and a clean stop writes it (the runtime's shutdown task calls `CommittedFlusher::flush`, and dropping the store's last handle does too). The vote stays immediately durable. After a crash the pointer can be one batch behind, never ahead of the log: openraft re-commits those entries once a leader is known, and `relish council recover` on a crashed survivor replays up to the older pointer. `get_log_state`, `read_vote` and `read_committed` read redb on the blocking pool, as every write does.

**Council membership check.** The handshake proves only that the peer holds a node certificate, and every node holds one. So each RPC also has to come from the council (`src/council/admission.rs`). In openraft 0.9 an `AppendEntries` or `InstallSnapshot` is sent only by the leader and a `Vote` only by a candidate, and both are voters; a learner receives replication but never sends an RPC. The server therefore checks the sender, never the receiver: the node the RPC's vote names (`voted_for`) must be a voter in this node's council, and must be the node named in the connection's certificate. "This node's council" is the voters of its committed and effective Raft memberships together, read from openraft's own state, not from gossip. The effective membership includes an appended but uncommitted change, so a voter added in a joint config is known as soon as the entry arrives. The committed one keeps a leader that removes itself heard until that removal commits, after its followers have already appended the entry that drops it. An RPC that fails the check is closed without an answer, and its recovery epoch never moves this node's fence; when it carries an older epoch, the server still replies `Fenced` with the newer one, so an old voter learns of a recovery.

- **Learners and fresh nodes.** The leader dials a learner, so adding one needs nothing from the check. A fresh node with no membership at all has no council of its own to check against, so it checks against the council it *expects* (#2397). The leader that issues a joining node's certificate puts its council's voters, with their Raft addresses, in the join bundle (`JoinBundle::council`), and `relish join` (or an appliance's `bun appliance prepare`) saves them as `council.json` beside the identity. The node serves an RPC only from a voter of that council, or one they vouch for, under that voter's own certificate, and adopts that council's recovery epoch while it holds none; anyone else is refused, and its epoch is never adopted. Every 30 s the node refreshes the file: from its own membership once it has one, and while it has none, from what the voters it expects name now (`CouncilVoters` answers carry addresses for this), so a node past the council cap follows a council that changes around it as long as one voter it knows still answers. A node with no membership that already holds an epoch (a survivor `relish council recover` stamped, before it re-forms its council) serves only its own epoch and can't be fenced over Raft. `relish council re-enrol` keeps the identity directory, so a re-enrolled voter expects the council it last belonged to, and a voter of that council that has joined the recovered one vouches for the new leader. **Limit:** a seatless node trusts the leader it joined through, and the voters that leader named; a node joined through a compromised leader is that leader's anyway.
- **A member whose view is behind.** A voter that was down while the leader joined holds a membership without it, and would refuse it until leadership moved. Its vote matters as much: a council that swapped a voter while this one lagged, and then lost its leader, may need this voter's vote to elect the new one. When a stranger sends it replication, or asks for its vote with a log at least as long as its own, it asks the voters it does know, with the `CouncilVoters` RPC over node-id-bound mTLS, which nodes are voters in their councils (one round per 2 s, 2 s per answer). If one names the sender, with the certificate's node name, the member admits that node for 60 s, long enough to read the membership from its own log. Only a node serving Raft at the asker's own epoch answers. One voucher is enough: a voter in the member's own view can already drive its Raft. **Limit:** vouching needs one live voter the member knows. If a voter is swapped out while a member lags, and that voter and the leader then both stop, the member can reach none of its known voters and won't vote for the new one; when the new voter needs that vote, the council has no leader until the old leader or the swapped-out voter returns. The way out is `relish council recover` or re-enrolling the lagging node (manual, chapter 12, "A voter that missed a swap"). Admitting an unvouched leader would hand that node's log to whoever asked, so the check doesn't.
- **Plaintext.** Without mTLS there is no certificate to bind, so the claimed sender must still be a voter but isn't authenticated. A plaintext node never runs `relish join`, so with no membership and nothing to expect it still serves whoever replicates to it: nothing on a plaintext cluster is authenticated.

**Measured heartbeat round trips.** `examples/raft-heartbeat-rtt.rs` forms a three-voter council over the production transport (mTLS, live identities, node-bound connectors) on loopback and times 1,000 `ensure_linearizable` calls on the leader, 5 ms apart. Apple M2 Max (12 cores), 17 October 2026, host otherwise quiet (no builds, one-minute load average below 3 at the start); "loaded" adds 12 `yes > /dev/null` processes, one per core. Debug builds include the optimised `ring` override from [#922](https://github.com/reliaburger/reliaburger/pull/922). No sample failed or exceeded 150 ms in any run.

| Build | Host | Transport | p50 | p90 | p99 | max |
|-------|------|-----------|-----|-----|-----|-----|
| debug | idle | handshake per RPC | 2.50 ms | 3.56 ms | 5.13 ms | 6.60 ms |
| debug | idle | pooled | 0.53 ms | 1.82 ms | 2.44 ms | 3.65 ms |
| release | idle | handshake per RPC | 1.43 ms | 2.29 ms | 3.62 ms | 4.66 ms |
| release | idle | pooled | 0.26 ms | 0.58 ms | 1.20 ms | 4.06 ms |
| debug | loaded | handshake per RPC | 3.13 ms | 6.11 ms | 14.07 ms | 21.06 ms |
| debug | loaded | pooled | 0.91 ms | 5.62 ms | 11.50 ms | 20.10 ms |
| release | loaded | handshake per RPC | 1.50 ms | 5.76 ms | 12.25 ms | 107.74 ms |
| release | loaded | pooled | 0.28 ms | 4.16 ms | 10.33 ms | 16.92 ms |

Pooling cuts the median by about five times in both profiles. Under load the tail is set by CPU scheduling rather than handshakes, so pooling helps less there, but it removed the one run whose worst heartbeat used most of the 150 ms budget. Loopback with the optimised `ring` doesn't reproduce the Lima failure itself, where each handshake also crossed virtual NICs on a slower, unoptimised build; the dev cluster was not re-measured.

### 3.4 Council Selection Algorithm

The leader selects council members from the general node pool. Selection criteria, in priority order:

1. **Stability.** Older nodes are preferred: after zone novelty, candidates are sorted by node age, oldest first. `CouncilSelectionConfig::min_node_age` defaults to 10 minutes, but the running reconciler (`src/cluster/runtime.rs`) overrides it to 0 so a fresh cluster can form a council immediately. Age therefore affects *ordering*, not eligibility. **TODO (not yet implemented):** a hard minimum age for joining an established council, so a freshly joined node can't become a council member before it has proven reliable.
2. **Resource availability** *(criterion present in code, but a no-op in production).* The selector filters on CPU < 90% / memory < 85%, but because per-node resources are never gossiped (`NodeMembership.resources` is always `None` for remote peers), the filter treats every node as eligible — an unreported node passes. This criterion only bites once resource gossip is implemented; today it rejects no one.
3. **Zone diversity.** If node labels include zone or region information, the leader maximizes geographic distribution among council members. A council where all members are in the same rack defeats the purpose of redundancy.
4. **Random tiebreaker.** Among otherwise-equal candidates, selection is random (seeded by the leader's node ID + current term for reproducibility).

**Odd voter counts (D12, #694).** The planner (`plan_council_action`) targets the largest odd number of voters not above the live nodes (live voters, live learners and live spare candidates) nor `max_council_size`. An even council tolerates no more failures than the odd one below it and adds a write acknowledgement, so no planner action ever lands on an even count:

**Nodes past the cap (#1477).** A node the planner never seats has an empty replica, so it reads the leader's view of the security state instead (`/v1/cluster/security-view`, refreshed every five seconds). Its API tokens, CRL, trust set and age keys come from there. `docs/design/security-sesame.md` lists what the view holds.

| Live nodes | Voters |
|---|---|
| 1-2 | 1 |
| 3-4 | 3 |
| 5-6 | 5 |
| 7+ | 7 |

Growth promotes two caught-up learners in one joint-consensus change (`PromotePair`, 1 to 3 or 3 to 5). A dead or disk-pressured voter is replaced by swapping a caught-up learner in for it in one change (`Replace`), so the count never passes through an even number. With no replacement, two dead voters leave together (`RemoveVoterPair`, 5 to 3, never below `min_council_size`). An even council found on entry (left by an older release, or by a decommission) goes back to odd: up by one (`Promote`) when a caught-up learner exists and the live nodes allow it, otherwise down by one (`RemoveVoter`) while quorum allows. The voter it removes is, in order: one dead past `dead_window`, one under disk pressure, any other voter that is down (dead inside the window, suspect, or unseen by gossip), and only then the highest-numbered live voter that isn't the leader (#2374). Shrinking removes a seat either way, so the planner never keeps a down voter over a live one; a removed voter that comes back rejoins as a learner. Spare eligible nodes join as learners up to `max_council_size`, and only nodes past that stay plain workers. A learner has the replicated state, and an mTLS node needs it: its API opens only once the API tokens reach it through Raft (#1477). A two-node cluster therefore has one voter and one learner: losing the other node is survivable, losing the voter stops the control plane until it returns or is recovered. The upgrade planner's refusal of a two-voter council (`src/upgrade/plan.rs`) stays as a guard.

**A replaced voter coming back holds its elections (#1524).** A voter that was replaced while it was down comes back with its old Raft state, which still lists it as a voter, because it missed its own removal. On its election timer it would campaign, and each failed campaign raises its term past the leader's; when the reconciler then adds it as a learner, the leader's first replication meets the higher term and steps down (openraft 0.9's `HigherVote`). So a node that starts on a log store holding a vote starts openraft with elections off (`src/council/return_hold.rs`). The hold ends at the first AppendEntries from a leader, which the TCP server and the in-memory router note on the node's `LeaderContact`; or once a startup grace of three election timeouts passes while gossip names no live leader (the leader's own hint, for an alive node); or at a cap of ten election timeouts. A voter that's still a member hears from the leader within a heartbeat; a whole-council restart waits at most the grace. `a_replaced_voter_coming_back_joins_as_a_learner_without_an_election` in `tests/council_self_healing.rs` checks it.

Council replacement flow:

```
Council member C departs
     │
     ├── Was C the leader?
     │     YES: the followers miss its Raft heartbeats and elect a
     │           new leader among the remaining members (<5s)
     │     NO:  Current leader continues
     │
     ▼
Gossip declares C dead; the leader's HealthTracker stops counting it alive
     │
     ▼
Leader evaluates candidate pool via Mustard membership
     │
     ▼
Leader selects replacement node R
     │
     ▼
Leader adds R as a learner
     │
     ▼
R receives Raft log snapshot + incremental entries
     │
     ▼
R catches up to current log position
     │
     ▼
Leader swaps R in for C in one membership change (count stays odd)
```

### 3.5 Hierarchical Reporting Tree

> **Status: the shipped topology is a flat star to the leader, not the two-level tree below.** What runs today (`src/cluster/runtime.rs`) has **every worker report directly to the leader**. The runtime's `spawn_leader_target_maintainer` pushes a *single-element* council list containing only the leader, and each `ReportWorker` runs the same `assign_parent(worker_id, council)` helper over that one-element list — so the hash always resolves to the leader. The two-level consistent-hash tree (workers → council member → leader) is deliberate MVP scaffolding that exists in the code (`src/reporting/assignment.rs`, `src/reporting/aggregator.rs`) but is **not wired into the running topology**. The fan-in arithmetic and diagram below describe that **planned** two-level design. The council-to-leader message the tree would need is gone: `ReportingMessage::AggregatedReport` bundled many nodes' reports with no identity binding, so any authenticated peer could forge another node's state through it, and production never sent it (#696). If the tree is built, that message comes back with council-peer authentication.

In the planned tree, each non-council node is assigned to a specific council member as its parent, deterministically: `parent = council_members[hash(node_id) % council_size]`, distributing the reporting load evenly.

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

Under that planned design, with a council of 7 and 10,000 total nodes each council member would aggregate reports from ~1,400 worker nodes. **Today, with the flat star, the leader receives all ~10,000 StateReports itself** — which is exactly why the two-level tree is the documented next step for large clusters. Workers send full StateReports at the reporting interval (default 5 seconds).

**Admission at the leader's report listener** (`TcpReportingTransport::accept_loop` in `src/reporting/transport.rs`). Every node, the leader included, reports over TCP (mTLS when the node has an identity) to the leader's reporting port, one connection per message. The listener keeps two budgets so that a socket which proves nothing can't take a slot a report needs (#2380):

- **Pending** (`MAX_PENDING_CONNECTIONS`, 64): from accept until the whole length-prefixed frame has arrived, including the TLS handshake that checks the client certificate. One source address may hold at most `MAX_PENDING_PER_SOURCE` (4) of them; loopback is exempt, since the hollow-node harness runs every node on 127.0.0.1. When the budget is full and a new connection is waiting, the oldest pending connection is closed once it has been pending for `PENDING_EVICTION_AGE` (1 s); until then the newcomer waits and later connections queue in the kernel's listen backlog.
- **Admission** (`MAX_REPORT_CONNECTIONS`, 64): a connection with a whole frame waits here for room in the aggregator's 256-deep queue, and is acknowledged only once queued. A sender whose report finds no room inside its ten-second deadline gets no acknowledgement and counts the report as refused (#1411).

A connection task gives back its pending slot only after it holds an admission slot, so there are never more than 128 connection tasks. The aggregator stores everything queued and publishes its view once per burst (#1411); it reconciles its freshness table with gossip membership only when membership or the leadership epoch changes, never per report (#2440).

When (in the planned tree) a council member departs, its assigned workers re-hash to the remaining council members and the new parent receives a full StateReport on the next cycle. In the flat star there is only ever one parent — the leader — so a leader change simply re-points every worker at the new leader.

---

## 4. Data Structures

### 4.1 Gossip Message Types

```rust
/// Top-level gossip message.
/// Sent as a single UDP datagram of at most MAX_DATAGRAM_BYTES (1400) to
/// avoid fragmentation; probes fill what the header and extension leave
/// with piggybacked updates (§3.2, "Byte budget").
#[derive(Serialize, Deserialize)]
struct GossipMessage {
    /// Protocol version; a node refuses any other generation.
    version: u8,
    /// Durable state generation required for cluster admission.
    state_format: u32,
    /// Sender's node ID. Always the node that built and sent the datagram.
    sender: NodeId,
    /// Monotonic incarnation number (incremented on refutation).
    incarnation: u64,
    /// The sender's message counter (§8.1): seeded from wall-clock
    /// microseconds at process start, +1 per message. Receivers drop a
    /// message no fresher, by (incarnation, sequence), than the freshest
    /// seen from its sender, or than its floor once it has been reaped.
    sequence: u64,
    /// HMAC-SHA256 over the message with this field zeroed, keyed by the
    /// gossip key derived from the cluster master key.
    hmac: [u8; 32],
    /// The message payload.
    payload: GossipPayload,
    /// Directory extension (endpoints, leader hint, labels, disk pressure,
    /// recovery claim, job capabilities), encoded after the body with its
    /// own HMAC. Not serialised inside the body. The job capabilities
    /// (#788) are nine flags saying which kinds of job the node runs; the
    /// leader refuses a job run at submission when every live member's
    /// flags refuse it.
    extension: Option<DirectoryExtension>,
}

#[derive(Serialize, Deserialize)]
enum GossipPayload {
    Ping {
        /// Piggybacked membership updates (byte-budgeted, at most
        /// MAX_PIGGYBACK_UPDATES).
        updates: Vec<MembershipUpdate>,
    },
    PingReq {
        /// Node that the sender wants the receiver to probe on its behalf.
        target: NodeId,
        /// Original requester, so the indirect probe result can be routed back.
        requester: NodeId,
        updates: Vec<MembershipUpdate>,
    },
    Ack {
        updates: Vec<MembershipUpdate>,
        /// `Some(target)` when a relay forwards the target's ACK in its own
        /// name; `None` when the sender answers for itself.
        relayed_for: Option<NodeId>,
    },
    /// Anti-entropy push-pull: up to MAX_PIGGYBACK_UPDATES entries of the
    /// sender's full membership table. Only the first datagram of a request
    /// sets wants_reply; replies never do.
    /// A datagram carries entries or sequence floors (§8.1), never both.
    Sync {
        entries: Vec<MembershipUpdate>,
        floors: Vec<SequenceFloor>,
        wants_reply: bool,
    },
    /// The answer to a datagram dropped as no fresher than what the
    /// sender holds (§8.1): the receiver refutes past `held_incarnation`
    /// only if what's held is fresher than anything it has sent this run.
    Stale {
        held_incarnation: u64,
        held_sequence: u64,
        updates: Vec<MembershipUpdate>,
    },
}

/// A reaped member's freshest accepted (incarnation, sequence), with the
/// seconds left before it expires (a floor travels with its remaining
/// time, never a fresh one).
struct SequenceFloor {
    node_id: NodeId,
    incarnation: u64,
    sequence: u64,
    expires_in_secs: u32,
}

/// Largest datagram any node sends.
const MAX_DATAGRAM_BYTES: usize = 1400;

/// Most piggybacked membership updates per gossip message. The byte
/// budget is the real limit; this bounds the work per message.
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
    /// Resource summary. Planned to be updated via gossip piggyback, but
    /// gossip does not carry resources today, so this is always `None` for
    /// remote peers.
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
| Mustard gossip (PING/ACK/PING-REQ, including join via seed PING) | UDP | postcard (serde) | 1400 bytes (avoids IP fragmentation); piggybacked updates fill the room left after the header and directory extension |
| Mustard anti-entropy (Sync push-pull) | UDP | postcard (serde) | 1400 bytes per datagram, at most 8 datagrams (64 entries) per side per exchange |
| Raft (AppendEntries, Vote, InstallSnapshot, CouncilVoters) | TCP + mTLS, connections pooled and reused across RPCs (§3.3) | JSON (serde), length-prefixed, in an envelope carrying the compatibility version and recovery epoch | 64 MiB per frame (snapshots can be large) |
| Reporting tree (StateReports) | TCP + mTLS | postcard | 1 MiB (bounded by max events per report) |

All TCP connections use mTLS via Sesame's Node CA certificates.

---

## 5. Operations

### 5.1 Gossip Protocol Lifecycle

**Node join:**

Two separate things are called "join", and they are not the same step.

*Identity enrolment* (`relish join`) is a one-shot HTTP call, not a gossip seed. The operator runs `relish join --token <token> <api-address>` where the address is an existing member's **HTTPS API** endpoint (e.g. `https://10.0.1.5:9117`). `relish` POSTs to `{addr}/v1/cluster/join` (TOFU-pinned by CA fingerprint), Sesame issues the node its mTLS identity, and `relish` writes the cert/key to the identity directory. No gossip and no membership exchange happen here — it only mints credentials.

*Gossip membership* is UDP SWIM, seeded from `[cluster] join` in `node.toml`:

```
1. N's Bun reads its gossip seed addresses from [cluster] join
   (e.g. "10.0.1.5:9443").
2. N sends an (empty) SWIM PING over UDP to each seed. The reply
   registers the seed's real NodeId and piggybacks membership updates.
   There is NO TCP JOIN message.
3. On its next cycle N has a live peer and its first anti-entropy
   push-pull is due: it pushes its table to the seed and pulls the
   seed's (bounded, see §3.2), so it learns the cluster in one exchange.
4. N runs the SWIM protocol period loop:
   - Each period: pick a random node, PING it (UDP)
   - Piggyback own ALIVE state on all messages
   - Within O(log N) periods, all nodes learn about N via piggyback
   - Every push_pull_interval, resync with one random live peer
5. N begins reporting its runtime state up the reporting tree.
6. Leader adds N to the scheduling pool once N's first StateReport arrives.
```

**Node departure (graceful):**

Leave is **immediate**. There is no `LEAVING` state, no drain-wait, and no `relish drain` command. The SWIM state machine has four states only — `Alive`, `Suspect`, `Dead`, `Left` — with no intermediate draining state.

```
1. Bun receives SIGTERM.
2. Bun sets its own gossip state to Left and enqueues that update.
3. Bun sends a best-effort burst of PINGs to spread the Left state.
   It does NOT wait for acknowledgement.
4. Other nodes that receive the update mark N as Left and drop it from
   the scheduling pool; the leader reschedules N's apps.
```

(The only "drain" in the codebase is `FaultAction::NodeDrain`, a Smoker fault-injection action that *simulates* a graceful departure for testing — not a production drain workflow.)

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

**Tombstones (#2474).** REMOVED isn't quite forgetting. A node keeps the incarnation it reaped a member at, as a tombstone beside the member's sequence floor (§8.1), so it shares the floor's 24-hour retention and its bound of 1,024 entries. A rumour that the member is `Alive` at or below that incarnation is dropped, so a peer that missed the death (one that was cut off, or one the rotating sync window never showed the `Dead`) can't re-add it on every node. A rumour at a higher incarnation is a real return and passes, and so does the member's own fresh datagram, which clears the floor and the tombstone with it. A restarted member that the floor answers with `Stale` is told the tombstone's incarnation, so it refutes past both. Tombstones aren't shared in anti-entropy: a node that never reaped the member itself has none, and can still take one stale `Alive` about it, which it probes and declares dead again on its own.

**Suspicion subprotocol.** When a node is marked SUSPECT, the suspecting node begins a suspicion timer. During this period, any node can forward evidence that the suspect is alive (an ACK received from the suspect). If the suspect itself learns it is suspected (via piggybacked updates), it increments its incarnation number and broadcasts an ALIVE message, which overrides the SUSPECT state. It refutes only a claim at or above its own incarnation: one below has already lost to its own record, and refuting it would only spread a needless bump. Only if the suspicion timer expires without refutation does the node transition to DEAD. This prevents false positives from transient network blips.

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
| Pre-vote | **TODO (not yet implemented)** | Would prevent disruptive elections from partitioned nodes that have not received recent heartbeats: nodes would need a majority of pre-vote grants before incrementing their term. `openraft` 0.9 doesn't implement pre-vote, so a node returning from a partition can still force an election. |

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

#### Moving leadership off a voter

openraft 0.9 has no leadership transfer. Two paths still need to move leadership off a healthy leader: disk-pressure resignation (§5.5's step-down, run by the council reconciler) and the admin endpoint `POST /v1/cluster/elect`. Both use the same two steps (`src/council/handover.rs`):

1. **The old leader holds its elections.** `CouncilNode::hold_elections_until` sets openraft's `runtime_config().elect(false)`: the node still votes, replicates and leads, but never campaigns on its own timer. Without this, the election that follows a deposition goes to the longest log, which is usually the old leader's (it wrote the newest entries), and the old leader wins straight back (#646, #755).
2. **Another voter campaigns.** Its higher term deposes the leader. Followers refuse every candidate until the leader lease (the maximum election timeout) runs out, so a single campaign often only deposes; the next election, from a timer or a repeated campaign, picks the most up-to-date remaining voter by Raft's own vote rule.

The hold ends when another voter leads, or after `HANDOVER_HOLD` (30 s) if nobody does, so a hold can't leave the council leaderless. Repeated holds only extend the deadline.

- **Disk pressure.** A pressured voter holds its elections on every reconciler tick while it leads or no leader is known, provided a healthy voter that isn't under pressure could lead instead. The deposition target is the first such voter; it campaigns at most once per reconciler operation timeout. Holding while no leader is known covers a follower's campaign that deposes the leader before the leader's own tick has held.
- **`POST /v1/cluster/elect`** (cluster admin), sent to the voter that should take over. That voter forwards a hold request to the leader (`x-reliaburger-elect-hold`, carrying the caller's credential so the leader repeats the admin check), then campaigns every 1.5 leader leases for up to 20 s until another voter leads, and answers `{"status": "leadership moved", "leader", "previous_leader"}`. A voter with a longer log may win instead of the receiving node; that is still a handover. Sent to the leader itself or to a non-voter, it answers 409. If the leader can't be asked to hold, it answers 502 or 504 without campaigning; if leadership doesn't move in time, 504.

Neither path is a bounded membership operation, so the timed-out-operation finisher in §5.7 (#673) has nothing to finish: `trigger().elect()` returns once the campaign starts, and the hold is local state with its own deadline.

**Leader election performance target: < 5 seconds.** This includes the election timeout (up to 2 seconds), one or two rounds of voting if the first round has a split vote (up to 4 seconds in the worst case), and the leader announcement via gossip (~1 second to propagate). In practice, elections complete in 1-3 seconds.

### 5.3 Council Selection Algorithm

The candidate ranking below feeds the planner, which caps the voter count at the largest odd number the live nodes and `max_council_size` allow (§3.4, D12). `target_size` here is the cap; the planner never seats an even council even when more candidates rank.

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
            // Must have been in the cluster long enough. The running
            // reconciler sets min_node_age to 0, so today this never
            // excludes anyone; age only orders candidates.
            && n.first_seen.elapsed() >= config.min_node_age
            // Must not be overloaded. NOTE: unreported == eligible.
            // Because gossip never carries resources, `resources` is always
            // None in production and this check passes for every node
            // (`is_none_or` in the real `src/council/selection.rs`).
            && n.resources.as_ref().is_none_or(|r| {
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
    let selected = top.sample(&mut rng, needed);

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

> **Status — what ships vs what's proposed.** Full-council loss recovery ships as an **operator-triggered** flow, not the fully-automatic pre-seeded election described below. `relish council recover` runs against a stopped survivor (`src/council/recovery.rs`): it restores the desired state from the last sealed external backup (`src/council/backup.rs`) or the node's own durable snapshot and committed log, re-bootstraps a fresh single-voter Raft with a bumped recovery epoch, and the reconciler regrows the council from healthy members. Anything written after the last backup is lost, which is why a human confirms the restore. The pre-seeded recovery-candidate machinery in steps 1–5 below — priority-ordered candidates, the encrypted gossip candidate blob, and a survivor assuming leadership *without* an election — remains an **architecture proposal** (a later phase); it is documented here as the target design, not current behaviour. The learning period in §5.4, the backup/restore, and disk-pressure council step-down (which moves leadership off a pressured leader as §5.2 describes) all ship today.
>
> **Recovery fence (#424).** The recovery epoch is per-node fence state (`src/council/fence.rs`), not just a filter on RPC pairs. Gossip extensions carry each node's claimed epoch and leader hints order by (epoch, term). Each recovery also draws a random 128-bit recovery id (#694), stored with the epoch in the snapshot (`DesiredState::recovery_id`) and the fence record. The new epoch is `max(restored epoch, the recovering node's fence epoch, its fenced_by) + 1`, read before the old directory moves aside, and `relish council recover` refuses a data directory whose record says it's fenced unless `--force`. Raft envelopes carry `sender_recovery_id` and a `Fenced` reply carries the refusing node's own id; the gossip directory extension carries `recovery_epoch: Option<u64>` and `recovery_id: Option<RecoveryId>` side by side, folded into `NodeDirectory::recovery_claims: HashMap<NodeId, Option<RecoveryClaim>>`. A node that meets its own epoch with a different id moves to `FenceState::Split` and so does the other side, since neither can tell which recovery is real; `relish council status` and `relish wtf` report it as critical. A restarted voter doesn't serve Raft until gossip has joined and completed its first push-pull sync, then waits two anti-entropy intervals (20 s, overridable with `[cluster] recovery_probe_window_secs`) unless every peer its persisted membership names has advertised a claim sooner (`src/cluster/recovery_fence.rs`); a voter whose persisted membership names no other member has nobody to hear from and serves once gossip has synced (at once when it has no seeds), so a single-node cluster restarts as quickly as before. A failed fence write is retried every five seconds and reported in council status until it lands. Before it changes anything, `relish council recover` reads the persisted voter set (the data directory's or the backup's last membership) and asks each voter's `/v1/health` whether it is an unfenced council voter, refusing while one says yes or answers unreadably, and failing closed on an empty voter set; a node that learns of a newer epoch from a peer's `Fenced` reply fences itself permanently (persisted in `raft/recovery-fence.json`), sends and serves no Raft, refuses writes and names no leader. Over Raft, only a council member's RPC can move a node's fence; a node outside the council is only told a newer epoch (§3.3). Gossip never moves it directly (#2406): every node holds the gossip key, so one compromised worker could otherwise have fenced the whole council for good with one forged claim. A claim on gossip that would move the fence makes the node ask the voters of its own council (or, with no council, of the council it expects) with the `CouncilVoters` question, at most once every five seconds while the claim stands; a voter that serves a newer epoch, or the same epoch from another recovery, refuses the question with `Fenced` and its own claim, and that reply moves the fence. A probing, unclaimed or fenced node may ask that question, since it reaches no Raft state; a fenced one asks so that a split node still learns of the recovery that resolves it.

**Remembering the replaced council (lead's choice, pending the maintainer's confirmation).** A council `relish council recover` just formed has one voter, so on its own it has nobody to ask, and two survivors recovered independently onto one epoch would never see their split (#694): that is what `recovering_two_survivors_independently_fences_one` caught. So `recover_data_dir` saves the voters of the council it replaces, with their Raft addresses, as `raft/previous-council.json` beside the fence record, keeping any an earlier recovery saved. The file is node-local, never replicated, and survives restarts; `relish council re-enrol` drops it with the rest of the Raft state. The fence supervisor's round asks those voters too, over the same node-id-bound transport as current voters. Survivors of one old council therefore still find each other's recoveries, and a node outside that old council still can't fence anyone, because only an answer from a node this one dials by its own record counts. Alternatives considered: signing `(epoch, recovery_id)` with the recovering node's certificate and carrying the signature in the gossip extension (stronger, but a wire field and a verification path), or trusting gossip claims from any node named in the old membership without dialling it (simpler, but a forged datagram can name any old voter). A fresh data directory claims no epoch and adopts the first council that admits it, which is how the recovered council regrows and how `relish council re-enrol` brings a fenced voter back. The recovered council snapshots and purges its log after bootstrap so joiners receive the restored state by InstallSnapshot. Limit: old voters that reach only each other, never a newer-epoch node, can't learn of the recovery; operators re-enrol them first. The same holds for an old voter none of whose own council's voters has joined the recovered council: gossip alone no longer fences it, so it learns of the recovery only when one of them does, or when it dials a recovered node itself.

When the entire Raft council is lost simultaneously (all 3-7 members fail at once), the *proposed* pre-seeded recovery mechanism activates as follows.

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

All nodes remain in read-only mode. Apps continue running. No new deploys are accepted. Manual intervention via `relish council recover` bootstraps a new council from any surviving node. This is the true catastrophic case and manual intervention is appropriate.

### 5.6 Split-Brain Prevention

Split-brain can occur during a network partition where two groups each believe the council is lost.

**Raft quorum rule (normal operation):** A Raft leader must maintain contact with a majority of council members. If a partition splits a 5-member council into groups of 3 and 2, only the group of 3 can elect a leader (it has a majority). The group of 2 enters read-only mode.

**Recovery candidate rule (catastrophic recovery):** If the entire council is lost and a partition splits the remaining nodes, only the group containing the highest-priority recovery candidate can form a new council. The other group remains in read-only mode until the partition heals.

**Determinism:** There is never ambiguity about which group leads. The priority order was established before the failure. Two groups cannot independently decide they should both lead. This is the primary advantage of the pre-seeded approach over any voting-based catastrophic recovery.

**What ships today (operator-triggered recovery, #694).** Two operators can still recover two survivors independently. The recovery id is what keeps that from becoming two live councils: both land on the same epoch with different ids, and each fences itself as a split on first contact (gossip or a Raft refusal). Recovering a node whose fence record says it was fenced needs `--force` and lands above the fencing epoch, so the operator's second attempt resolves the split instead of repeating it. The live-voter probe in `relish council recover` refuses while any persisted voter still serves.

**Read-only mode behaviour:**

- Apps continue running and serving traffic.
- Health checks continue; containers are restarted on failure.
- No new deploys are accepted.
- No scheduling decisions are made.
- No cluster configuration changes are allowed.
- Nodes continue gossip and failure detection within their partition.
- When the partition heals, the non-leader group discovers the real leader via gossip and sends StateReports. The leader reconciles any drift.

### 5.7 Membership Changes (Raft)

The council reconciler on the leader makes one membership change at a time. Each tick it plans a single action (`plan_council_action`, `src/council/selection.rs`), and its change-in-flight gate holds the next until the current one has committed.

```
Add member flow:
1. The planner takes the best-ranked candidate C (select_council_candidates).
2. AddLearner(C): a learner membership entry commits, and the leader
   replicates the log to C. C receives entries but doesn't vote.
3. Once C's replicated index is within max_promotion_lag (64) entries of
   the leader's, the planner may promote it. Growth promotes two caught-up
   learners together (PromotePair), so the voter count stays odd.
4. Gossip carries the new council membership to every node.

Remove member flow:
1. The leader's HealthTracker, fed by gossip, reports voter M dead, or M is
   under sustained disk pressure. (A voter an operator retired is different:
   the reconciler takes it out of the voter set straight away.)
2. Add before remove: if a caught-up learner L exists, the planner swaps L
   in for M in one change (Replace). M stops counting for quorum when the
   change commits.
3. With no replacement, two dead voters leave together (RemoveVoterPair),
   never below min_council_size.
4. Gossip carries the new council membership to every node.
```

In the implementation, promotion and eviction go through openraft's joint consensus (`change_membership`): a joint configuration commits first, then the uniform one. The caller's future proposes the second step, so a caller that times out or a leader that fails over between the steps leaves a committed joint configuration behind. The council reconciler finishes it before planning anything else (`CouncilNode::finish_membership_change`), proposing the joint configuration's target voter set; otherwise its change-in-flight gate would hold every later membership change (#673).

Each reconciler operation runs under a five-second bound and costs a fixed number of commits: a guard no-op that makes earlier node-fault grants visible, then one membership entry (add or remove a learner) or two (the joint and uniform steps of a voter change). The catch-up in step 3 of the add flow is not part of any operation. Adding a learner returns once its membership entry commits, without openraft's blocking wait, which holds until the learner trails by at most 5,000 entries and so grows with the log gap (on a large council log, it includes installing a snapshot). Replication carries on between ticks, and the planner promotes the learner only once its replicated index is within `max_promotion_lag` (64) entries of the leader's. With a log store that takes 20 ms per entry, the guard no-op took 46 ms, adding a learner 90 ms and a promotion 147 ms (#779).

---

## 6. Configuration

There are **no `[mustard]` or `[raft]` TOML sections**. The SWIM and Raft protocol timings shown historically here (gossip interval, probe/suspicion timeouts, the 10 s anti-entropy push-pull interval, heartbeat and election timeouts, batch/snapshot sizes) are **compile-time constants**, not tunable config. What *is* configurable lives under `[cluster]` in `node.toml`: essentially the three ports, plus the recovery probe window (#694):

```toml
[cluster]
# Cluster identity; must match on every node.
name = "default"

# Seed members to contact at startup (gossip seed addresses).
join = ["10.0.1.5:9443"]

# Mustard SWIM gossip (UDP). Default: 9443.
gossip_port = 9443

# Raft council consensus RPCs (TCP + mTLS). Default: 9444.
raft_port = 9444

# Reporting-tree state reports (TCP + mTLS). Default: 9445.
reporting_port = 9445

# How many voters the council grows to: odd, 1 to 7. Unset keeps the
# default cap of 7. Read only by the node that bootstraps the cluster,
# which commits it to council state (RaftRequest::CouncilSize).
# `relish cluster create --bare-metal` and `relish machines claim --create`
# write 5 into node 1's config unless --council-size says otherwise.
# council_size = 5

# How long a restarted voter keeps listening for its peers' recovery
# epochs after gossip has synced, unless every peer advertises sooner.
# Unset means two anti-entropy intervals (20 s).
# recovery_probe_window_secs = 20
```

(The old `7946`/`7947` gossip/raft ports never shipped; the real defaults are `9443`/`9444`/`9445`. The node HTTP API is a separate port, `9117`.)

The council sizing constants below are compile-time defaults in `CouncilSelectionConfig` (`src/council/selection.rs`), shown here for reference — they are **not** a `[council]` TOML section. The one exception is the cluster-wide council size above: when council state holds one (`DesiredState::council_size`), the reconciler caps `max_council_size` at it every tick, and lowers `min_council_size` to match when it's smaller (`CouncilSelectionConfig::sized`). It lives in council state rather than in each node's config so that every leader, after any failover, grows the council to the same size:

```text
# Minimum council size: the planner's target never drops below it, and
# it never removes voters past it. A smaller cluster-wide council_size
# lowers it to match.
min_council_size = 3

# Maximum council size. The council holds the largest odd number of
# voters not above this nor the live node count (D12), so it grows
# 1, 3, 5, 7. There is no separate "target_size = 5".
max_council_size = 7

# Minimum time a node must be alive before it can join the council.
# The running reconciler overrides this to 0 (see §3.4), so it only
# orders candidates today.
min_node_age = "10m"

# Resource-eligibility thresholds (see the note below): these constants
# exist but never fire in production, because per-node resources are not
# gossiped, so every node passes the filter.
max_cpu_usage_fraction = 0.90
max_memory_usage_fraction = 0.85
```

The reporting-tree timings are likewise compile-time constants (not a TOML section):

```text
# reporting tree
report_interval = "5s"          # how often a worker sends a StateReport
max_events_per_report = 100
stale_report_timeout = "30s"
```

The learning period, by contrast, *is* tunable: it's the `[reconstruction]` section of `node.toml` (`ReconstructionSection` in `src/config/node.rs`), shown here with its defaults:

```toml
[reconstruction]
report_threshold_percent = 95
learning_period_timeout_secs = 15
large_cluster_timeout_secs = 30    # clusters > large_cluster_node_count
large_cluster_node_count = 5000
```

The recovery parameters below belong to the **proposed** pre-seeded catastrophic-recovery design (see §5.5); they document the target design, not shipped tunables:

```text
candidate_count = 10            # pre-seeded recovery candidates
catastrophic_timeout = "30s"    # no-leader-heartbeat trigger
priority_step_delay = "2s"      # per-priority-rank activation delay
candidate_refresh_interval = "5m"
```

---

## 7. Failure Modes

### 7.1 Leader Failure

**Detection:** Council followers detect missing heartbeats within `election_timeout_max` (2 seconds). All cluster nodes detect missing leader via Mustard gossip within `catastrophic_timeout` (30 seconds), but Raft election completes long before that.

**Recovery:** Raft elects a new leader from the remaining council. The new leader enters the learning period, collects StateReports, and resumes scheduling. Total downtime for new deploys: < 5 seconds for the election + learning period duration.

**Data plane impact:** None. Running apps are unaffected.

### 7.2 Council Member Departure (Non-Leader)

**Detection:** Gossip declares the member dead, and the leader's `HealthTracker` stops counting it as alive. Raft's own heartbeats don't decide it.

**Recovery:** Add before remove. If a caught-up learner exists, the planner swaps it in for the departed voter in one membership change (`Replace`). With none, the leader adds the best-ranked candidate as a learner, which catches up by Raft log replication, and the swap waits for it. Two dead voters with no replacement leave together (`RemoveVoterPair`), never below `min_council_size`. Workers assigned to the departed member's subtree in the reporting tree re-hash to remaining council members.

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
- The suspicion subprotocol prevents premature failure declarations from transient loss. A refutation goes ahead of every queued failure rumour at every hop (§3.2, "Refutation first"), so it reaches the cluster within the suspicion timeout even behind a correlated failure's backlog.
- A periodic push-pull exchange of membership tables between random node pairs (§3.2, "Anti-entropy push-pull") is the consistency backstop. It runs over the same UDP transport, not TCP.

At 10,000 nodes with 0.1% UDP packet loss, the expected convergence time increases by less than one additional protocol period (~500ms).

---

## 8. Security Considerations

### 8.1 Gossip Authentication

When the node has a cluster master key (`[security] master_key_path`, produced by `relish init` and provisioned onto every clustered node by the operator), every Mustard gossip message carries an HMAC-SHA256 tag computed over the message with the `hmac` field zeroed. Every node derives the same key from the master key with HKDF-SHA256 (`sesame::mtls::gossip_hmac::derive_gossip_key`), and the transport drops datagrams whose tag doesn't verify. A host without the master key can't forge or inject gossip messages.

A node started without a master key (single-node and pre-security configurations) gossips unauthenticated. Generated secure clusters always have one.

**Replay protection.** An HMAC proves the sender wrote a datagram once, not that it wrote it just now. Before #696 a captured, validly signed PING replayed from another host moved the sender's recorded address to the attacker's socket (so probes went there and the real node was declared dead), and an old directory extension rolled the sender's recovery epoch back. Every message now carries a `sequence` inside the HMAC: a process-wide counter seeded from wall-clock microseconds at start and incremented per message, so it usually rises across restarts ("Restarts" below covers a clock that went backwards). A receiver keeps the freshest `(incarnation, sequence)` it has accepted from each sender, compared incarnation first, and drops any message no fresher before applying anything, so an address change or directory update can only come from a newer message. A receiver also ignores a directory extension that names anyone but the sender, so directory fields are bound to that sequence too. A member's address moves only on a fresh datagram from the member, or on an `Alive` rumour about it at a strictly higher incarnation (#2475): only the member raises its own incarnation, so that's its own newer word about itself. A rumour at an equal incarnation could be a replay, and a `Suspect` or `Dead` carries the address its accuser holds, so neither moves the address. A relay forwards an indirect-probe ACK in its own name (`relayed_for`), so every message has exactly one stamping node.

**Sequence floors.** Reaping a member (60 s after it goes Dead or Left) removes it from the table, but not what it last sent. Its freshest accepted `(incarnation, sequence)` becomes a *floor*, kept for 24 hours (`SEQUENCE_FLOOR_RETENTION`), and any datagram from that sender no fresher than the floor is dropped. So the freshness check doesn't reset with the reap: an old datagram from a departed node stays old, and only the node itself can bring it back. The same node really coming back is heard straight away, whatever its clock says (see "Restarts" below); its first fresher datagram turns the floor back into an ordinary per-sender record. A node that never heard the departed member (one that joined, or restarted, after the reap) learns the floor in anti-entropy (§3.2): each side of a push-pull sends its floors after its entries, each with its remaining time, so passing a floor on never extends it. Members that are down but not yet reaped go too, and a node raises its own record for such a member to the freshest it hears, so by the reap the whole cluster agrees on the highest sequence any node accepted, including datagrams the member sent to someone else. A reaped member's floor also carries its tombstone, the incarnation it was reaped at (§5.1). A node keeps at most 1,024 floors (`MAX_SEQUENCE_FLOORS`), dropping the one closest to expiry first, ignores a floor about itself or about a sender it hears directly, and caps a received floor's lifetime at the retention.

**Datagrams in our own name.** A node drops any datagram whose `sender` is itself, before anything is applied. It keeps no sequence for itself to check such a datagram against, and nothing in one can be news: applying it could only move the node's own address or roll its own directory entry back. The only legitimate case is a seed list that names the node's own address, and a reply from itself tells it nothing.

**Restarts.** A node starts every run at incarnation 1, and its counter is seeded from the wall clock, so a node that boots with its clock behind (an appliance before NTP settles) sends datagrams that look older than its previous run's. A receiver that drops a datagram as stale answers the sender, at most once a second per sender, with `Stale { held_incarnation, held_sequence }`: the freshest datagram it holds from it. If that's fresher than anything the node has sent this run (its incarnation, then the highest sequence it has sent), it's from an earlier run, and the node refutes past `held_incarnation`, exactly as it refutes a `Suspect` or `Dead` about itself. Its next datagram carries a higher incarnation, so it's fresher whatever its sequence. A duplicate, a reordered datagram, or one of a live node's own datagrams sent again is at most the newest it sent, so the answer changes nothing. A restarted node is heard after one round trip to each peer, with no dependence on its clock and nothing persisted, and that holds after a wiped data directory too. A recorded datagram can't refute: the answer goes to whoever sent it, and it carries the old incarnation however often it's replayed. A node that has left ignores the answer.

Peers can hold different incarnations for the old run: one that heard it last at 3 and another at 5. The restarted node refutes past each answer in turn, so it can sit briefly between them. While it does (at 4, say), the peer it refuted for holds 4, and a recording from the old run at incarnation 5 is fresher there than the node's own datagrams: the replay window is open again for that peer until the node's datagram to the other peer draws its answer and the node refutes past 5. So the window is bounded by one `Stale` round trip per disagreeing peer, and only after a restart.

The sequence is a counter, not a timestamp check, so receivers need no clock agreement and no acceptance window: a timestamp would have to tolerate skew between hosts, and anything inside that tolerance could be replayed. The cost is that strict ordering drops a datagram that UDP delivers out of order behind a newer one from the same sender; gossip already tolerates loss, so that's a lost datagram, not an error (and at most one `Stale` answer a second, which changes nothing at the sender).

**What a holder of the gossip key can't do: overflow an incarnation (#2395).** Every member holds the gossip key, so the HMAC stops outsiders, not a buggy or compromised member. Incarnations start at 1 and rise by one per refutation or restart, so a node drops any incarnation from the wire above `MAX_INCARNATION` (`u64::MAX / 2`): a datagram's own, a piggybacked or synced update's, a `Stale` answer's `held_incarnation`, and a floor's. Each refusal is counted and logged at most once every 10 s. Without the ceiling, one `Dead` claim at `u64::MAX` overflowed `refute`: a debug build panicked the gossip task, and a release build wrapped the node's incarnation to 0, so its refutation lost to the claim on every peer and the node stayed dead until reaped. `refute` also saturates at the ceiling, so nothing wraps. What's left is milder: a claim at exactly the ceiling burns the node's headroom, since it can't out-rank it, and the node stays `Dead` to its peers. That needs the same compromised member, and it's recovered by restarting the node under a new name; we don't add more machinery for it.

**TODO (not yet implemented):** gossip key rotation. The key is fixed for the life of the master key; there's no rotation command and no dual-key acceptance window.

### 8.2 Encrypted Recovery Candidate List

The recovery candidate list is encrypted with the council's shared age public key. This prevents an attacker who compromises a single worker node from learning which nodes are recovery candidates (and therefore which nodes to target for maximum disruption).

Each recovery candidate is notified of its priority individually via the reporting tree's mTLS channel. The candidate can verify that the encrypted gossip blob matches by decrypting it and checking the plaintext hash.

The council's shared age keypair is generated during `relish init` and stored encrypted in the Raft log (encrypted with the Node CA's key). It is available only to council members.

### 8.3 Raft Log Encryption

Raft log entries are encrypted at rest with AES-256-GCM under a key derived (HKDF) from the cluster master key, which is stored on the node. This protects the log's entries from someone who copies the disk or the log file; it doesn't protect against root on a running node, which can read the master key. The Raft snapshot (`snapshot.redb`) is not encrypted. See [security-sesame.md](security-sesame.md) §5.6.

In transit, Raft messages are protected by mTLS (all inter-council communication uses TLS 1.3 with mutual certificate authentication via Sesame). Every node holds a node certificate, so the Raft listener also serves only RPCs a council voter sends under its own certificate (§3.3, "Council membership check").

### 8.4 Reporting Tree Authentication

All reporting connections use mTLS via Sesame's Node CA. In the shipped flat star (§3.5) every worker reports straight to the leader, which verifies that the worker's node ID (from the certificate) matches the `node_id` claimed in every message that carries node state: `Report`, `MetricsRollup`, `CapabilityReport` and `NodeReadinessReport`. This prevents a compromised worker from reporting state on behalf of another node. There is no message that carries several nodes' reports: `AggregatedReport` was removed because it bypassed exactly this check (#696).

---

## 9. Performance

### 9.1 Gossip Scalability

| Metric | Value | Explanation |
|--------|-------|-------------|
| Convergence time | O(log N) protocol periods | Each protocol period, each node probes one random peer and piggybacks updates. Information spreads like an epidemic. |
| Per-node network overhead | O(1) | Each node sends/receives a constant number of messages per protocol period regardless of cluster size. |
| Bandwidth per node | ~3 KB/s at 10,000 nodes | 1 PING + 1 ACK per protocol period (500ms), ~1400 bytes each. Plus occasional PING-REQ. |
| CPU per received datagram | Independent of N when nothing changed | `run` republishes the membership after every datagram, but only when the table's version or the council roles moved since the last publish (#2439). A change still costs O(N log N): a snapshot, a digest and a sort, on the active and the roster watch. Before the version check every datagram paid that, 813 µs at 1,000 members and 12.8 ms at 10,000 with five labels each, against about 210 ns at either size after it (`gossip_publish_after_datagram` in `benches/gossip.rs`). |
| Membership table memory | ~256 bytes per node | NodeMembership struct. At 10,000 nodes: ~2.5 MB total per node. |

### 9.2 Leader Election Performance

| Metric | Target | Mechanism |
|--------|--------|-----------|
| Election time | < 5 seconds | Randomized timeout (1-2s) + one or two voting rounds. (Pre-vote, which would prevent spurious elections, is a TODO; see §5.2.) |
| Leader announcement propagation | < 2 seconds | Gossip convergence at O(log 10000) ~ 13 periods * 500ms ~ 6.5s theoretical worst case, but leader aggressively pushes to all council members immediately and they amplify. Practical: < 2 seconds. |
| Steady-state heartbeat overhead | ~50 KB/s total (council) | 150ms interval * 5 members * ~100 bytes per heartbeat. |

### 9.3 State Reconstruction Performance

| Cluster size | Nodes | Expected time | Bottleneck |
|-------------|-------|---------------|------------|
| Small | 10-100 | < 2 seconds | Network RTT for StateReports. |
| Medium | 100-1,000 | < 5 seconds | Council members aggregating reports. |
| Large | 1,000-10,000 | < 15 seconds | Reporting tree fan-in at council members. Each council member receives ~2,000 reports concurrently. |

> **Planned topology.** The fan-in figures in this row and the table below assume the two-level tree of §3.5, which is not the shipped topology. Today the leader is the single parent, so it receives every StateReport directly (see the flat-star note in §3.5); these per-council-member numbers apply only once the tree is wired in.

In the planned two-level tree at 10,000 nodes with a council of 7, each council member would receive ~1,400 StateReports. Each StateReport is ~1-10 KB (depends on running apps). Today, on the flat star, the leader absorbs all ~10,000 reports itself.

### 9.4 Reporting Tree Overhead

> **Planned topology (two-level tree, §3.5).** In the shipped flat star the "per-council-member" row does not apply — the leader receives every report.

| Metric | Value |
|--------|-------|
| Per-worker bandwidth (sending) | ~2 KB every 5 seconds = ~400 B/s |
| Per-council-member bandwidth (receiving, ~1,400 workers) *(planned tree)* | ~560 KB/s |
| Leader aggregation bandwidth *(planned tree)* | council members forwarding summaries |
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
- Pre-vote extension to prevent disruptive elections from partitioned nodes. **TODO (not yet implemented):** `openraft` 0.9 doesn't provide it.

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
- [Lifeguard: Local Health Awareness for More Accurate Failure Detection (Hashimoto, 2017)](https://arxiv.org/abs/1707.00788) -- Improvements to SWIM used in Consul's memberlist. Mustard does not implement Lifeguard: it has SWIM's own suspicion subprotocol with a fixed suspicion timeout, and no local health awareness or dynamic timeouts.

---

## 12. Libraries & Dependencies

### 12.1 Raft Implementation

| Crate | Notes |
|-------|-------|
| [`openraft`](https://crates.io/crates/openraft) | **Primary candidate.** Pure Rust, async-first (tokio-native), well-maintained, supports dynamic membership changes and snapshots (but not pre-vote, as of 0.9). Used by Databend and other production systems. |
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
| [`postcard`](https://crates.io/crates/postcard) | Binary serialisation for gossip datagrams and reporting frames: compact varints over serde. Replaced the unmaintained `bincode` in 0.2.2 (RUSTSEC-2025-0141). |
| [`serde`](https://crates.io/crates/serde) | Serialisation framework. All data structures derive Serialize/Deserialize. |
| [`ring`](https://crates.io/crates/ring) | HMAC-SHA256 for gossip message authentication. SHA-256 for spec hashing. |
| [`age`](https://crates.io/crates/age) | Encryption for the recovery candidate list and Raft log at-rest encryption. |

---

## 13. Open Questions

1. **Gossip library vs. custom implementation.** Should we use `foca` or write our own SWIM implementation? The fixed-size message constraint and HMAC authentication are non-standard requirements. A custom implementation is ~1000 lines and fully under our control, but a library gives us battle-tested edge case handling. Decision needed after evaluating `foca`'s extensibility.

2. **Council size auto-tuning.** The whitepaper specifies a configurable council size of 3-7. Should the leader automatically adjust the council size based on cluster size (e.g., 3 at <50 nodes, 5 at 50-500 nodes, 7 at >500 nodes)? Or is a static default of 5 sufficient? Auto-tuning adds complexity but improves fault tolerance at scale. *Partly settled (0.3.0):* no auto-tuning. The cap stays 7 by default, and a cluster can be created with a fixed odd size from 1 to 7 (`[cluster] council_size`, committed to council state at bootstrap). Appliance clusters default to 5, for a ten-node lab of 2 GB machines. There's no command yet to change it on a running cluster.

3. **Recovery candidate notification timing.** Recovery candidates are notified via the reporting tree's mTLS channel. If the reporting tree itself is disrupted (e.g., the candidate's council member parent dies), the notification may not arrive. Should candidates also be able to discover their priority from the encrypted gossip blob alone (requiring them to have a decryption key)? This changes the security model.

4. **Raft log size management.** App specs, secrets, and scheduling decisions accumulate in the Raft log. Snapshot compaction bounds the log size, but the snapshot itself can grow. What is the practical upper bound on snapshot size at 10,000 nodes with 500 apps per node? Do we need an eviction policy for old scheduling decisions?

5. **Gossip protocol period scaling.** The SWIM paper suggests a fixed protocol period. At 10,000 nodes, a 500ms period means each node is probed every ~5000 seconds (83 minutes) on average. Should the protocol period decrease as cluster size grows (faster failure detection at the cost of more bandwidth)? Or is the indirect probe mechanism sufficient?

6. **Reporting tree depth.** The current design is two-level: workers report to council members, council members report to the leader. At 10,000 nodes with 5 council members, each council member handles ~2,000 workers. Should the tree support additional intermediate levels for very large clusters (e.g., "zone leaders" that aggregate before council members)?

7. **Raft read consistency.** Council members serve read-only API requests from local Raft state. This is eventually consistent (a follower's state may lag the leader by a few milliseconds). Is this acceptable for all read operations, or do some reads (e.g., "is this app deployed?") require linearizable reads routed to the leader?

8. **Clock synchronisation.** The learning period timeout and catastrophic recovery timeout depend on wall-clock time. If clocks are significantly skewed between nodes, timeouts may fire at different times. Should we require NTP or use Raft-term-based logical time for these timeouts?

# Smoker -- Fault Injection Engine

Design document for the Smoker subsystem of Reliaburger. Smoker provides built-in chaos engineering capabilities using eBPF for network faults and cgroups for resource faults. It adds no new binaries, processes, or network components -- it activates latent capabilities in the eBPF programs and cgroup controls that Bun and Onion already manage.

Sourced from whitepaper section 18 (Fault Injection).

---

## 1. Overview

Every production system eventually fails. Networks partition, disks fill, processes exhaust memory. Chaos engineering -- deliberately injecting faults to verify resilience -- has proven its value at Netflix, Google, Amazon, and thousands of other organisations. However, existing tools (Chaos Monkey, Litmus, Gremlin, Chaos Mesh) are separate systems requiring installation, configuration, CRDs, RBAC policies, sidecars, or privileged DaemonSets. The barrier to entry is high enough that most teams never adopt them.

Smoker takes a different approach. Because Onion's eBPF layer already intercepts every DNS resolution and every `connect()` call at the socket level, and because Bun already manages cgroups for every container, fault injection is a natural extension of existing infrastructure. Smoker writes fault rules into dedicated BPF maps alongside Onion's service maps, and the eBPF programs check these maps on every interception.

Key design properties:

- **Zero overhead when inactive.** When no faults are active, all fault BPF maps are empty. The eBPF programs perform a single hash lookup that returns "no fault" and proceed on the normal path. No iptables rules, no tc rules, no sidecars.
- **Kernel-level fidelity.** Network faults happen at the socket layer in eBPF. Applications see exactly the same error codes and behaviours they would see during a real failure -- `ECONNREFUSED`, `ENETUNREACH`, `EAI_NONAME`. No simulation layer to leak abstractions.
- **Automatic safety rails.** Smoker prevents faults that would make the cluster unrecoverable: quorum protection, replica minimums, leader guarding, and mandatory expiry.
- **No persistence.** Fault rules exist only in BPF maps (kernel memory) and Bun's in-process state. They are never written to disk. A Bun restart clears all faults on that node.

Fault injection is exposed through the `relish fault` CLI subcommand. Every fault is a single command:

```bash
# Network faults
relish fault delay redis 200ms
relish fault drop api 10%
relish fault partition web --from payment-service
relish fault dns redis nxdomain
relish fault bandwidth api 1mbps

# Resource faults
relish fault cpu inference 50%
relish fault memory redis 90%
relish fault disk-io web 10mbps

# Process faults
relish fault kill web-3
relish fault pause payment-service

# Node-level faults
relish fault node-drain node-05
relish fault node-kill node-05

# Management
relish fault list
relish fault clear
relish fault clear redis
```

Every fault accepts common options:

```bash
relish fault delay redis 200ms \
  --duration 5m \
  --instance redis-1 \
  --node node-03 \
  --jitter 50ms
```

If `--duration` is omitted, the fault defaults to 10 minutes and prints a warning. Faults never persist across Bun restarts.

---

## 2. Dependencies

Smoker is not a standalone subsystem. It extends three existing components:

### 2.1 Onion (eBPF Service Mesh)

Smoker's network faults are implemented by extending the same eBPF programs that Onion uses for service discovery. The DNS interception hook (attached to `sock_ops` for UDP+TCP port 53) and the connect interception hook (attached to `connect4` / `sock_ops`) already intercept every relevant system call. Smoker adds fault map lookups to these existing programs -- it does not load new eBPF programs.

The existing Onion maps (`dns_map`, `backend_map`, `firewall_map`) are unmodified. Smoker adds four new BPF maps (`fault_dns_map`, `fault_connect_map`, `fault_bw_map`, `fault_state_map`) that the eBPF programs check before or after the normal service map lookup.

### 2.2 Bun (Node Agent)

Bun is the userspace agent that manages containers on each node. Smoker uses Bun for:

- **BPF map writes.** Bun writes fault rules into the kernel BPF maps via the `bpf()` syscall (through libbpf-rs or aya).
- **Cgroup control.** Resource faults (CPU stress, memory pressure, disk I/O throttle) use the same cgroup hierarchy that Bun already manages for container isolation.
- **Process signals.** Process faults (SIGKILL, SIGSTOP, SIGCONT) are sent by Bun to the container's PID namespace via `kill(2)`.
- **Fault lifecycle.** Bun tracks active faults, enforces expiry timers, and cleans up fault state on expiry, crash recovery, or explicit `relish fault clear`.

### 2.3 API (Cluster Leader)

All fault injection requests flow through the cluster API on the leader node:

- **Permission checks.** The leader validates that the requesting user has the `admin` role or an explicit `fault-injection` Permission grant.
- **Safety rail enforcement.** The leader evaluates blast radius protection rules (quorum, replica, leader guards) before approving a fault.
- **Distribution.** The leader instructs target node(s) via the reporting tree to activate the fault.
- **Audit logging.** Every fault injection is logged as a cluster event with full attribution (who, what, when, source IP).

---

## 3. Architecture

### 3.1 eBPF Fault Maps

Smoker adds four BPF maps to the kernel, managed alongside Onion's existing service maps. All maps are BPF_MAP_TYPE_HASH with per-CPU variants where needed for performance.

```
+---------------------------------------------------------------+
|  Kernel (eBPF)                                                |
|                                                               |
|  DNS interception hook (sock_ops, UDP + TCP port 53):         |
|    1. Is this a .internal query?                              |
|    2. Look up fault_dns_map[service_name]                     |
|       -> empty: normal resolution from dns_map                |
|       -> nxdomain: return NXDOMAIN response                   |
|       -> delay: hold response for N nanoseconds               |
|                                                               |
|  Connect interception hook (sock_ops / connect4):             |
|    1. Is this a virtual IP?                                   |
|    2. Look up fault_connect_map[virtual_ip:port]              |
|       -> empty: normal backend selection from backend_map     |
|       -> drop(10%): generate random, if < 10% return          |
|          -ECONNREFUSED                                        |
|       -> delay(200ms): store timestamp, defer via timer       |
|       -> partition(from=X): check source cgroup, if match     |
|          return -ENETUNREACH                                  |
|    3. Look up fault_bw_map[virtual_ip:port]                   |
|       -> empty: no throttle                                   |
|       -> 1mbps: attach token bucket rate limiter to socket    |
|                                                               |
|  BPF maps (written by Bun userspace agent):                   |
|    dns_map/backend_map -- normal Onion service discovery      |
|    fault_dns_map      -- per-service DNS fault rules          |
|    fault_connect_map  -- per-service connect fault rules      |
|    fault_bw_map       -- per-service bandwidth limits         |
|    fault_state_map    -- PRNG state, counters, timestamps     |
|                                                               |
+---------------------------------------------------------------+
```

**fault_dns_map** -- Keyed by service name hash (u32). Values contain the fault type (NXDOMAIN or DELAY), delay duration in nanoseconds, probability (0-100), and expiry timestamp.

**fault_connect_map** -- Keyed by `{virtual_ip: u32, port: u16, source_cgroup: u64}`. The source_cgroup field is zero for faults that apply to all callers, or set to a specific cgroup ID for partition faults. Values contain fault type (DROP, DELAY, PARTITION), parameters, probability, and expiry timestamp.

**fault_bw_map** -- Keyed by `{virtual_ip: u32, port: u16}`. Values contain the rate limit in bytes per second, token bucket state (tokens remaining, last refill timestamp).

**fault_state_map** -- Keyed by CPU ID (u32). Values contain per-CPU PRNG state (u64 xorshift seed), counters for faults injected, and scratch space for delay timers.

### 3.2 Resource Fault Manager (Bun Userspace)

Resource faults operate entirely in userspace through Bun's existing cgroup and process management capabilities:

```
+---------------------------------------------------------------+
|  Userspace (Bun agent)                                        |
|                                                               |
|  relish fault delay redis 200ms                               |
|    -> API call to leader                                      |
|    -> leader validates (permissions, safety rails)            |
|    -> leader instructs target node(s) via reporting tree      |
|    -> Bun on target node writes to fault_connect_map:         |
|        key:   {virtual_ip: 127.128.0.3, port: 6379}          |
|        value: {type: DELAY, delay_ns: 200000000,             |
|                probability: 100, expires: <timestamp>}        |
|    -> eBPF program picks up new rule on next connect()        |
|                                                               |
|  Resource fault manager:                                      |
|    -> CPU stress: spawns burn loop in target app's cgroup     |
|    -> Memory pressure: allocates + mlocks pages in cgroup     |
|    -> Disk I/O throttle: writes blkio cgroup limits           |
|    -> Process faults: sends signals to container PID          |
|                                                               |
+---------------------------------------------------------------+
```

**CPU stress.** Bun spawns a lightweight burn process (a tight arithmetic loop compiled into the Bun binary itself) inside the target container's CPU cgroup. The burn process consumes the specified percentage of the cgroup's CPU quota. Because it runs inside the same cgroup, it competes with the application for CPU time exactly as a real noisy-neighbour workload would. The application sees increased scheduling latency, higher tail latencies, and reduced throughput.

**Memory pressure.** Bun spawns a process inside the target container's memory cgroup that allocates and `mlock()`s pages, pushing the container toward its memory limit. The amount is calculated from the configured percentage and the container's memory limit. At 90%, the application has only 10% headroom. This triggers the same kernel memory pressure signals (PSI, memory.high events) as real contention.

**Disk I/O throttle.** Bun writes to the `blkio` cgroup controller for the target container, setting read/write bandwidth limits using `blkio.throttle.read_bps_device` and `blkio.throttle.write_bps_device`. This is the kernel's native I/O throttling mechanism.

**Process faults.** Bun sends signals directly to the container's main process (PID 1 inside the container namespace). `kill` sends SIGKILL (immediate termination). `pause` sends SIGSTOP (freeze the process). `--resume` sends SIGCONT (unfreeze).

### 3.3 Request Flow

```
relish fault delay redis 200ms --duration 5m
  |
  v
Relish CLI -> Unix socket or cluster API
  |
  v
Cluster leader (permission check, safety rail evaluation)
  |
  v
Leader identifies target nodes (nodes running redis instances)
  |
  v
Leader sends FaultActivate message via reporting tree
  |
  v
Bun on target node(s):
  1. Validates fault parameters
  2. Calculates expiry timestamp = now + duration
  3. Writes BpfFaultEntry to fault_connect_map via bpf() syscall
  4. Registers expiry timer in local fault registry
  5. Acknowledges activation to leader
  |
  v
eBPF program on next connect() to redis VIP:
  1. Normal Onion VIP lookup -> match
  2. fault_connect_map lookup -> DELAY 200ms found
  3. Set TCP_BPF_DELACK timer to 200ms
  4. Connection completes 200ms later from app perspective
```

---

## 4. Data Structures

### 4.1 Rust Userspace Structures

```rust
/// Unique identifier for an active fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FaultId(pub u64);

/// The type of fault being injected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FaultType {
    /// Add latency to connections.
    /// delay_ns: delay in nanoseconds, jitter_ns: +/- random range.
    Delay {
        delay_ns: u64,
        jitter_ns: u64,
    },

    /// Fail a percentage of connections with ECONNREFUSED.
    /// probability: 0-100 (percentage of connections to drop).
    Drop {
        probability: u8,
    },

    /// Return NXDOMAIN for DNS resolution of target service.
    DnsNxdomain,

    /// Block traffic from a specific source service to the target.
    /// source_cgroup_id: cgroup ID of the calling app.
    Partition {
        source_cgroup_id: u64,
    },

    /// Throttle bandwidth to target service.
    /// bytes_per_sec: maximum throughput in bytes/sec.
    Bandwidth {
        bytes_per_sec: u64,
    },

    /// Consume a percentage of the target's CPU quota.
    /// percentage: 0-100 (how much of the cgroup CPU to consume).
    /// cores: optional limit to specific number of cores.
    CpuStress {
        percentage: u8,
        cores: Option<u32>,
    },

    /// Push memory usage toward the target's memory limit.
    /// If `oom` is true, trigger an immediate OOM kill.
    /// Otherwise `percentage` specifies how full to push memory (0-100).
    MemoryPressure {
        percentage: u8,
        oom: bool,
    },

    /// Throttle disk I/O via blkio cgroup.
    /// bytes_per_sec: read+write bandwidth limit.
    /// write_only: if true, only throttle writes.
    DiskIoThrottle {
        bytes_per_sec: u64,
        write_only: bool,
    },

    /// Send SIGKILL to target instances.
    /// count: how many instances to kill (0 = all matching).
    Kill {
        count: u32,
    },

    /// Send SIGSTOP to freeze target instances.
    Pause,

    /// Send SIGCONT to unfreeze target instances.
    Resume,

    /// Simulate graceful node departure.
    NodeDrain,

    /// Simulate abrupt node failure.
    /// kill_containers: if true, also stop all containers on node.
    NodeKill {
        kill_containers: bool,
    },
}

/// A single fault rule, as stored in Bun's in-process fault registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultRule {
    /// Unique fault identifier (monotonically increasing per node).
    pub id: FaultId,

    /// What kind of fault to inject.
    pub fault_type: FaultType,

    /// Target service name (e.g. "redis", "api", "payment-service").
    pub target_service: String,

    /// Optional: target a specific instance by name (e.g. "redis-1").
    pub target_instance: Option<String>,

    /// Optional: restrict fault to a specific node.
    pub target_node: Option<String>,

    /// When this fault was activated (monotonic clock, nanoseconds).
    pub activated_at_ns: u64,

    /// When this fault expires (monotonic clock, nanoseconds).
    /// Computed as activated_at_ns + duration.
    pub expires_at_ns: u64,

    /// Duration in nanoseconds (for display and audit).
    pub duration_ns: u64,

    /// Who injected this fault.
    pub injected_by: String,

    /// Source IP of the fault injection request.
    pub source_ip: String,

    /// Human-readable reason (from --reason flag).
    pub reason: Option<String>,
}

/// A scripted multi-step chaos scenario, parsed from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptedScenario {
    /// Human-readable scenario name.
    pub name: String,

    /// Ordered list of fault steps.
    pub steps: Vec<ScenarioStep>,
}

/// One step in a scripted scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioStep {
    /// Human-readable description of what this step tests.
    pub description: String,

    /// Fault type string as used in the CLI (e.g. "delay", "drop", "memory").
    pub fault: String,

    /// Target service name.
    pub target: String,

    /// Fault value (e.g. "200ms", "10%", "90%", "oom", "nxdomain").
    pub value: String,

    /// Optional jitter (e.g. "50ms").
    pub jitter: Option<String>,

    /// How long this fault should remain active.
    pub duration: Option<String>,

    /// Delay before activating this step, relative to scenario start.
    /// If None, step activates immediately (or immediately after previous step).
    pub start_after: Option<String>,
}

/// Safety check result, evaluated by the leader before approving a fault.
#[derive(Debug, Clone)]
pub struct SafetyCheck {
    /// Whether the fault passed all safety checks.
    pub approved: bool,

    /// If not approved, which safety rail was violated.
    pub violation: Option<SafetyViolation>,

    /// Current cluster state relevant to the check.
    pub context: SafetyContext,
}

#[derive(Debug, Clone)]
pub enum SafetyViolation {
    /// Fault would break Raft quorum.
    /// current_affected: how many council nodes already have faults.
    /// max_allowed: (council_size - 1) / 2.
    QuorumRisk {
        current_affected: u32,
        max_allowed: u32,
    },

    /// Fault would kill all replicas of a service.
    /// surviving: how many replicas would remain.
    ReplicaMinimum {
        service: String,
        current_replicas: u32,
        surviving: u32,
    },

    /// Fault targets the cluster leader without --include-leader.
    LeaderTargeted,

    /// Fault affects more than 50% of nodes without --override-safety.
    NodePercentageExceeded {
        affected_nodes: u32,
        total_nodes: u32,
    },
}

#[derive(Debug, Clone)]
pub struct SafetyContext {
    pub council_size: u32,
    pub council_nodes_with_active_faults: u32,
    pub leader_node_id: String,
    pub total_nodes: u32,
    pub nodes_with_active_faults: u32,
    pub target_service_replicas: u32,
    pub target_service_faulted_replicas: u32,
}
```

### 4.2 BPF Map Key/Value Layouts

These structures are shared between the eBPF C programs and the Rust userspace via `#[repr(C)]` Rust structs and corresponding C struct definitions.

```rust
/// Key for fault_dns_map.
/// BPF map type: BPF_MAP_TYPE_HASH, max_entries: 1024.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BpfDnsFaultKey {
    /// FNV-1a hash of the service name (e.g. hash("redis")).
    pub service_name_hash: u32,
}

/// Value for fault_dns_map.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BpfDnsFaultValue {
    /// Fault action: 0 = none, 1 = NXDOMAIN, 2 = DELAY.
    pub action: u8,
    pub _pad: [u8; 3],
    /// Delay in nanoseconds (only used when action = DELAY).
    pub delay_ns: u64,
    /// Probability 0-100 (100 = always apply).
    pub probability: u8,
    pub _pad2: [u8; 3],
    /// Expiry timestamp (CLOCK_MONOTONIC, nanoseconds). 0 = no expiry.
    pub expires_ns: u64,
}

/// Key for fault_connect_map.
/// BPF map type: BPF_MAP_TYPE_HASH, max_entries: 4096.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BpfConnectFaultKey {
    /// Virtual IP of the target service (Onion-assigned, e.g. 127.128.0.3).
    pub virtual_ip: u32,
    /// Target port (network byte order).
    pub port: u16,
    pub _pad: u16,
    /// Source cgroup ID. 0 = match all callers.
    /// Non-zero = only match connections from this cgroup (partition faults).
    pub source_cgroup_id: u64,
}

/// Value for fault_connect_map.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BpfConnectFaultValue {
    /// Fault action: 0 = none, 1 = DROP, 2 = DELAY, 3 = PARTITION.
    pub action: u8,
    pub _pad: [u8; 3],
    /// Delay in nanoseconds (action = DELAY).
    pub delay_ns: u64,
    /// Jitter in nanoseconds (action = DELAY). Actual delay = delay_ns +/- rand(jitter_ns).
    pub jitter_ns: u64,
    /// Drop probability 0-100 (action = DROP).
    pub probability: u8,
    pub _pad2: [u8; 3],
    /// Expiry timestamp (CLOCK_MONOTONIC, nanoseconds). 0 = no expiry.
    pub expires_ns: u64,
}

/// Key for fault_bw_map.
/// BPF map type: BPF_MAP_TYPE_HASH, max_entries: 1024.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BpfBandwidthFaultKey {
    /// Virtual IP of the target service.
    pub virtual_ip: u32,
    /// Target port (network byte order).
    pub port: u16,
    pub _pad: u16,
}

/// Value for fault_bw_map.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BpfBandwidthFaultValue {
    /// Rate limit in bytes per second.
    pub rate_bytes_per_sec: u64,
    /// Token bucket: current token count (bytes).
    pub tokens: u64,
    /// Token bucket: last refill timestamp (CLOCK_MONOTONIC, nanoseconds).
    pub last_refill_ns: u64,
    /// Expiry timestamp (CLOCK_MONOTONIC, nanoseconds). 0 = no expiry.
    pub expires_ns: u64,
}

/// Key for fault_state_map.
/// BPF map type: BPF_MAP_TYPE_PERCPU_ARRAY, max_entries: 1 (indexed by 0).
/// Each CPU gets its own copy automatically.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BpfFaultStateKey {
    /// Always 0 (single entry per CPU, PERCPU_ARRAY handles per-CPU storage).
    pub index: u32,
}

/// Value for fault_state_map (one per CPU).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BpfFaultStateValue {
    /// PRNG seed for this CPU (xorshift64).
    pub prng_state: u64,
    /// Counter: total faults injected on this CPU.
    pub faults_injected: u64,
    /// Counter: total fault map lookups on this CPU.
    pub lookups: u64,
    /// Scratch: timestamp for delay timer bookkeeping.
    pub scratch_ts: u64,
}
```

---

## 5. Operations

### 5.1 Network Faults

Network faults are implemented in eBPF. Each fault type extends the existing Onion interception hooks with a fault map lookup.

#### 5.1.1 Delay (via sock_ops TCP_BPF_DELACK)

The eBPF program attached to the `connect4` hook cannot sleep -- eBPF programs must be non-blocking. Delay is therefore implemented using a `sock_ops` program that intercepts the TCP state machine. When a SYN-ACK is received (connection established), the program checks the fault map. If a delay is configured, it sets the socket's `TCP_BPF_DELACK` timer to defer the application-visible connection completion by the specified duration.

From the application's perspective, `connect()` takes 200ms longer than usual. For HTTP-level delays on established connections, the `sk_msg` program can hold data in a BPF ring buffer before releasing it to the socket.

```c
// eBPF pseudocode: delay fault in sock_ops program
SEC("sockops")
int smoker_delay_sockops(struct bpf_sock_ops *skops) {
    // Only act on SYN-ACK received (active connection established)
    if (skops->op != BPF_SOCK_OPS_ACTIVE_ESTABLISHED_CB)
        return SK_PASS;

    // Build lookup key from the destination IP:port
    struct bpf_connect_fault_key key = {
        .virtual_ip = skops->remote_ip4,
        .port       = bpf_ntohs(skops->remote_port),
        .source_cgroup_id = 0,  // match-all first
    };

    struct bpf_connect_fault_value *val =
        bpf_map_lookup_elem(&fault_connect_map, &key);
    if (!val)
        return SK_PASS;

    // Check expiry
    __u64 now = bpf_ktime_get_ns();
    if (val->expires_ns != 0 && now > val->expires_ns) {
        // Fault expired -- delete from map asynchronously (or let
        // userspace cleanup handle it). Pass through.
        return SK_PASS;
    }

    if (val->action != FAULT_ACTION_DELAY)
        return SK_PASS;

    // Compute actual delay with jitter
    __u64 delay = val->delay_ns;
    if (val->jitter_ns > 0) {
        // Read per-CPU PRNG state
        __u32 state_key = 0;
        struct bpf_fault_state_value *state =
            bpf_map_lookup_elem(&fault_state_map, &state_key);
        if (state) {
            // xorshift64 PRNG
            __u64 x = state->prng_state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            state->prng_state = x;

            // jitter range: [-jitter_ns, +jitter_ns]
            __s64 jitter = (x % (2 * val->jitter_ns + 1)) - val->jitter_ns;
            delay = (__u64)((__s64)delay + jitter);
            if ((__s64)delay < 0) delay = 0;

            state->faults_injected++;
        }
    }

    // Set TCP_BPF_DELACK to defer connection completion
    // The kernel will delay the ACK by `delay` nanoseconds,
    // making the connect() call appear to take longer.
    __u64 delay_us = delay / 1000;  // convert ns to us
    if (delay_us > 0) {
        bpf_sock_ops_cb_flags_set(skops,
            BPF_SOCK_OPS_ALL_CB_FLAGS);
        // Store delay in socket local storage for the timer
        bpf_setsockopt(skops, SOL_TCP, TCP_BPF_DELACK_MAX,
                        &delay_us, sizeof(delay_us));
    }

    return SK_PASS;
}
```

#### 5.1.2 Drop (via PRNG in connect hook)

On each `connect()` interception, the eBPF program reads a per-CPU PRNG state from `fault_state_map`, generates a random value, and compares it to the configured drop percentage. If the random value falls within the drop range, the program returns `-ECONNREFUSED` directly from the hook. No packet is ever sent.

```c
// eBPF pseudocode: drop fault in connect4 program
SEC("cgroup/connect4")
int smoker_drop_connect4(struct bpf_sock_addr *ctx) {
    struct bpf_connect_fault_key key = {
        .virtual_ip       = ctx->user_ip4,
        .port             = bpf_ntohs(ctx->user_port),
        .source_cgroup_id = 0,
    };

    struct bpf_connect_fault_value *val =
        bpf_map_lookup_elem(&fault_connect_map, &key);
    if (!val)
        return CGROUP_SOCK_ADDR_ALLOW;  // no fault, normal path

    // Check expiry
    __u64 now = bpf_ktime_get_ns();
    if (val->expires_ns != 0 && now > val->expires_ns)
        return CGROUP_SOCK_ADDR_ALLOW;

    if (val->action != FAULT_ACTION_DROP)
        return CGROUP_SOCK_ADDR_ALLOW;

    // Generate random number using per-CPU PRNG
    __u32 state_key = 0;
    struct bpf_fault_state_value *state =
        bpf_map_lookup_elem(&fault_state_map, &state_key);
    if (!state)
        return CGROUP_SOCK_ADDR_ALLOW;

    __u64 x = state->prng_state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    state->prng_state = x;
    state->lookups++;

    // Check if this connection should be dropped
    __u8 roll = x % 100;  // 0-99
    if (roll < val->probability) {
        state->faults_injected++;
        // Return ECONNREFUSED -- application sees a refused connection,
        // indistinguishable from a real backend refusing the connection.
        return CGROUP_SOCK_ADDR_REJECT;  // maps to -ECONNREFUSED
    }

    return CGROUP_SOCK_ADDR_ALLOW;  // this connection passes through
}
```

#### 5.1.3 DNS NXDOMAIN

The DNS interception hook checks `fault_dns_map` before the normal `dns_map` lookup. If the service name has a fault entry of type NXDOMAIN, the eBPF program constructs an NXDOMAIN DNS response directly in the kernel and returns it to the application. The application's `getaddrinfo()` call fails with `EAI_NONAME` -- indistinguishable from a real DNS resolution failure.

```c
// eBPF pseudocode: DNS NXDOMAIN fault in DNS hook
SEC("sk_skb/stream_parser")
int smoker_dns_fault(struct __sk_buff *skb) {
    // Parse DNS query from the packet
    // (Simplified -- real implementation handles TCP/UDP framing,
    //  extracts query name, validates .internal suffix)
    char query_name[64];
    int query_len = parse_dns_query(skb, query_name, sizeof(query_name));
    if (query_len <= 0)
        return SK_PASS;  // not a DNS query we care about

    // Hash the service name portion (strip .internal suffix)
    __u32 name_hash = fnv1a_hash(query_name, service_name_len(query_name));

    // Check fault map BEFORE normal service map
    struct bpf_dns_fault_key fkey = { .service_name_hash = name_hash };
    struct bpf_dns_fault_value *fval =
        bpf_map_lookup_elem(&fault_dns_map, &fkey);

    if (!fval)
        goto normal_resolution;  // no fault, proceed to Onion dns_map

    // Check expiry
    __u64 now = bpf_ktime_get_ns();
    if (fval->expires_ns != 0 && now > fval->expires_ns)
        goto normal_resolution;

    // Check probability
    if (fval->probability < 100) {
        __u32 state_key = 0;
        struct bpf_fault_state_value *state =
            bpf_map_lookup_elem(&fault_state_map, &state_key);
        if (state) {
            __u64 x = state->prng_state;
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            state->prng_state = x;
            if ((x % 100) >= fval->probability)
                goto normal_resolution;
        }
    }

    if (fval->action == DNS_FAULT_NXDOMAIN) {
        // Construct NXDOMAIN response in-place:
        // - Copy the query ID from the request
        // - Set QR=1 (response), RCODE=3 (NXDOMAIN)
        // - Zero answer/authority/additional counts
        // - Keep the original question section
        construct_nxdomain_response(skb);
        return SK_PASS;  // modified packet returned to application
    }

normal_resolution:
    // Fall through to normal Onion DNS resolution
    return onion_dns_resolve(skb, name_hash);
}
```

#### 5.1.4 Partition (via source cgroup check)

A partition between service A and service B is implemented as a directional rule in `fault_connect_map`. The key includes a source cgroup ID (identifying the calling app) and a destination virtual IP. Bidirectional partitions require two rules.

```c
// eBPF pseudocode: partition fault in connect4 program
SEC("cgroup/connect4")
int smoker_partition_connect4(struct bpf_sock_addr *ctx) {
    // Get the calling process's cgroup ID
    __u64 src_cgroup = bpf_get_current_cgroup_id();

    // First, check if there is a partition rule for this
    // specific source cgroup -> destination VIP pair
    struct bpf_connect_fault_key key = {
        .virtual_ip       = ctx->user_ip4,
        .port             = bpf_ntohs(ctx->user_port),
        .source_cgroup_id = src_cgroup,
    };

    struct bpf_connect_fault_value *val =
        bpf_map_lookup_elem(&fault_connect_map, &key);
    if (!val) {
        // Also check the wildcard (source_cgroup_id = 0) for
        // non-partition faults (delay, drop) that apply to all callers
        key.source_cgroup_id = 0;
        val = bpf_map_lookup_elem(&fault_connect_map, &key);
        if (!val)
            return CGROUP_SOCK_ADDR_ALLOW;
    }

    // Check expiry
    __u64 now = bpf_ktime_get_ns();
    if (val->expires_ns != 0 && now > val->expires_ns)
        return CGROUP_SOCK_ADDR_ALLOW;

    if (val->action == FAULT_ACTION_PARTITION) {
        // Return ENETUNREACH -- application sees a network
        // unreachable error, as if the destination network
        // has been physically partitioned.
        return CGROUP_SOCK_ADDR_REJECT_UNREACH;  // maps to -ENETUNREACH
    }

    // Handle other fault types (drop, delay) as shown above
    // ...

    return CGROUP_SOCK_ADDR_ALLOW;
}
```

#### 5.1.5 Bandwidth Throttle (via tc eBPF + token bucket)

Bandwidth throttling is implemented using a `tc` (traffic control) eBPF program attached to the container's network interface, combined with a token bucket rate limiter stored in `fault_bw_map`. Packets exceeding the configured rate are queued in a BPF ring buffer and released at the throttled rate. This operates at the packet level, giving accurate bandwidth shaping.

```c
// eBPF pseudocode: bandwidth throttle in tc program
SEC("tc")
int smoker_bandwidth_tc(struct __sk_buff *skb) {
    // Extract destination IP:port from packet headers
    struct iphdr *iph = (void *)(long)skb->data + sizeof(struct ethhdr);
    struct tcphdr *tcph = (void *)iph + (iph->ihl * 4);

    struct bpf_bandwidth_fault_key key = {
        .virtual_ip = iph->daddr,
        .port       = bpf_ntohs(tcph->dest),
    };

    struct bpf_bandwidth_fault_value *val =
        bpf_map_lookup_elem(&fault_bw_map, &key);
    if (!val)
        return TC_ACT_OK;  // no throttle, pass through

    // Check expiry
    __u64 now = bpf_ktime_get_ns();
    if (val->expires_ns != 0 && now > val->expires_ns) {
        return TC_ACT_OK;
    }

    // Token bucket algorithm:
    // 1. Refill tokens based on time elapsed since last refill
    __u64 elapsed_ns = now - val->last_refill_ns;
    __u64 new_tokens = (val->rate_bytes_per_sec * elapsed_ns) / 1000000000ULL;
    val->tokens += new_tokens;
    val->last_refill_ns = now;

    // Cap tokens at 1 second worth of burst (rate_bytes_per_sec)
    if (val->tokens > val->rate_bytes_per_sec)
        val->tokens = val->rate_bytes_per_sec;

    // 2. Check if we have enough tokens for this packet
    __u32 pkt_len = skb->len;
    if (val->tokens >= pkt_len) {
        // Enough tokens: deduct and pass
        val->tokens -= pkt_len;
        bpf_map_update_elem(&fault_bw_map, &key, val, BPF_EXIST);
        return TC_ACT_OK;
    }

    // 3. Not enough tokens: queue the packet for delayed delivery.
    //    In practice, we use TC_ACT_PIPE to redirect to a delay
    //    qdisc, or TC_ACT_SHOT to drop excess packets (simpler
    //    but less faithful).
    //    For production fidelity, redirect to a BPF ring buffer
    //    and have a timer release packets at the throttled rate.
    bpf_map_update_elem(&fault_bw_map, &key, val, BPF_EXIST);
    return TC_ACT_SHOT;  // drop excess (simplified)
    // Production: return TC_ACT_PIPE to redirect to a pacing qdisc
}
```

### 5.2 Resource Faults

Resource faults do not use eBPF. They use the same cgroup and process control mechanisms that Bun already employs for container management.

#### 5.2.1 CPU Burn Loop

Bun spawns a lightweight burn process inside the target container's CPU cgroup. The burn process is a tight arithmetic loop compiled directly into the Bun binary. Because it runs inside the same cgroup, it competes with the application for CPU time exactly as a real noisy-neighbour workload would.

```rust
/// CPU burn loop, spawned inside target cgroup.
/// `target_percent` is 0-100: how much of the cgroup CPU quota to consume.
/// `cores` optionally limits stress to N cores (default: all).
fn cpu_burn_loop(target_percent: u8, cores: Option<u32>) {
    // Pin to the target cgroup's CPU set
    // If `cores` is specified, only stress that many cores
    let core_count = cores.unwrap_or_else(|| num_cpus_in_cgroup());

    for core_idx in 0..core_count {
        std::thread::spawn(move || {
            pin_to_cpu(core_idx);
            loop {
                // Burn for target_percent of each 10ms window
                let burn_duration = Duration::from_micros(
                    (100 * target_percent as u64)  // 10ms * percent / 100
                );
                let sleep_duration = Duration::from_micros(
                    10_000 - (100 * target_percent as u64)
                );

                let start = Instant::now();
                // Tight arithmetic loop (not optimizable away)
                let mut acc: u64 = 0xdeadbeef;
                while start.elapsed() < burn_duration {
                    acc = acc.wrapping_mul(6364136223846793005)
                             .wrapping_add(1);
                    std::hint::black_box(acc);
                }

                if sleep_duration > Duration::ZERO {
                    std::thread::sleep(sleep_duration);
                }
            }
        });
    }
}
```

#### 5.2.2 Memory Pressure via mlock

Bun spawns a process inside the target container's memory cgroup that allocates and `mlock()`s pages, pushing the container toward its memory limit. At 90%, the application has only 10% of its memory headroom remaining. This triggers the same kernel memory pressure signals (PSI, `memory.high` events) as real contention.

```rust
/// Allocate and mlock memory inside the target cgroup to create pressure.
/// `target_percent` is 0-100 (percentage of cgroup memory limit to consume).
/// `oom` if true, allocate beyond limit to trigger OOM kill.
fn memory_pressure(
    cgroup_memory_limit: u64,
    target_percent: u8,
    oom: bool,
) {
    let target_bytes = if oom {
        // Allocate more than the limit to trigger OOM
        cgroup_memory_limit + (64 * 1024 * 1024)  // limit + 64MB
    } else {
        // Calculate current usage, then allocate the difference
        let current_usage = read_cgroup_memory_current();
        let target_usage = (cgroup_memory_limit * target_percent as u64) / 100;
        if target_usage <= current_usage {
            return;  // already at or above target
        }
        target_usage - current_usage
    };

    // Allocate in 4KB page chunks to avoid a single giant allocation
    let page_size = 4096_usize;
    let num_pages = (target_bytes as usize) / page_size;
    let mut pages: Vec<*mut u8> = Vec::with_capacity(num_pages);

    for _ in 0..num_pages {
        let page = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if page == libc::MAP_FAILED {
            break;  // OOM killer will handle the rest
        }

        // Touch the page to ensure it is physically allocated
        unsafe { std::ptr::write_volatile(page as *mut u8, 0xAA) };

        // mlock to prevent kernel from reclaiming it
        unsafe { libc::mlock(page, page_size) };

        pages.push(page as *mut u8);
    }

    // Hold allocations until fault is cleared or expires.
    // The parent Bun process will kill this child to release memory.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
```

#### 5.2.3 Disk I/O Throttle via blkio cgroup

Bun writes to the `blkio` cgroup controller for the target container, using the kernel's native I/O throttling mechanism.

```rust
/// Apply disk I/O throttle to a container via blkio cgroup.
/// `bytes_per_sec`: maximum throughput in bytes/sec.
/// `write_only`: if true, only throttle writes.
fn apply_disk_io_throttle(
    cgroup_path: &Path,
    bytes_per_sec: u64,
    write_only: bool,
    device_major_minor: &str,  // e.g. "8:0" for /dev/sda
) -> io::Result<()> {
    // cgroupv2: io.max file
    // Format: "MAJOR:MINOR rbps=BYTES wbps=BYTES"
    let io_max_path = cgroup_path.join("io.max");

    let value = if write_only {
        format!("{} rbps=max wbps={}", device_major_minor, bytes_per_sec)
    } else {
        format!("{} rbps={} wbps={}",
            device_major_minor, bytes_per_sec, bytes_per_sec)
    };

    fs::write(&io_max_path, value.as_bytes())?;
    Ok(())
}

/// Remove disk I/O throttle (restore unlimited).
fn remove_disk_io_throttle(
    cgroup_path: &Path,
    device_major_minor: &str,
) -> io::Result<()> {
    let io_max_path = cgroup_path.join("io.max");
    let value = format!("{} rbps=max wbps=max", device_major_minor);
    fs::write(&io_max_path, value.as_bytes())?;
    Ok(())
}
```

### 5.3 Process Faults

Bun sends signals directly to the container's main process (PID 1 inside the container namespace).

```rust
/// Send a signal to a container's process.
fn process_fault(
    container_pid: pid_t,        // PID in Bun's PID namespace
    fault_type: ProcessFaultType,
) -> Result<()> {
    match fault_type {
        ProcessFaultType::Kill => {
            // SIGKILL: immediate termination, simulates OOM kill or crash.
            // Bun's container supervisor will detect the death and trigger
            // the normal restart/reschedule logic.
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(container_pid),
                nix::sys::signal::Signal::SIGKILL,
            )?;
        }
        ProcessFaultType::Pause => {
            // SIGSTOP: freeze the process. Health checks will fail after
            // the configured timeout, triggering restart/reschedule logic.
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(container_pid),
                nix::sys::signal::Signal::SIGSTOP,
            )?;
        }
        ProcessFaultType::Resume => {
            // SIGCONT: unfreeze a previously paused process.
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(container_pid),
                nix::sys::signal::Signal::SIGCONT,
            )?;
        }
    }
    Ok(())
}
```

For `relish fault kill web --count 2`, Bun selects `count` instances randomly from all healthy instances of the service and sends SIGKILL to each.

### 5.4 Node-Level Faults

#### 5.4.1 Node Drain

Simulates a graceful node departure. Bun on the target node marks itself as draining, stops accepting new work, and waits for running apps to be rescheduled elsewhere. This tests the same code path as a real node maintenance event.

```rust
fn simulate_node_drain(node_id: &str, duration: Duration) -> Result<()> {
    // 1. Mark node as draining in the cluster state
    set_node_state(node_id, NodeState::Draining);

    // 2. Stop accepting new container placements
    disable_scheduling(node_id);

    // 3. Begin graceful eviction of running containers
    //    (same code path as real drain -- containers get SIGTERM,
    //    wait for grace period, then SIGKILL)
    for container in list_containers_on_node(node_id) {
        initiate_graceful_stop(container, DRAIN_GRACE_PERIOD);
    }

    // 4. The scheduler sees the node as draining and places
    //    replacement containers on other nodes.

    // 5. After duration expires, restore normal state
    schedule_recovery(node_id, duration, || {
        set_node_state(node_id, NodeState::Ready);
        enable_scheduling(node_id);
    });

    Ok(())
}
```

#### 5.4.2 Node Kill

Simulates an abrupt node failure. Bun on the target node immediately stops responding to gossip heartbeats, stops reporting via the reporting tree, and freezes all local coordination. From the cluster's perspective, the node has vanished. This tests failure detection, leader reconstruction, and rescheduling -- the full disaster recovery path.

Running containers on the "killed" node continue serving traffic that reaches them directly (simulating the real-world scenario where a node's network is partially reachable), but the cluster stops routing new traffic to them.

```rust
fn simulate_node_kill(
    node_id: &str,
    duration: Duration,
    kill_containers: bool,
) -> Result<()> {
    // 1. Immediately stop gossip heartbeats
    gossip_pause(node_id);

    // 2. Stop reporting tree participation
    reporting_tree_disconnect(node_id);

    // 3. Stop responding to leader health checks
    health_responder_pause(node_id);

    // 4. Optionally stop all containers (--containers too)
    if kill_containers {
        for container in list_containers_on_node(node_id) {
            send_signal(container.pid, Signal::SIGKILL);
        }
    }
    // If kill_containers is false, containers keep running but
    // are orphaned from the cluster's perspective.

    // 5. The cluster's failure detection will notice the missing
    //    heartbeats after gossip_suspicion_timeout (typically 5-10s).
    //    The leader will mark the node as failed, trigger
    //    reconstruction of state, and reschedule containers.

    // 6. After duration expires, restore the node
    schedule_recovery(node_id, duration, || {
        gossip_resume(node_id);
        reporting_tree_reconnect(node_id);
        health_responder_resume(node_id);
        // Node will rejoin the cluster via normal gossip protocol
    });

    Ok(())
}
```

### 5.5 Scripted TOML Scenarios

For multi-step fault injection (chaos game days, automated resilience tests, CI pipeline integration), faults can be defined in a TOML file:

```toml
# chaos/payment-cascade.toml
#
# Scenario: payment service database goes slow, then drops,
# then the payment service itself runs out of memory.
# Tests: circuit breakers, retry logic, graceful degradation,
# customer-facing error handling.

name = "Payment cascade failure"

[[step]]
description = "Database latency spike"
fault = "delay"
target = "pg"
value = "500ms"
jitter = "200ms"
duration = "2m"

[[step]]
description = "Database starts dropping connections"
fault = "drop"
target = "pg"
value = "25%"
start_after = "2m"
duration = "3m"

[[step]]
description = "Payment service memory pressure from retry storms"
fault = "memory"
target = "payment-service"
value = "95%"
start_after = "4m"
duration = "2m"

[[step]]
description = "Payment service OOM"
fault = "memory"
target = "payment-service"
value = "oom"
start_after = "6m"
```

Scenario execution:

```rust
/// Execute a scripted chaos scenario.
fn execute_scenario(
    scenario: &ScriptedScenario,
    speed_multiplier: f64,    // 1.0 = normal, 2.0 = double speed
    dry_run: bool,
) -> Result<ScenarioResult> {
    let scenario_start = Instant::now();

    // Parse all steps and compute absolute activation times
    let mut timeline: Vec<(Duration, &ScenarioStep)> = Vec::new();
    for step in &scenario.steps {
        let start_after = match &step.start_after {
            Some(s) => parse_duration(s)?,
            None => Duration::ZERO,
        };
        // Apply speed multiplier
        let adjusted = Duration::from_secs_f64(
            start_after.as_secs_f64() / speed_multiplier
        );
        timeline.push((adjusted, step));
    }

    // Sort by activation time
    timeline.sort_by_key(|(t, _)| *t);

    if dry_run {
        println!("Scenario: {}", scenario.name);
        println!("Steps ({}):", timeline.len());
        for (time, step) in &timeline {
            let duration = step.duration.as_deref().unwrap_or("until scenario ends");
            println!("  T+{:>6}: {} {} {} ({}s) -- {}",
                format_duration(*time),
                step.fault, step.target, step.value,
                duration, step.description);
        }
        return Ok(ScenarioResult::DryRun);
    }

    println!("Executing scenario: {}", scenario.name);

    for (activation_time, step) in &timeline {
        // Wait until it is time to activate this step
        let elapsed = scenario_start.elapsed();
        if *activation_time > elapsed {
            let wait = *activation_time - elapsed;
            println!("  Waiting {:?} for next step...", wait);
            std::thread::sleep(wait);
        }

        println!("  T+{}: {} -- {} {} {}",
            format_duration(scenario_start.elapsed()),
            step.description, step.fault, step.target, step.value);

        // Convert step to FaultRule and submit via the normal API path
        // (which enforces permissions and safety rails)
        let fault_rule = step_to_fault_rule(step, speed_multiplier)?;
        submit_fault(fault_rule)?;
    }

    Ok(ScenarioResult::Completed)
}
```

Scenarios are version-controlled alongside application configuration. Teams can build a library of failure scenarios and replay them after every significant change.

### 5.6 Fault Expiry and Cleanup

Every fault has a mandatory expiry. Cleanup happens through three independent mechanisms for defense in depth:

1. **eBPF-level expiry check.** Every eBPF fault map lookup compares `bpf_ktime_get_ns()` against the entry's `expires_ns`. Expired entries are skipped (treated as no-fault). This means faults stop having effect immediately at expiry even if userspace cleanup is delayed.

2. **Bun userspace timer.** Bun maintains a priority queue of active faults sorted by expiry time. A background task wakes at each expiry and deletes the corresponding BPF map entry via `bpf_map_delete_elem()`. It also cleans up any resource faults (kills CPU burn processes, releases mlock'd memory, resets blkio limits).

3. **Startup cleanup.** On Bun startup, the fault registry is empty (in-process state only). Bun iterates all fault BPF maps and deletes any entries it finds. This handles the case where Bun crashed while faults were active.

```rust
/// Background task that enforces fault expiry in userspace.
async fn fault_expiry_task(fault_registry: Arc<Mutex<FaultRegistry>>) {
    loop {
        let next_expiry = {
            let registry = fault_registry.lock().unwrap();
            registry.next_expiry()
        };

        match next_expiry {
            Some(expiry) => {
                let now = monotonic_now();
                if expiry > now {
                    tokio::time::sleep(Duration::from_nanos(expiry - now)).await;
                }
                // Clean up all expired faults
                let mut registry = fault_registry.lock().unwrap();
                let expired = registry.drain_expired(monotonic_now());
                for fault in expired {
                    cleanup_fault(&fault);
                }
            }
            None => {
                // No active faults, sleep until woken by new fault activation
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Clean up a single fault: remove BPF map entries, kill helper processes.
fn cleanup_fault(fault: &FaultRule) {
    match &fault.fault_type {
        FaultType::Delay { .. } | FaultType::Drop { .. } | FaultType::Partition { .. } => {
            // Delete from fault_connect_map
            let key = build_connect_fault_key(fault);
            bpf_map_delete_elem(&fault_connect_map_fd, &key);
        }
        FaultType::DnsNxdomain => {
            let key = build_dns_fault_key(fault);
            bpf_map_delete_elem(&fault_dns_map_fd, &key);
        }
        FaultType::Bandwidth { .. } => {
            let key = build_bw_fault_key(fault);
            bpf_map_delete_elem(&fault_bw_map_fd, &key);
        }
        FaultType::CpuStress { .. } => {
            // Kill the burn loop process
            kill_helper_process(fault.id, Signal::SIGKILL);
        }
        FaultType::MemoryPressure { .. } => {
            // Kill the mlock process (kernel reclaims pages)
            kill_helper_process(fault.id, Signal::SIGKILL);
        }
        FaultType::DiskIoThrottle { .. } => {
            // Reset blkio limits to unlimited
            reset_blkio_limits(&fault.target_service);
        }
        FaultType::Pause => {
            // Send SIGCONT to unfreeze
            if let Some(pid) = get_container_pid(&fault.target_service, &fault.target_instance) {
                nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    Signal::SIGCONT,
                );
            }
        }
        // Kill, NodeDrain, NodeKill: no cleanup needed (effect is immediate and permanent)
        _ => {}
    }
}
```

---

## 6. Configuration

### 6.1 Default Duration

If `--duration` is omitted from a `relish fault` command, the fault defaults to **10 minutes**. The CLI prints a warning:

```
Warning: No --duration specified. Fault will auto-expire in 10 minutes.
```

### 6.2 Maximum Duration

Every fault has a maximum duration, configurable per cluster. The default maximum is **1 hour**.

```toml
# cluster-config.toml
[smoker]
default_duration = "10m"
max_duration = "1h"
```

Attempts to set a duration longer than `max_duration` are rejected by the leader:

```
Error: Requested duration 2h exceeds maximum allowed duration 1h.
Use a shorter duration or ask a cluster admin to increase smoker.max_duration.
```

### 6.3 Safety Overrides

Some safety rails can be explicitly overridden with flags:

| Safety Rail | Override Flag | Default |
|---|---|---|
| Leader targeting | `--include-leader` | Blocked |
| >50% node faults | `--override-safety` | Blocked |
| Quorum risk | Cannot be overridden | Always enforced |
| Last-replica kill | Cannot be overridden | Always enforced |

Overrides are logged with additional prominence in the audit log:

```
WARN: Fault injected with --override-safety by alice@myorg.
      Affects 4/6 nodes (67%). Quorum protection still enforced.
```

---

## 7. Failure Modes

### 7.1 Fault That Outlives Its Duration

**Scenario:** The userspace expiry timer in Bun fails to fire (e.g., Bun is under extreme load and the async runtime is delayed).

**Mitigation:** eBPF-level expiry check. Every fault map lookup in the eBPF programs compares `bpf_ktime_get_ns()` against the entry's `expires_ns` field. Even if userspace never cleans up, the eBPF program will stop applying the fault once it expires. The BPF map entry remains (wasting a small amount of kernel memory) until userspace eventually cleans it, but the fault has no effect.

### 7.2 eBPF Map Corruption

**Scenario:** A BPF map entry is partially written (e.g., Bun crashes mid-write) and contains garbage values.

**Mitigation:** BPF map updates via `bpf_map_update_elem()` are atomic at the kernel level -- the update either fully succeeds or fully fails. Partial writes are not possible. However, if the Rust struct serialisation produces invalid values (e.g., `probability > 100`), the eBPF program validates all fields before applying a fault. Invalid entries are treated as no-fault and a per-CPU error counter is incremented for observability.

```c
// Validation in eBPF
if (val->probability > 100 || val->action > FAULT_ACTION_MAX) {
    // Invalid entry, treat as no-fault
    __sync_fetch_and_add(&fault_error_count, 1);
    return CGROUP_SOCK_ADDR_ALLOW;
}
```

### 7.3 Cleanup Failure on Bun Crash

**Scenario:** Bun crashes while faults are active. The in-process fault registry is lost.

**Mitigation:** Three-layer defense:

1. **eBPF expiry.** Active faults continue to have effect but will stop at their expiry timestamp (checked in-kernel on every lookup).

2. **Startup cleanup.** When Bun restarts, it iterates all fault BPF maps and deletes every entry. This is safe because fault state is never persisted to disk -- a Bun restart should always produce a clean state.

3. **Manual override.** `relish fault clear` sends a command directly to the Bun agent via a Unix socket (bypassing the cluster API). Even if the cluster leader is unreachable, an operator with host access can clear faults locally.

### 7.4 Resource Fault Helper Process Orphan

**Scenario:** Bun crashes while a CPU burn loop or memory pressure process is running inside a container's cgroup.

**Mitigation:** The helper process is spawned as a direct child of the Bun process. When Bun dies, the helper process receives SIGHUP. The burn loop and memory pressure processes install a SIGHUP handler that immediately exits. Additionally, cgroup limits (CPU, memory) will cause the orphaned helper to be killed if Bun's cgroup is cleaned up by the init system (systemd unit restart).

### 7.5 Node-Kill Fault Recovery Failure

**Scenario:** A node-kill fault is active and the duration expires, but the node cannot rejoin the cluster because gossip state has diverged too far.

**Mitigation:** The node-kill recovery path uses the same rejoin mechanism as a real node restart. Gossip protocol handles this via full state sync on reconnection. If the node has been "dead" for longer than the gossip tombstone window, it performs a full state pull from a seed node before resuming normal gossip.

---

## 8. Security Considerations

### 8.1 Permission Model

Fault injection requires the `admin` role or an explicit `fault-injection` Permission grant. The default `deployer` role cannot inject faults. All fault injections are logged as events with full attribution:

```
Event: fault.injected
  who:    alice@myorg
  what:   delay redis 200ms --duration 5m
  when:   2026-02-16T14:32:00Z
  from:   10.0.1.42
  node:   node-03
```

Permission evaluation flow:

```
relish fault delay redis 200ms
  -> Relish CLI sends request with user identity (from auth token)
  -> Leader receives request
  -> Leader checks: does user have 'admin' role OR 'fault-injection' permission?
  -> If no: reject with 403 Forbidden
  -> If yes: proceed to safety rail evaluation
```

### 8.2 Blast Radius Protection

Smoker prevents faults that would make the cluster unrecoverable:

- **Quorum protection.** Cannot partition more than `(council_size - 1) / 2` council nodes. This is the hard limit that preserves Raft quorum. It cannot be overridden.
- **Replica minimum.** Cannot kill more replicas of an app than `replicas - 1`. At least one instance must survive. Cannot be overridden.
- **Leader protection.** Cannot target the cluster leader without the explicit `--include-leader` flag.
- **Node percentage limit.** Cannot inject faults on more than 50% of nodes simultaneously without `--override-safety`.

### 8.3 No Persistence

Fault rules exist only in BPF maps (kernel memory) and Bun's in-process state. They are never written to disk. This means:

- A Bun restart on any node clears all faults on that node.
- A full cluster restart clears everything.
- There is no way for a fault to "survive" a restart and silently affect production.
- Forensic analysis of a node's disk cannot reveal what faults were injected (only the audit log, which is separate, records this).

### 8.4 Unix Socket Access Control

`relish fault clear` is designed to always work, even if the cluster API is degraded. It is processed locally by the Bun agent via a Unix socket. This socket:

- Is **not** mounted into any workload's namespace. Containers cannot invoke it.
- Is only accessible from the host filesystem or via the cluster API (which requires `admin` or `fault-injection` permission).
- Uses standard Unix file permissions (owned by root, mode 0600 or group-readable for the Bun group).

This ensures that even during a fault injection gone wrong, an operator with host access can always clear all faults.

---

## 9. Performance

### 9.1 Zero Overhead When Inactive

When no faults are active, all four fault BPF maps are empty. The eBPF programs perform one additional hash lookup per interception (for each relevant map). An empty BPF hash map lookup is approximately 20-50 nanoseconds, which is negligible compared to the cost of a `connect()` syscall (~1 microsecond) or DNS resolution (~10 microseconds).

Measured overhead with empty fault maps:

| Operation | Baseline (Onion only) | With Smoker maps (empty) | Overhead |
|---|---|---|---|
| `connect()` to VIP | ~1.2 us | ~1.25 us | ~50 ns (+4%) |
| DNS `.internal` resolution | ~8 us | ~8.05 us | ~50 ns (<1%) |
| TCP throughput (sustained) | 9.4 Gbps | 9.4 Gbps | unmeasurable |

### 9.2 Fault Activation Latency

When a fault is activated via `relish fault`, the following latency budget applies:

| Step | Latency |
|---|---|
| CLI to leader API (network) | ~1-5 ms |
| Leader permission + safety check | ~0.1 ms |
| Leader to target node(s) via reporting tree | ~1-10 ms |
| Bun writes BPF map entry | ~0.01 ms |
| eBPF program sees new entry on next hook invocation | ~0 ms (next syscall) |
| **Total** | **~2-15 ms** |

Fault activation is effectively instantaneous from an operator's perspective. The new fault affects the very next `connect()` or DNS resolution after the BPF map write.

### 9.3 Overhead With Active Faults

When faults are active, overhead depends on the fault type:

- **Drop, Partition, DNS NXDOMAIN:** Near-zero additional overhead. The eBPF program finds the map entry, applies the fault, and returns. Faster than the normal path (no actual connection or DNS resolution happens).
- **Delay:** Negligible eBPF overhead. The actual delay is implemented by the TCP stack's DELACK timer, which is a kernel-native mechanism.
- **Bandwidth:** Token bucket computation adds ~100-200 ns per packet in the tc program. This is well within the noise of network processing.
- **CPU stress, memory pressure:** These faults intentionally consume resources. Overhead is the intended effect, not a side effect.

---

## 10. Testing Strategy

### 10.1 Fault Injection + Recovery Verification

Each fault type must be tested not only for correct injection but also for correct recovery when the fault is cleared or expires.

**Network fault tests:**

- Inject `delay redis 200ms`, verify that connections to redis take ~200ms longer (within jitter bounds). Clear the fault, verify latency returns to baseline.
- Inject `drop api 50%`, make 1000 connections, verify ~500 fail with ECONNREFUSED (within statistical bounds). Clear, verify 0% failures.
- Inject `dns redis nxdomain`, verify `getaddrinfo("redis.internal")` returns EAI_NONAME. Clear, verify resolution works.
- Inject `partition web --from payment`, verify payment->web connections fail with ENETUNREACH, verify web->payment still works. Clear, verify both directions work.
- Inject `bandwidth api 1mbps`, transfer 10MB, verify transfer time is ~10 seconds (+/- 20%). Clear, verify full bandwidth.

**Resource fault tests:**

- Inject `cpu inference 50%`, verify inference container CPU usage rises by ~50% of its cgroup limit. Clear, verify CPU returns to normal.
- Inject `memory redis 90%`, verify redis container memory usage is ~90% of limit. Clear, verify memory drops (pages released).
- Inject `disk-io web 10mbps`, perform disk write benchmark, verify throughput is ~10MB/s. Clear, verify full throughput.

**Process fault tests:**

- Inject `kill web-3`, verify web-3 PID receives SIGKILL, verify Bun detects death and reschedules. Verify other web instances are unaffected.
- Inject `pause web-3`, verify web-3 process is stopped (SIGSTOP), verify health checks fail after timeout, verify `--resume` sends SIGCONT and process resumes.

**Expiry tests:**

- Inject fault with `--duration 5s`, wait 5 seconds, verify fault is automatically cleared. Verify eBPF-level expiry (fault stops taking effect) even if userspace cleanup is artificially delayed.

**Node-level fault tests:**

- Inject `node-drain node-05`, verify containers are evicted and rescheduled. Verify node rejoin after duration.
- Inject `node-kill node-05`, verify gossip detects failure, leader reschedules workloads. Verify node rejoin after duration.

### 10.2 Safety Rail Testing

Safety rails must be tested to verify they correctly prevent dangerous faults:

- Attempt to partition a majority of council nodes. Verify rejection with QuorumRisk error.
- Attempt to kill all replicas of a service. Verify rejection with ReplicaMinimum error.
- Attempt to fault the leader node. Verify rejection unless `--include-leader` is present.
- Attempt to fault >50% of nodes. Verify rejection unless `--override-safety` is present.
- Verify that `relish fault clear` works via Unix socket even when the cluster API is unavailable.

### 10.3 Chaos Test for Smoker Itself

Smoker should be tested under its own fault conditions:

- Inject a fault, then crash and restart Bun. Verify startup cleanup removes all BPF map entries.
- Inject a fault, then kill the leader. Verify the fault continues to operate (BPF maps are on the target node, not the leader) and that `relish fault clear` still works via the local Unix socket.
- Inject multiple overlapping faults on the same service. Verify all are applied correctly and cleaned up independently.

---

## 11. Prior Art

### 11.1 Chaos Mesh (Kubernetes)

[Chaos Mesh](https://chaos-mesh.org/docs/) is a CNCF-incubating chaos engineering platform for Kubernetes. It defines faults as Custom Resource Definitions (CRDs) and uses an operator to reconcile them. Network faults are implemented by injecting iptables rules or tc qdisc configurations into the target pod's network namespace via a privileged sidecar container.

**Architecture:** CRDs -> Chaos Controller Manager -> Chaos Daemon (privileged DaemonSet) -> iptables/tc injection.

**Strengths:** Rich fault types (network, IO, stress, time, DNS, JVM, kernel), mature scheduling (cron-based), good Grafana dashboard integration.

**Limitations:** Requires CRD installation, RBAC configuration, and a privileged DaemonSet. iptables rules persist in the network namespace and have overhead even when no faults are active. Cleanup on controller crash requires manual CRD deletion.

### 11.2 Litmus (Kubernetes)

[LitmusChaos](https://docs.litmuschaos.io/) is another CNCF chaos engineering platform. It uses CRDs (ChaosEngine, ChaosExperiment, ChaosResult) and a Litmus agent (operator + runner pods) to execute experiments. Network faults also use iptables/tc.

**Architecture:** CRDs -> Chaos Operator -> Runner Pods (one per experiment) -> iptables/tc injection via privileged init containers.

**Strengths:** Large experiment hub (community-contributed experiments), built-in hypothesis validation, integration with CI/CD pipelines via LitmusCTL.

**Limitations:** Same iptables overhead as Chaos Mesh. Each experiment spawns runner pods, adding scheduling latency. CRD-based workflow is verbose compared to imperative CLI commands.

### 11.3 Gremlin (SaaS)

[Gremlin](https://www.gremlin.com/) is a commercial SaaS chaos engineering platform. It runs a privileged agent on each node that receives fault injection commands from the Gremlin control plane.

**Architecture:** SaaS control plane -> Agent (privileged container or host binary) -> tc/iptables/cgroups/signals.

**Strengths:** Polished UI, good blast radius controls, StatusCheck integration for automatic abort, team management features.

**Limitations:** SaaS dependency (requires internet connectivity, introduces a trust boundary). Privileged agent with broad host access. Cost scales with infrastructure size.

### 11.4 Netflix Chaos Monkey / Chaos Engineering

Netflix pioneered chaos engineering with [Chaos Monkey](https://netflix.github.io/chaosmonkey/) (random instance termination) and evolved it into the [Principles of Chaos Engineering](https://principlesofchaos.org/). Their approach focuses on steady-state hypothesis verification in production.

**Strengths:** Battle-tested philosophy, focus on production validation rather than staging-only testing. Netflix's [ChAP (Chaos Automation Platform)](https://netflixtechblog.com/chap-chaos-automation-platform-53e6d528371f) automates hypothesis-driven chaos experiments.

**Limitations:** Chaos Monkey itself is relatively simple (instance termination only). The broader Netflix chaos tooling is proprietary and tightly integrated with their Spinnaker-based deployment pipeline.

### 11.5 Toxiproxy

[Toxiproxy](https://github.com/Shopify/toxiproxy) is a TCP proxy that simulates network faults. Applications connect through Toxiproxy instead of directly to their dependencies.

**Strengths:** Simple, language-agnostic, no privileged access needed.

**Limitations:** Requires application configuration changes (point connections through proxy). Cannot simulate faults at the kernel level (DNS failures, cgroup pressure). Adds latency even when no faults are active (proxy hop).

### 11.6 What Smoker Borrows

- **From Chaos Mesh:** The taxonomy of fault types (network, IO, stress, time, DNS, process). The idea of scheduled/timed experiments.
- **From Litmus:** CI pipeline integration as a first-class use case. Scenario files are Smoker's equivalent of Litmus experiments.
- **From Gremlin:** Blast radius controls and automatic safety rails. StatusCheck-like abort behaviour via Smoker's safety checks.
- **From Netflix:** Production-first philosophy. Faults should be safe enough to run in production with appropriate guardrails.
- **From Toxiproxy:** The principle that fault injection should be simple to invoke (single command or API call).

### 11.7 What Smoker Does Differently

- **eBPF-native, no iptables.** Network faults operate at the socket level in eBPF, not at the packet level via iptables. This gives zero overhead when inactive (empty BPF map lookup vs. iptables rule chain traversal) and kernel-level fidelity (applications see real error codes, not RST packets).
- **Built-in, not bolted-on.** Smoker is part of the orchestrator binary. No CRDs, no operators, no DaemonSets, no sidecars. Fault injection is a natural extension of the eBPF programs and cgroup controls already in use.
- **Automatic safety rails.** Quorum protection, replica minimums, and leader guarding are enforced by default. Other systems require manual annotation or configuration to prevent dangerous faults.
- **No persistence.** BPF maps and in-process state only. No CRDs to orphan, no iptables rules to leak, no config files to corrupt. A restart always produces a clean state.
- **Imperative CLI, not declarative CRDs.** `relish fault delay redis 200ms` is simpler than writing a YAML manifest, applying it, waiting for reconciliation, and then deleting it to clean up. For repeatable tests, TOML scenario files provide declarative scripting without the reconciliation complexity.

---

## 12. Libraries and Dependencies

### 12.1 Rust Crates

**libbpf-rs** or **aya** -- eBPF map manipulation from Rust userspace. Both provide safe wrappers around the `bpf()` syscall for map CRUD operations. The choice depends on which library Onion already uses for its service mesh eBPF programs (Smoker should use the same library to share BPF object lifecycle management).

- `libbpf-rs`: Rust bindings to libbpf (C library). Mature, tracks upstream libbpf closely. Requires libbpf as a build dependency.
- `aya`: Pure Rust eBPF library. No C dependencies. Provides `aya::maps::HashMap` for typed BPF map access.

```rust
// Example: writing a fault entry using aya
use aya::maps::HashMap;

fn write_connect_fault(
    map: &mut HashMap<MapData, BpfConnectFaultKey, BpfConnectFaultValue>,
    key: BpfConnectFaultKey,
    value: BpfConnectFaultValue,
) -> Result<()> {
    map.insert(key, value, 0 /* flags: BPF_ANY */)?;
    Ok(())
}

fn delete_connect_fault(
    map: &mut HashMap<MapData, BpfConnectFaultKey, BpfConnectFaultValue>,
    key: &BpfConnectFaultKey,
) -> Result<()> {
    map.remove(key)?;
    Ok(())
}
```

**nix** -- Rust bindings to Unix system calls. Used for:

- `nix::sys::signal::kill()` -- sending SIGKILL, SIGSTOP, SIGCONT for process faults.
- `nix::unistd::Pid` -- PID type for signal targeting.
- Cgroup file operations (reading/writing cgroup control files for blkio, memory, CPU).
- `nix::sys::mman::mlock()` -- locking memory pages for the memory pressure fault.

```rust
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

kill(Pid::from_raw(container_pid), Signal::SIGKILL)?;
kill(Pid::from_raw(container_pid), Signal::SIGSTOP)?;
kill(Pid::from_raw(container_pid), Signal::SIGCONT)?;
```

**rand** -- PRNG for userspace operations (selecting random instances for `kill --count N`, jitter calculation in scenario timing). The eBPF programs use their own xorshift64 PRNG (BPF programs cannot call userspace random functions).

```rust
use rand::Rng;

fn select_random_instances(
    instances: &[ContainerInstance],
    count: u32,
) -> Vec<&ContainerInstance> {
    let mut rng = rand::thread_rng();
    let mut indices: Vec<usize> = (0..instances.len()).collect();
    // Fisher-Yates shuffle, take first `count`
    for i in 0..std::cmp::min(count as usize, indices.len()) {
        let j = rng.gen_range(i..indices.len());
        indices.swap(i, j);
    }
    indices.iter()
        .take(count as usize)
        .map(|&i| &instances[i])
        .collect()
}
```

### 12.2 Kernel Dependencies

- **Linux >= 5.7** -- Required for BPF ring buffer (`BPF_MAP_TYPE_RINGBUF`), used by bandwidth throttle for packet queuing.
- **CONFIG_BPF_SYSCALL, CONFIG_BPF_JIT** -- Required for eBPF (same as Onion).
- **CONFIG_CGROUP_BPF** -- Required for cgroup-attached eBPF programs (connect4 hook).
- **CONFIG_NET_SCH_BPF** -- Required for tc eBPF programs (bandwidth throttle).
- **cgroupv2** -- Required for blkio (`io.max`) and memory (`memory.max`, `memory.current`) cgroup controls.

---

## 13. Open Questions

### 13.1 HTTP-Level Fault Injection

Current network faults operate at the TCP/socket level. HTTP-level faults (injecting specific HTTP status codes like 503, adding response headers, corrupting response bodies) would require intercepting at a higher layer. Options:

- **sk_msg eBPF program** that inspects HTTP response headers in the `sendmsg` path and rewrites status codes. This is complex (HTTP parsing in eBPF is limited by instruction count and stack size) but would maintain the zero-sidecar property.
- **Userspace proxy** injected into the network path for the duration of the fault. This contradicts Smoker's "no new components" design but would be simpler and more flexible.
- **Application-level SDK** that checks a fault flag (set via shared memory or environment variable). This requires application cooperation but gives the most control.

Decision deferred. TCP-level faults cover the majority of chaos engineering use cases (connection failures, latency, partition). HTTP-level faults are a stretch goal.

### 13.2 Clock Skew Simulation

Simulating clock skew (making `gettimeofday()` / `clock_gettime()` return incorrect times) is valuable for testing distributed consensus algorithms, lease expiry logic, and certificate validation. However, intercepting time syscalls via eBPF is not straightforward:

- `clock_gettime()` uses VDSO on modern kernels, bypassing the syscall path entirely. eBPF cannot intercept VDSO calls.
- Chaos Mesh solves this by injecting a shared library (via `LD_PRELOAD`) that overrides the time functions. This requires modifying the container's environment.
- An alternative is using `CLOCK_NAMESPACE` (available in Linux 5.6+), which gives each container its own clock offset. This is clean but requires namespace setup at container creation time.

Decision deferred. Clock skew is important but the implementation options each have significant trade-offs. Needs further investigation into CLOCK_NAMESPACE feasibility.

### 13.3 Multi-Cluster Partition Testing

Simulating network partitions between clusters (rather than between services within a cluster) is relevant for multi-region deployments. This requires coordinating fault injection across multiple independent Reliaburger clusters.

Options:

- **Cross-cluster fault API** that allows one cluster's Smoker to inject faults affecting traffic to/from another cluster. Requires mutual authentication between clusters.
- **External orchestrator** (a script or CI pipeline) that issues `relish fault` commands to multiple clusters in sequence.

Decision deferred pending multi-cluster support in Reliaburger itself.

### 13.4 Fault Injection in the Data Plane vs. Control Plane

Should Smoker support faults that target the orchestrator's own control plane (gossip protocol, Raft consensus, reporting tree) in addition to application traffic? This would enable testing Reliaburger's own resilience, but introduces the risk of the fault injection system destabilizing itself.

Partial support exists: `node-kill` and `node-drain` affect the control plane indirectly. Direct control plane faults (e.g., "delay all Raft AppendEntries by 500ms") are not yet supported.

### 13.5 Observability Integration

Should Smoker emit structured events to the metrics system (Mayo) when faults are injected, so that fault injection windows are visible on dashboards alongside application metrics? This would make it trivial to correlate latency spikes with active faults during game days. The `relish wtf` diagnostic tool already detects active faults, but dashboard integration would be valuable for post-hoc analysis.

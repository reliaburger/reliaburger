# Breaking Things on Purpose

In 2012, Netflix deployed Chaos Monkey to production. It randomly killed instances in their AWS fleet. Engineers thought this was insane. Within a year, every team at Netflix had hardened their services against instance failure. The practice spread. Today we call it chaos engineering.

The idea is simple: if you don't know how your system fails, you're going to find out at 3am on a Saturday. Better to find out on a Tuesday afternoon, on your terms, with a rollback plan.

Most chaos engineering tools are separate systems. Chaos Mesh needs CRDs, an operator, and a privileged DaemonSet. Litmus spawns runner pods for each experiment. Gremlin is a SaaS with a privileged agent on every node. The barrier to entry is high enough that most teams never adopt them.

Smoker takes a different approach. It's built into Reliaburger. No extra
binaries, no sidecars, no CRDs. When no connect-time fault is active, the
loaded eBPF hook does one empty hash-map lookup and continues. When you want to
break a connection, it's one command:
`relish fault drop redis 100% --acknowledge`.

## Safety first

Before we write a single line of fault injection code, we need to answer a question: what happens if someone injects a fault that destroys the cluster?

This isn't hypothetical. A chaos engineering tool that can take down production is worse than no tool at all. Smoker has four safety rails, and two of them cannot be overridden.

```rust
pub enum SafetyViolation {
    QuorumRisk {
        current_affected: u32,
        max_allowed: u32,
    },
    ReplicaMinimum {
        service: String,
        current_replicas: u32,
        surviving: u32,
    },
    LeaderTargeted,
    NodePercentageExceeded {
        affected_nodes: u32,
        total_nodes: u32,
    },
}
```

Four variants. The `match` in `evaluate_safety` handles every one. The compiler won't let you add a fifth rail without handling it everywhere.

**Quorum protection** is the hard limit. In a 5-member council, you can fault at most 2 members — `(5 - 1) / 2 = 2`. A third would break Raft quorum, and the cluster would stop accepting writes. This rail cannot be overridden. No `--force`, no `--yes-i-really-mean-it`. If you need to test what happens when quorum breaks, you use the in-memory test infrastructure, not production.

**Replica minimum** prevents you from killing all instances of a service.
`relish fault kill web --count 0 --acknowledge` (kill all) is rejected if it
would leave zero surviving replicas. At least one must survive.

**Leader protection** blocks faults targeting the cluster leader unless you explicitly pass `--include-leader`. This is overridable because sometimes you *want* to test leader failover — but you should know you're doing it.

**Node percentage** blocks faults affecting more than 50% of nodes unless you pass `--override-safety`. Again, overridable with intent.

The evaluation order matters. Quorum is checked first, then replicas, then leader, then node percentage. If both quorum and leader are violated, the user sees the quorum error — the more dangerous one.

## The fault registry

Active faults live in an in-memory registry. Not on disk, not in Raft, not in a database. When Bun restarts, the registry is empty. This is the point.

```rust
pub struct FaultRegistry {
    faults: Vec<FaultRule>,
    expiry_queue: BinaryHeap<Reverse<(u64, u64)>>,
    next_id: u64,
}
```

Every fault has a mandatory expiry. If you don't pass `--duration`, it defaults to 10 minutes and the CLI prints a warning. There is no way to create a fault that lasts forever.

Cleanup happens through two independent mechanisms:

1. **Userspace expiry.** Every health tick (1 second), the agent calls `drain_expired()`, which pops entries from the min-heap and removes them from the registry. For any fault that left a durable change — a frozen process, a capped `cpu.max`, a squeezed `memory.high`, an `io.max` throttle — the agent then *reverses* it. Expiry isn't just forgetting the fault; it's putting the target back the way it was.

2. **eBPF-level expiry.** For network faults, the BPF programs check `bpf_ktime_get_ns()` against the entry's `expires_ns` field on every `connect()`. Even if the userspace timer is delayed, the kernel stops applying the fault at the right time.

The reversal is the part that's easy to get wrong, and it's worth being blunt about why. A chaos experiment that can't undo itself isn't an experiment, it's an outage you scheduled. Every persistent fault records what it needs to restore at the moment it's applied — the saved cgroup value, the list of PIDs it froze — and stashes that on the registry entry. When the fault is cleared (`relish fault clear <id>`), cleared en masse, or expires on its own, the same reversal runs. Three doors, one exit.

The registry is wrapped in `Arc<tokio::sync::Mutex<FaultRegistry>>` because the agent event loop and the expiry background task both need access. We use `tokio::sync::Mutex`, not `std::sync::Mutex`. In async code, a standard mutex can block the tokio runtime if the lock is held across an `.await` point. The tokio mutex yields instead.

## Process faults: the easy ones

The simplest faults are process signals.
`relish fault kill web-3 --acknowledge` sends SIGKILL to the container's main
process. `relish fault pause web --acknowledge` sends SIGSTOP, which freezes
the process. Health checks fail after the configured timeout, triggering the
restart logic.

A pause used to be a trap. SIGSTOP froze the process, but nothing ever un-froze it — expiry deleted the registry entry and left the workload wedged. You had to remember to send a separate `--resume` (SIGCONT) fault by hand. That's exactly the kind of "cleanup that doesn't clean up" a chaos tool must not have. Now a pause records the PIDs it froze, and when the fault clears or expires the agent SIGCONTs them automatically. The manual resume still exists, but you no longer *need* it: the process comes back on its own when the fault's time is up.

```rust
pub fn kill_process(pid: i32) -> Result<(), ProcessFaultError> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .map_err(|e| ProcessFaultError::SignalFailed {
        signal: "SIGKILL",
        pid,
        source: e,
    })
}
```

Three functions, three signals, three lines of real logic each. The `nix` crate provides type-safe wrappers around the `kill(2)` syscall. The error handling adds context (which signal, which PID) so debugging is straightforward.

These faults work on all Unix platforms — no eBPF, no cgroups, no Linux-specific features. You can test them on macOS.

## Resource faults: cgroup control

Resource faults use the same cgroup hierarchy that Bun already manages for container isolation. The key word is *target*: the limit has to land on the workload's own cgroup, not on Bun. The first version of this code got that wrong for CPU. It spawned burn loops on `spawn_blocking` threads and let them fight for whatever CPU the Bun process could get. That starved the orchestrator, not the app, and it couldn't be lifted before its deadline. It looked like a chaos fault and behaved like a self-inflicted wound.

So how does a fault find its target's cgroup? A resource fault arrives naming only a service, e.g. `web`. Bun keeps a list of the instances it runs, each carrying its namespace, app name and ordinal, and the cgroup for an instance is `/sys/fs/cgroup/reliaburger/{namespace}/{app}/{ordinal}`. Threading that instance metadata into the fault is what turns "throttle web" into "write this file for `default/web/0` and `default/web/1`". No instances match? The fault is rejected, not silently dropped.

**CPU stress** caps `cpu.max`. A cgroup-v2 `cpu.max` is `"{quota_us} {period_us}"` — how many microseconds of CPU the group may use per period. To steal 80% we leave the workload 20% of one core:

```rust
pub fn cpu_stress_quota(percentage: u8) -> String {
    let remaining = 100u64.saturating_sub(percentage as u64).max(1);
    let quota_us = remaining * CPU_PERIOD_US / 100;   // 100_000us period
    format!("{quota_us} {CPU_PERIOD_US}")             // e.g. "20000 100000"
}
```

The `.max(1)` is a deliberate floor: 100% would wedge the process entirely, and a process that can't run at all is a Kill, not a stress. Before we write the cap we *read* the current `cpu.max` and save it. That saved string is the reversal — clear or expire the fault and the original quota goes straight back.

**Memory pressure** squeezes `memory.high`, the soft limit. The kernel forces a workload into reclaim (and, past its working set, allocation stalls) as it approaches `memory.high`, without the outright OOM kill a lowered `memory.max` would cause. We set it to a percentage of the hard `memory.max`, so 90% pressure leaves a 10% headroom band, and we save the previous soft limit to restore later. A cgroup with no hard limit can't be squeezed this way, so the fault is refused rather than pretending. And an `--oom` request? That one genuinely isn't reversible — it kills — so we reject it and point you at a Kill fault, which is the honest way to test the OOM/restart path.

**Disk I/O throttle** writes cgroupv2's `io.max`, the kernel's native per-device throttle:

```rust
let value = format!("{device_major_minor} rbps={bytes_per_sec} wbps={bytes_per_sec}");
std::fs::write(&io_max_path, value.as_bytes())?;
```

`io.max` is per-device, so the throttle has to name a `major:minor`. We resolve it from the disk backing the node's volumes directory, where workloads actually write. Reversal writes `rbps=max wbps=max` back to the same file.

All resource faults are Linux-only. On macOS, the functions return `ResourceFaultError::UnsupportedPlatform`, and `apply_fault` turns that into a clear "requires Linux cgroups" rejection rather than a fake success. The pure computation (the quota arithmetic) is unit-tested everywhere; the actual cgroup writes and their reversal are tested against real cgroup files under `make test-linux`.

## Network faults: eBPF

This is where Smoker earns its keep. Network faults operate at the kernel level, in the same eBPF programs that Onion uses for service discovery.

The loaded Onion program adds one map alongside its existing maps:

- `fault_connect_map` — per-service connection faults (drop and partition)

Earlier designs also named `fault_bw_map` and `fault_state_map`. No loaded
program consumes them. Keeping their Rust structs didn't make bandwidth
shaping real, so Bun now refuses delay and bandwidth until a TC packet path
owns the maps and proves the effect.

There's no `fault_dns_map`. DNS resolution moved out of the kernel and into a userspace responder (Chapter 3), so the DNS fault lives there too. More on that in a moment — it's a good lesson in keeping a fault pointed at the code that actually runs.

The Rust-side structs use `#[repr(C)]` with explicit padding to match the C layouts exactly:

```rust
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BpfConnectFaultKey {
    pub virtual_ip: u32,
    pub port: u16,
    pub _pad: u16,
    pub source_cgroup_id: u64,
}
```

The `_pad` field exists because the C compiler inserts 2 bytes of padding between `port` (u16) and `source_cgroup_id` (u64, which needs 8-byte alignment). Without explicit padding, the Rust struct would have a different layout than the C struct, and BPF map operations would silently corrupt data.

We verify this with size assertions that run on every platform:

```rust
#[test]
fn connect_fault_key_size() {
    assert_eq!(std::mem::size_of::<BpfConnectFaultKey>(), 16);
}
```

If someone adds a field and forgets padding, this test catches it before any eBPF code runs.

### Connection drop

The simplest network fault. On each `connect()`, the eBPF program looks up `fault_connect_map`. If it finds a DROP entry, it generates a random number using a per-CPU xorshift64 PRNG and compares it to the configured probability:

```c
__u8 roll = x % 100;
if (roll < fval->probability) {
    state->faults_injected++;
    return 0;  /* -ECONNREFUSED */
}
```

The application sees `ECONNREFUSED` — exactly what a real connection failure looks like. No simulation layer, no proxy, no iptables rules. The connection never leaves the kernel.

### Partition

A partition between service A and service B uses the `source_cgroup_id` field
in the key. The eBPF program checks `bpf_get_current_cgroup_id()` against the
key. Clients don't get to assert that id: Bun resolves every running instance
of the named source app, records every exact key it writes, and removes those
keys on clear or expiry. If the calling process is in a blocked cgroup and the
destination matches, Linux refuses `connect()` with `EPERM` before sending a
packet.

Bidirectional partitions require two map entries (A→B and B→A). Unidirectional partitions are one entry — A can't reach B, but B can still reach A.

### DNS NXDOMAIN (and a fault that did nothing)

Here's a fault that taught us a lesson. The `DnsNxdomain` fault is meant to make a service's name stop resolving: you point it at `redis`, and any container that asks for `redis.internal` gets NXDOMAIN back, exactly as if Redis had never been deployed. It's how you prove your app survives a dependency vanishing from DNS.

For a long time it did nothing at all.

The original plan put the fault in the kernel: a `fault_dns_map` that the in-kernel DNS interception hook would check before resolving. But that in-kernel DNS path never shipped. As Chapter 3 explains, cgroup socket hooks can rewrite an address but can't read or synthesise a DNS packet payload, so DNS resolution moved to a small **userspace** responder (`src/onion/dns.rs`). The fault, though, stayed pointed at the kernel map — a map that lived only in an eBPF object we never load. So applying `DnsNxdomain` dutifully wrote an entry into a map nobody reads, reported success, and changed nothing. The acceptance gate in Chapter 15 caught it: an advertised fault that's a silent no-op is worse than no fault, because you *think* you tested something.

The fix is to put the fault where the code actually runs. The responder resolves `.internal` names; the fault belongs in that lookup. We give the responder a read-only handle to "which services are currently faulted", and it checks that handle before it answers:

```rust
use std::collections::BTreeMap;

/// Which services the Smoker is currently forcing NXDOMAIN for.
#[derive(Debug, Clone, Default)]
pub struct DnsFaultState {
    /// App name → expiry (CLOCK_MONOTONIC nanoseconds). 0 means "no expiry".
    faulted: BTreeMap<String, u64>,
}

impl DnsFaultState {
    pub fn is_faulted(&self, app: &str, now_ns: u64) -> bool {
        match self.faulted.get(app) {
            Some(&expires_ns) => expires_ns == 0 || now_ns < expires_ns,
            None => false,
        }
    }
}
```

`BTreeMap` is Rust's ordered map (a balanced tree, like C++'s `std::map`); we use it rather than the hash-based `HashMap` because the set is tiny and an ordered map serialises and prints deterministically, which is easier to reason about. The `is_faulted` method returns `true` only while a fault is live: an entry whose deadline has already passed is treated as gone even if it's still sitting in the map. That's a belt-and-braces guard, and it's why the fault can't outlive its window even if a message goes missing.

How does the responder *get* this state? The same way it gets the service map: a `watch` channel. A `watch` channel in tokio is a single-writer, many-reader broadcast of the *latest* value — readers don't get a history, they get whatever's current, which is exactly right for "the set of faults right now". The agent owns the writer (`watch::Sender<DnsFaultState>`); the responder holds a `watch::Receiver<DnsFaultState>` and reads the newest value on each query with `borrow()`. When a fault is applied, cleared, or expires, the agent rebuilds the set from its fault registry and sends it:

```rust
fn publish_dns_faults(&self) {
    let faults = self
        .fault_registry
        .iter()
        .filter(|rule| matches!(rule.fault_type, FaultType::DnsNxdomain))
        .map(|rule| (rule.target_service.clone(), rule.expires_at_ns));
    let _ = self.dns_faults_tx.send(DnsFaultState::from_faults(faults));
}
```

The resolver check itself is three lines, sitting right after the source-ACL check and before the service-map lookup:

```rust
let now_ns = crate::smoker::types::monotonic_now_ns();
if dns_faults.borrow().is_faulted(&service_id.name, now_ns) {
    return build_status_response(query, RCODE_NXDOMAIN);
}
```

The application's `getaddrinfo("redis.internal")` now fails with `EAI_NONAME`. From the application's perspective, the service simply doesn't exist — which is the whole point.

Two things fall out of this. First, `DnsNxdomain` no longer counts as an "eBPF
fault": it works wherever the responder runs, so `requires_ebpf()` returns
`false` for it. Drop and partition need the connect hook. Delay and bandwidth
need a future TC hook and are rejected on every current node. Second, reversal
is free: clearing or expiring the DNS fault removes it from the registry, we
republish the smaller set, and the name resolves again on the very next query.
No kernel map to clean up, because there never should have been one.

## Network security

Network security extends the same eBPF connect hook with egress enforcement. When an app specifies `[egress] allow = ["api.stripe.com:443"]`, only those destinations are permitted for non-VIP traffic.

The implementation uses two maps:

- `egress_enabled_map` — flags which cgroups have egress enforcement active
- `egress_map` — allowed (cgroup, destination IP, port) tuples

For non-VIP connections, the hook checks if the calling cgroup has enforcement enabled. If so, it looks up the destination in `egress_map`. Missing entry means denied:

```c
struct egress_value *ev = bpf_map_lookup_elem(&egress_map, &ek);
if (!ev || ev->action != 1)
    return 0;  /* -ECONNREFUSED: egress not allowed */
```

Egress is opt-in. Apps without `[egress]` have all egress allowed. This is backward compatible — existing deployments don't need config changes.

## Scripted chaos scenarios

For repeatable tests, faults can be defined in a TOML file:

```toml
name = "Payment cascade failure"

[[step]]
description = "Database latency spike"
fault = "delay"
target = "pg"
value = "500ms"
jitter = "200ms"
duration = "2m"

[[step]]
description = "Database drops connections"
fault = "drop"
target = "pg"
value = "25%"
start_after = "2m"
duration = "3m"
```

The executor builds a timeline, sorts by activation time, and runs each step at the right moment. A speed multiplier lets you run scenarios faster for CI:

```bash
relish fault scenario payment-cascade.toml --speed 10.0 --acknowledge
```

Dry-run mode prints the timeline without executing:

```bash
relish fault scenario payment-cascade.toml --dry-run
```

## The chaos test suite

The roadmap defines 8 chaos scenarios. Each tests a different failure mode and verifies that Smoker's safety rails and the cluster's recovery mechanisms work correctly.

1. **Kill leader mid-deploy.** Safety rails block this without `--include-leader`. With the flag, the new leader picks up the deploy from Raft state and completes it.

2. **Kill node.** Replicas are rescheduled to surviving nodes. Multi-replica apps maintain zero downtime.

3. **Drain node.** Graceful eviction: containers get SIGTERM, wait for the grace period, then SIGKILL. The scheduler places replacements before the originals stop.

4. **Kill 2 of 3 replicas.** Safety rails allow this (1 survives). The supervisor restarts both within the health timeout.

5. **Rapid leader elections.** Quorum protection prevents faulting more than `(N-1)/2` council members. The cluster stabilises after the fault expires.

6. **Node failure with volume app.** The node is "dead" but volumes are on disk. An alert fires. When the node recovers, data is intact.

7. **Resource exhaustion.** OOM kill triggers restart + recovery. CPU stress triggers degraded performance but not failure. Disk full triggers an alert and GC.

8. **Bun restart.** The fault registry is in-memory, so it's empty after restart. Containers keep running (they're OS processes, not Bun children). The agent reconnects and resumes any interrupted deploy.

Each test in `tests/chaos_smoker.rs` exercises the safety rails and registry logic that make these scenarios safe to run. The eBPF-level tests run in the Lima dev cluster via `relish dev test`.

## Now it actually breaks

Everything above describes a fault *pipeline*: parse the request, check the safety rails, record the fault in the registry, set an expiry. For a long time that pipeline had a hole in the middle. The agent recorded the fault and reported success, but nothing on the box actually changed. A `kill` fault added a row to a table and killed nothing. A partition fault claimed two nodes could no longer talk while packets flowed between them unimpeded. The tests passed, because the tests only checked the bookkeeping.

That's worse than useless. A chaos tool that lies about what it did teaches you false confidence, which is the one thing you buy chaos engineering to destroy. So the wiring step closed the hole, and the guiding rule was: **inject the fault for real, or say honestly that you can't.**

Each fault type now maps to a mechanism, and the mechanism is the truth:

- **Kill, Pause, Resume** send signals to the workload's real PIDs. The agent already knows every instance's process id through the supervisor, so a `kill` fault resolves the target service to its PIDs and delivers `SIGKILL`; pause/resume use `SIGSTOP`/`SIGCONT`. A pause now records its frozen PIDs and auto-resumes them on clear or expiry, so a paused workload can't be left wedged. These work on every platform because signals are POSIX, not Linux-specific.
- **CPU stress, memory pressure and disk-IO throttle** cap the *target* instance's cgroup — `cpu.max`, `memory.high`, `io.max` — after threading the workload's namespace/app/ordinal into the fault. The pre-limit value is saved and restored on clear or expiry. They need cgroups, so they work on Linux and return a clear "requires Linux cgroups" error elsewhere.
- **Node drain and node kill** are authenticated cluster-level operations with
  separate scheduler and transport effects. They never borrow a workload
  fault's implementation.
- **Drop and service partition** need the eBPF data path from Chapter 3.
  Without it, the API rejects them. **Delay and bandwidth** need packet-time TC
  control and are rejected even when the connect hook is loaded. A 400, not a
  fake 200.
- **DNS NXDOMAIN** acts in the userspace `.internal` responder (see above), so it needs no eBPF — it takes effect wherever the responder runs, and reverses on clear or expiry by republishing the faulted-service set.
- **Council partition** populates the real gossip/Raft transport blocklists.
- **Service partition** populates source-cgroup/VIP/port keys in
  `fault_connect_map`.

That distinction is worth dwelling on. You don't need eBPF to partition the
cluster control plane honestly. Gossip and Raft each consult a transport
blocklist; the council operation resolves peer names and inserts their
addresses there. But that says nothing about whether one workload can reach
another service VIP. The latter is a cgroup connect-map rule. Giving the two
operations distinct enum variants means the compiler, the quorum rail and the
cleanup code can no longer quietly treat them as the same fault.

### The safety context is real too

The safety rails from the top of this chapter only protect you if the numbers they read are real. The rail that guards Raft quorum asks "how many council members already have an active node-level fault?" — and for a while the answer was hardcoded to zero, which meant the rail could never fire in production. It passed its unit tests (which supply the context by hand) and did nothing in the wired path.

The agent now builds that context from live state every time a fault arrives: council size from the Raft metrics, alive-node count from the membership table, the target service's replica count from the supervisor, and the active node-level fault count from the registry. Counting node faults conservatively — treating every active partition or node-kill as if it *could* be sitting on a council member — means the quorum rail protects the worst case rather than assuming the best. On a three-member council, `max_allowed` is `(3-1)/2 = 1`: the first partition is within budget, the second is rejected with a `QuorumRisk` violation. The `fault_injection_rejected_when_quorum_at_risk` test drives two partition faults through the real API and asserts the second one comes back 4xx.

The lesson repeats one from earlier chapters: a check that always passes is worse than no check, because it looks like protection. The gap between recording a fault and injecting one is the gap between chaos engineering and vandalism — and the gap between a safety rail and a comment is whether the numbers behind it are real.

### Closing the last gaps

Wiring a pipeline honestly is one thing; wiring *every* path through it is another. A few holes survived the first pass, and each one had the same shape as the bug it lived next to: a path that looked done but quietly wasn't.

The council-level partition — `relish chaos council-partition`, which blocks a node's gossip and Raft transports — took effect but never cleaned up after itself. `chaos heal` cleared the registry and wiped the blocklists, but never ran the per-fault reversal loop, so a SIGSTOPped workload frozen by an *earlier* fault stayed frozen and a `cpu.max` cap stayed capped, with no record a fault had ever existed. Heal now runs the same reverse-each-fault loop as "clear all faults": SIGCONT the paused, restore the capped, unblock the partitioned, *then* wipe anything left over. And a partition now records its reversal — the peer ids it blocked — so both `heal` and TTL expiry unblock precisely those peers and leave any other partition in force. A Ctrl-C'd partition no longer outlives the terminal that started it.

The safety context had the subtlest gap. It was built "every time a fault arrives" — except when the node had no council, where it returned nothing and the caller skipped the rails entirely. That's backwards: the rail that stops you killing a service's last replica doesn't need a council at all, only a local replica count. So the context is now built unconditionally; with no council the quorum, leader, and node-percentage rails self-neutralise on zeroed fields, but the replica-minimum rail still fires. `fault kill --count 0` against a single-replica service is refused whether or not the node is part of a cluster. The legacy `chaos council-partition` path runs the rails too now, so a partition that would strand quorum is refused on the old API just as it is on the new one.

Last, a partial failure. A resource fault writes a cgroup limit to each replica in turn; if the third write failed, the caller dropped the fault from the registry — discarding the reversal state for the two replicas already throttled, which stayed throttled forever. The apply loop now rolls back the replicas it already changed before returning the error, so a fault that can't be applied to all of its targets is applied to none of them.

Phase 15 closed one gap we had left deliberately. A service-to-service
`Partition` fault
(`relish fault partition web --from api --acknowledge`) now refuses unless Bun
has loaded the eBPF connect path. Bun resolves the source app's live cgroup ids
itself, writes one exact source/VIP/port key per cgroup, owns those keys for
reversal and rolls back a partial write. The old quorum test moved onto the
separate `CouncilPartition` transport operation, so an eBPF-free cluster no
longer needs a pretend service partition to test Raft safety.

## Process workloads

Not everything runs in a container. Monitoring agents, log shippers, custom exporters — these are host binaries that need to run alongside your containerised apps. Until now, you'd manage them separately with systemd or supervisord. Process workloads make them first-class citizens.

Two fields in the app config:

```toml
[app.metrics-exporter]
exec = "/usr/local/bin/metrics-exporter"
command = ["--port", "9090"]
port = 9090
```

Or for inline scripts:

```toml
[job.db-backup]
script = """
#!/bin/sh
pg_dump production > /tmp/backup.sql
"""
schedule = "0 3 * * *"
```

`exec` and `script` are mutually exclusive with `image` — you either run a container or a process, not both. They're also mutually exclusive with each other. The validation logic catches this at config parse time, before anything gets deployed.

### The ProcessManager

The `ProcessManager` wraps `ProcessGrill` with two responsibilities: allowlist validation and script temp file lifecycle.

```rust
pub fn prepare_exec(&self, binary: &Path) -> Result<PreparedWorkload, ProcessWorkloadError> {
    if !self.config.is_binary_allowed(binary) {
        return Err(ProcessWorkloadError::BinaryNotAllowed {
            path: binary.to_path_buf(),
        });
    }
    Ok(PreparedWorkload { binary: binary.to_path_buf(), args: Vec::new(), temp_file: None })
}
```

For scripts, it writes the content to a temp file in a secure directory, makes it executable, and returns a workload that runs it via `/bin/sh -c`. The temp file is cleaned up after execution — success or failure.

The allowlist is configured per node:

```toml
[process_workloads]
allowed_binaries = ["/usr/local/bin/metrics-exporter", "/usr/bin/python3"]
mount_isolation = true
```

An empty list means all binaries are allowed. This is the default — opt-in restriction rather than opt-out freedom. On Linux, `mount_isolation = true` runs process workloads in a separate mount namespace so they can't see `/var/lib/reliaburger` or other workloads' volumes.

### How it fits together

Process workloads get the same treatment as containers: they appear in the service map, get VIPs and DNS names, receive health checks, and can be targeted by fault injection. The OCI spec generation detects `exec`/`script` and sets the command accordingly. ProcessGrill spawns the process. The supervisor manages its lifecycle. From the cluster's perspective, a process workload is just another app.

## Batch scheduling

The Meat scheduler's Filter→Score→Select→Commit pipeline evaluates every node for every placement. That's the right trade-off for long-running apps where quality of placement matters — you want the best node, not just any node. But for batch jobs (short-lived, many identical instances), you need throughput.

One hundred thousand jobs. One hundred nodes. Under one second.

The batch scheduler takes a different approach. Instead of evaluating each job individually, it groups jobs by resource profile (identical CPU/memory/GPU requirements) and bin-packs each group in bulk:

```rust
pub fn schedule_batch(
    jobs: &[BatchJob],
    nodes: &mut [NodeCapacity],
) -> BatchAllocation {
    // Group jobs by resource profile
    let mut profile_groups: BTreeMap<ResourceProfile, Vec<&BatchJob>> = BTreeMap::new();
    for job in jobs {
        let profile = ResourceProfile::from(&job.resources);
        profile_groups.entry(profile).or_default().push(job);
    }
    // ...
}
```

For each profile group, the scheduler sorts nodes by available capacity (most room first), then greedily assigns as many jobs as will fit on each node before moving to the next. The `jobs_that_fit` function divides available resources by the job's requirements — pure integer arithmetic, no I/O. The groups live in a `BTreeMap` rather than a `HashMap` for one reason: `HashMap` iteration order is random per process, and the groups are processed in order, so the same submission would produce a different assignment plan on every run. Ordered keys make the plan reproducible (a test in Chapter 12 pins it).

The complexity is O(nodes × profiles + total_jobs). If you have 100 nodes and all jobs are identical (1 profile), it's O(100 + 100,000) — essentially linear in the number of jobs. Even with 100 different profiles, it's O(10,000 + 100,000). The per-job pipeline would be O(100 × 100,000) — ten million evaluations.

The `BatchTracker` handles the async side. Submission returns immediately with a `BatchId`. The tracker records which jobs went to which nodes and updates their status as completion reports arrive via the reporting tree. You can poll `summary(batch_id)` to see how many are done:

```rust
pub struct BatchSummary {
    pub batch_id: u64,
    pub total: usize,
    pub pending: usize,
    pub completed: usize,
    pub failed: usize,
    pub unschedulable: usize,
    pub done: bool,
    pub elapsed_secs: u64,
}
```

(The `unschedulable` count arrived later, in Chapter 12's durability work — jobs the scheduler couldn't place used to be silently omitted from the batch; now they're part of its story.)

The 100K-in-<1s benchmark runs as a unit test on every build. If someone introduces a regression that makes scheduling slower, the test fails immediately.

## Build jobs

The final piece of the infrastructure puzzle: building images inside the cluster. No more pushing from your laptop to a remote registry, then pulling from the registry to the cluster. Build where the images will run.

### A complete example

Say you have a Python API. The source tree looks like this:

```
my-api/
  Dockerfile
  requirements.txt
  app.py
  tests/
    test_app.py
```

The Dockerfile is standard:

```dockerfile
FROM python:3.12-slim
WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY app.py .
EXPOSE 8080
CMD ["python", "app.py"]
```

To build this inside the cluster and push it to Pickle, you write a build config:

```toml
[build.my-api]
context = "./my-api"
dockerfile = "Dockerfile"
destination = "pickle://my-api:v1.2.3"
namespace = "production"
platform = ["linux/amd64", "linux/arm64"]
```

That's it. `context` is the directory containing your source. Everything inside it gets sent to the builder. `dockerfile` defaults to `"Dockerfile"` if you leave it out. `destination` uses the `pickle://` protocol, which means "push to the local Pickle registry on this cluster". `platform` defaults to both amd64 and arm64, so the image works on mixed-architecture clusters.

You can pass build arguments too:

```toml
[build.my-api.args]
PIP_INDEX_URL = "https://internal-pypi.corp.example.com/simple"
APP_VERSION = "1.2.3"
```

These become `--build-arg` flags, which your Dockerfile picks up with `ARG`:

```dockerfile
ARG PIP_INDEX_URL
ARG APP_VERSION
RUN pip install --index-url ${PIP_INDEX_URL} -r requirements.txt
```

Once the build completes, deploy the image like any other app:

```toml
[app.my-api]
image = "my-api:v1.2.3"
port = 8080
replicas = 3

[app.my-api.health]
path = "/healthz"
```

Pickle resolves `my-api:v1.2.3` locally — no Docker Hub round-trip. The scheduler pulls the image from whichever Pickle node has it cached (or replicates it if needed).

The `pickle://` protocol is enforced at config validation time — you can't accidentally push to Docker Hub or a remote registry from a build job.

### Choosing a builder

We need something that can build OCI images from Dockerfiles without a Docker daemon. We looked at six options:

**kaniko** (Google) was the obvious choice two years ago. Every Kubernetes CI tutorial recommended it. Then Google archived it in mid-2025. The repo is frozen, no more releases, no security patches. If you're still using it, you're running on borrowed time.

**BuildKit** (Docker/Moby) is the most powerful option. It parallelises layer builds, supports build secrets, SSH forwarding, multi-platform builds. But it's a client-server architecture: you run `buildkitd` as a daemon and talk to it via `buildctl`. For in-cluster builds, you either manage buildkitd as a long-lived service (another stateful component to babysit) or use the "daemonless" wrapper where buildkitd starts, builds, and exits in a single container. Either way, more moving parts than we want.

**img** (Jessie Frazelle) was a thin wrapper around BuildKit for unprivileged builds. Abandoned in 2020. Superseded by BuildKit's own rootless mode.

**ko** (Google) is excellent if your workload is exclusively Go. It compiles Go binaries and assembles OCI images in pure userspace. But it doesn't process Dockerfiles. Not general-purpose.

**Cloud Native Buildpacks** auto-detect your language and build without a Dockerfile. Different paradigm entirely. Good for PaaS-style "push your code" workflows, but we want Dockerfile support.

**buildah** (Red Hat/Podman ecosystem) is a single binary that runs, builds, and exits. No daemon. No background process. No client-server split. `buildah bud` builds from a Dockerfile, `buildah push` pushes to any OCI-compliant registry. With `--storage-driver vfs`, it works in a completely unprivileged container — no FUSE, no special kernel modules. VFS is slower than overlayfs (it copies instead of overlaying), but for a build job that completes and exits, speed matters less than simplicity.

Can you see where this is going? We went with buildah.

### The full flow

Here's what happens, step by step, when you submit a build job.

**1. The CLI tars the build context and uploads it to Pickle.** The context directory (containing your Dockerfile, source, requirements.txt, etc.) is packed into a tar archive, hashed, and uploaded to Pickle's blob store as a regular blob. This is the key insight: Pickle is already a content-addressed blob store that every node in the cluster can talk to. We don't need a shared filesystem or a separate file transfer mechanism.

```rust
pub fn tar_context(context_dir: &Path) -> Result<Vec<u8>, BuildError> {
    let mut archive = Vec::new();
    let mut tar = tar::Builder::new(&mut archive);
    tar.append_dir_all(".", context_dir)?;
    tar.finish()?;
    Ok(archive)
}
```

The digest of the tar becomes the context's identity. If two builds use the same context, the blob is already there — no re-upload needed.

**2. The CLI submits a build job with the context digest.** The leader schedules it to a node like any other job. The build node doesn't need access to your local filesystem. It just needs to reach Pickle, which it already does.

**3. The build node downloads the context blob from Pickle.** It fetches the tar from Pickle's OCI blob endpoint (`GET /v2/_buildcontext/blobs/sha256:...`), extracts it to a temp directory, and now has everything buildah needs.

**4. Buildah builds the image.** The build node runs buildah as a subprocess:

```
buildah bud --storage-driver vfs -f Dockerfile \
  --platform linux/amd64,linux/arm64 \
  --manifest localhost:9117/my-api:v1.2.3 .
```

`--storage-driver vfs` tells buildah to use plain file copies instead of overlayfs. No kernel modules, no FUSE, no privileged access. Slower than overlay, but works anywhere without special permissions. Buildah reads the Dockerfile, pulls base images (e.g. `python:3.12-slim` from Docker Hub), runs each instruction, and produces an OCI image. With `--platform linux/amd64,linux/arm64`, it builds for both architectures and creates a manifest list (OCI index) so the image works on mixed clusters.

**5. Buildah pushes the image back to Pickle.**

```
buildah push --storage-driver vfs --tls-verify=false \
  localhost:9117/my-api:v1.2.3 docker://localhost:9117/my-api:v1.2.3
```

Pickle's OCI Distribution API lives on the same port as the Bun agent (9117). Buildah speaks the standard Docker registry protocol: it uploads layer blobs via `POST /v2/{name}/blobs/uploads/`, then pushes the manifest via `PUT /v2/{name}/manifests/{tag}`. For multi-platform builds, it pushes each per-architecture manifest first, then the manifest list. Pickle already handles both.

`--tls-verify=false` because Pickle runs on localhost. No point doing TLS to yourself.

**6. The image is ready.** `relish images` shows it. Other nodes pull it via Pickle's replication protocol. When you deploy with `image = "my-api:v1.2.3"`, the scheduler sees which nodes already have the layers cached (image locality scoring from Phase 2) and prefers them.

The entire flow uses two existing pieces of infrastructure: Pickle for blob storage and transfer, buildah for Dockerfile execution. No new daemons, no shared filesystems, no scp.

### Dependencies

Buildah is the only external dependency for build jobs. It's not bundled into the Bun binary — it's installed on the host, like runc.

On Ubuntu/Debian:
```
apt-get install -y buildah
```

On Fedora/RHEL:
```
dnf install -y buildah
```

The `relish dev create` command installs it automatically when provisioning Lima VMs. If you're setting up a production cluster manually, add it to your node image alongside runc.

### Namespace-scoped pushes

If the image name contains a slash (`pickle://production/myapp:v1`), the prefix is treated as a namespace scope. A build in namespace "staging" can't push to `production/myapp`:

```rust
if let Some(build_ns) = &spec.namespace
    && ns_prefix != build_ns
{
    return Err(BuildError::NamespaceMismatch { ... });
}
```

No prefix means the build can push anywhere — fine for shared infrastructure images. Layer caching is deferred to Phase 12.

## Lessons learned

Every chapter is supposed to end with what was tricky, what clicked, and what we'd do differently. This chapter had more of those moments than most.

**Safety rails must be non-overridable.** We initially considered making all
four rails overridable with `--force`. Then we thought about what happens when
someone runs
`relish fault kill web --count 0 --override-safety --acknowledge` at 3am and
takes down every replica of the payments service. Quorum protection and
replica minimum are now hard limits. No flag, no escape hatch, no "but I
really need to". If you want to test total failure, use the in-memory test
harness where there's nothing real to break.

**`#[repr(C)]` alignment is invisible until it isn't.** We wrote the BPF map structs, ran the size assertions, and three of them failed. The issue: a `u8` field followed by a `u64` field gets 7 bytes of implicit padding that the C compiler inserts for alignment. The Rust compiler does the same thing with `#[repr(C)]`, but if you don't add explicit `_pad` fields, the size assertion catches the mismatch. We reorganised the structs to pack small fields together (action + probability as two adjacent `u8`s, followed by `_pad: [u8; 6]`, then the `u64`s). The size assertion pattern — test it on every platform, catch it before any BPF code runs — saved us from a class of bugs that would have been hellish to debug at runtime.

**`BPF_MAP_TYPE_PERCPU_ARRAY` isn't universal.** Our first attempt used a per-CPU array for the PRNG state. It failed to create on the Lima test VM's kernel. We replaced it with `bpf_get_prandom_u32()`, a BPF helper available since Linux 4.1. Simpler, more portable, and equally suitable for probabilistic fault injection. The lesson: don't reach for the fancier BPF map type when a helper function does the same job.

**kaniko died while we were designing.** We spent time evaluating kaniko as the build backend. Then Google archived it. The software industry doesn't owe you backward compatibility. We switched to buildah — daemonless, rootless, actively maintained, and it speaks the same OCI registry protocol. The transition was painless because we'd already designed the build system around a clean subprocess interface: prepare args, spawn, done. If buildah dies too, we swap the binary. The lesson: minimise your coupling surface to external tools. Two subprocess calls and a standard protocol are better than a deep SDK integration.

**A chaos tool that lies is worse than no chaos tool.** This one took a while to land properly, because the bugs were all in the "close enough" category.

`fault partition` returned `Ok(())` and blocked nothing. `chaos heal` cleared the registry without reversing anything, so a SIGSTOPped workload stayed frozen and cgroup caps outlived their fault. `CpuStress` accepted a `--cores` argument, ignored it, and computed its quota as though the target had exactly one core — on a four-core node "80% stress" quietly meant "you now have 20% of one core", a 95% cut.

Take that last one seriously for a moment. Every individual piece works: the flag parses, the cgroup write succeeds, the workload really does slow down, the fault really does clear. Run the experiment and you get a plausible result. You just get it for a blast radius nobody chose, and you write down the number you asked for rather than the number you applied.

That's the specific danger of tooling whose entire purpose is producing evidence. A monitoring bug shows up as a gap in a graph. A chaos bug shows up as *confidence* — you tested it, it held, ship it. The failure is silent by construction, because the thing that would have told you is the thing that's broken.

So the quota now scales by the cores it's told to stress, and `None` means "as many cores as the workload actually has", derived from its own `cpu.max`:

```rust
pub fn baseline_cores(current_cpu_max: &str, host_cores: u32) -> u32 {
    let mut parts = current_cpu_max.split_whitespace();
    let (Some(quota), Some(period)) = (parts.next(), parts.next()) else {
        return 1;
    };
    if quota == "max" {
        return host_cores.max(1);
    }
    match (quota.parse::<u64>(), period.parse::<u64>()) {
        (Ok(quota), Ok(period)) if period > 0 => ((quota / period) as u32).max(1),
        _ => 1,
    }
}
```

Note which way the fallback leans. An unparseable `cpu.max` gives one core, which *under*-reads the baseline and makes the fault harsher than asked. That's deliberate. Between "your experiment was slightly more brutal than you specified" and "your experiment was much gentler than you specified and passed", only one of them lets a real weakness through.

**Pickle is a better transport than you'd think.** The build context upload problem seemed like it needed a file transfer mechanism — scp, NFS, a custom sync protocol. Then we realised we already had a content-addressed blob store that every node in the cluster can talk to. Tar the context, upload it as a blob, let the build node download it. No shared filesystems, no new infrastructure. Sometimes the best solution is the one you've already built for a different purpose.

## Tests

A chaos tool poses an awkward testing question: how do you test something whose job is to break things, without breaking your test runner? The answer runs right through this chapter's design. The *logic* — safety rails, the registry, scheduling — is pure and tested in-memory, so it runs anywhere. The *kernel enforcement* — eBPF actually dropping a `connect()` — is gated behind the same eBPF machinery as Chapter 3.

### Unit tests — the logic

The 91 tests in `src/smoker/` cover the parts that must never be wrong:

- **Safety rails** — one test per `SafetyViolation` variant, plus the evaluation-order test (quorum reported before leader when both trip). These are the tests that let us promise quorum protection can't be bypassed.
- **The registry** — expiry via the min-heap, and the property that a `bun` restart leaves it empty.
- **`#[repr(C)]` size assertions** — `connect_fault_key_size` and friends, which run on *every* platform and catch a padding mistake before any BPF code loads.

The batch scheduler and build pieces add their own: the 100K-jobs-in-under-a-second benchmark runs *as a unit test* (a regression that slows scheduling fails the build), and the build tests cover `pickle://` enforcement, namespace-scope rejection, and buildah argument construction.

### Integration tests — the scenarios, in memory

The eight chaos scenarios live in `tests/chaos_smoker.rs`, and they're deliberately built on in-memory infrastructure so they run on a laptop with no eBPF host:

```
kill_leader_blocked_without_flag      rapid_elections_quorum_protection
kill_leader_allowed_with_flag         oom_kill_blocked_for_all_replicas
kill_non_leader_node_approved         cpu_stress_allowed_no_replica_check
drain_node_tracked_in_registry        registry_cleared_on_restart
```

`tests/chaos.rs` complements them with cluster-recovery-from-partition tests (carried over from Chapter 2), also in-memory and deterministic. Neither needs eBPF, because what they're testing is the decision-making — "would this fault be allowed, and is it tracked correctly?" — not the kernel mechanism.

### Gated tests — the kernel actually dropping packets

Proving that an eBPF connect fault *really* returns `ECONNREFUSED` needs a real kernel. Those tests ride along with Chapter 3's eBPF suite, behind both the `ebpf` feature and the env var:

```sh
RELIABURGER_EBPF_TESTS=1 cargo test --features ebpf --test ebpf
```

On a Mac, `relish dev test` runs them inside Lima. Process faults (signals) run on any Unix including macOS; resource faults (cgroups) are Linux-only and their config logic is unit-tested everywhere, returning `UnsupportedPlatform` off Linux.

### Running them

```sh
cargo test --lib smoker meat::batch         # safety rails, registry, batch
cargo test --test chaos_smoker               # the 8 scenarios (in-memory)
cargo test --test chaos                       # partition recovery
relish dev test onion                         # eBPF fault enforcement (Lima)
```

Phase 8 adds 222 tests, bringing the total to 1263.

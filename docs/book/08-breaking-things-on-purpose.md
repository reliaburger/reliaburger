# Breaking Things on Purpose

In 2012, Netflix deployed Chaos Monkey to production. It randomly killed instances in their AWS fleet. Engineers thought this was insane. Within a year, every team at Netflix had hardened their services against instance failure. The practice spread. Today we call it chaos engineering.

The idea is simple: if you don't know how your system fails, you're going to find out at 3am on a Saturday. Better to find out on a Tuesday afternoon, on your terms, with a rollback plan.

Most chaos engineering tools are separate systems. Chaos Mesh needs CRDs, an operator, and a privileged DaemonSet. Litmus spawns runner pods for each experiment. Gremlin is a SaaS with a privileged agent on every node. The barrier to entry is high enough that most teams never adopt them.

Smoker takes a different approach. It's built into Reliaburger. No extra binaries, no sidecars, no CRDs. When no faults are active, the overhead is a single empty hash map lookup per `connect()` call — about 50 nanoseconds. When you want to break something, it's one command: `relish fault delay redis 200ms`.

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

**Replica minimum** prevents you from killing all instances of a service. `relish fault kill web --count 0` (kill all) is rejected if it would leave zero surviving replicas. At least one must survive.

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

1. **Userspace expiry.** Every health tick (1 second), the agent calls `drain_expired()`, which pops entries from the min-heap and removes them from the registry.

2. **eBPF-level expiry.** For network faults, the BPF programs check `bpf_ktime_get_ns()` against the entry's `expires_ns` field on every `connect()`. Even if the userspace timer is delayed, the kernel stops applying the fault at the right time.

The registry is wrapped in `Arc<tokio::sync::Mutex<FaultRegistry>>` because the agent event loop and the expiry background task both need access. We use `tokio::sync::Mutex`, not `std::sync::Mutex`. In async code, a standard mutex can block the tokio runtime if the lock is held across an `.await` point. The tokio mutex yields instead.

## Process faults: the easy ones

The simplest faults are process signals. `relish fault kill web-3` sends SIGKILL to the container's main process. `relish fault pause web` sends SIGSTOP, which freezes the process. Health checks fail after the configured timeout, triggering the restart logic. `--resume` sends SIGCONT.

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

Resource faults use the same cgroup hierarchy that Bun already manages for container isolation. CPU stress spawns a burn loop inside the target's cgroup. Memory pressure allocates and `mlock`s pages. Disk I/O throttle writes to the `io.max` cgroup file.

The CPU burn loop is a tight arithmetic loop with `std::hint::black_box` to prevent the compiler from optimising it away:

```rust
pub struct CpuBurnConfig {
    pub percentage: u8,
    pub cores: Option<u32>,
    pub window_us: u64,  // 10ms default
}
```

For 50% CPU stress, each thread burns for 5ms and sleeps for 5ms in a 10ms window. Because the burn process runs inside the same cgroup as the application, they compete for CPU time exactly as a real noisy-neighbour workload would.

Memory pressure uses `mmap` + `mlock`. We allocate anonymous pages inside the target's memory cgroup and lock them so the kernel can't reclaim them. At 90%, the application has only 10% of its headroom remaining. This triggers the same kernel memory pressure signals (PSI, `memory.high` events) as real contention.

Disk I/O throttle uses cgroupv2's `io.max` file — the kernel's native I/O throttling:

```rust
let value = format!("{device_major_minor} rbps={bytes_per_sec} wbps={bytes_per_sec}");
std::fs::write(&io_max_path, value.as_bytes())?;
```

One write to a cgroup file. The kernel handles everything else.

All resource faults are Linux-only. On macOS, the functions return `ResourceFaultError::UnsupportedPlatform`. The unit tests for configuration logic (burn durations, pressure calculations) run everywhere.

## Network faults: eBPF

This is where Smoker earns its keep. Network faults operate at the kernel level, in the same eBPF programs that Onion uses for service discovery.

We add four new BPF maps alongside Onion's existing maps:

- `fault_connect_map` — per-service connection faults (drop, delay, partition)
- `fault_dns_map` — per-service DNS faults (NXDOMAIN)
- `fault_bw_map` — per-service bandwidth throttling
- `fault_state_map` — per-CPU PRNG state for probabilistic faults

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

A partition between service A and service B uses the `source_cgroup_id` field in the key. The eBPF program checks `bpf_get_current_cgroup_id()` against the key. If the calling process is in the blocked cgroup and the destination matches, the connection is refused with ENETUNREACH.

Bidirectional partitions require two map entries (A→B and B→A). Unidirectional partitions are one entry — A can't reach B, but B can still reach A.

### DNS NXDOMAIN

The DNS interception hook checks `fault_dns_map` before the normal `dns_map` lookup. If the service name has a fault entry of type NXDOMAIN, the application's `getaddrinfo()` call fails with `EAI_NONAME`. From the application's perspective, the service simply doesn't exist.

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
relish fault scenario payment-cascade.toml --speed 10.0
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
    let mut profile_groups: HashMap<ResourceProfile, Vec<&BatchJob>> = HashMap::new();
    for job in jobs {
        let profile = ResourceProfile::from(&job.resources);
        profile_groups.entry(profile).or_default().push(job);
    }
    // ...
}
```

For each profile group, the scheduler sorts nodes by available capacity (most room first), then greedily assigns as many jobs as will fit on each node before moving to the next. The `jobs_that_fit` function divides available resources by the job's requirements — pure integer arithmetic, no I/O.

The complexity is O(nodes × profiles + total_jobs). If you have 100 nodes and all jobs are identical (1 profile), it's O(100 + 100,000) — essentially linear in the number of jobs. Even with 100 different profiles, it's O(10,000 + 100,000). The per-job pipeline would be O(100 × 100,000) — ten million evaluations.

The `BatchTracker` handles the async side. Submission returns immediately with a `BatchId`. The tracker records which jobs went to which nodes and updates their status as completion reports arrive via the reporting tree. You can poll `summary(batch_id)` to see how many are done:

```rust
pub struct BatchSummary {
    pub batch_id: u64,
    pub total: usize,
    pub pending: usize,
    pub completed: usize,
    pub failed: usize,
    pub done: bool,
    pub elapsed_secs: u64,
}
```

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

**1. The CLI reads your build config** and validates it. The `pickle://` destination is parsed, the context directory is checked, and mutual exclusivity rules are enforced (you can't have both `image` and `context` on the same app). If anything is wrong, you get an error before any work happens.

**2. The leader picks a node to run the build.** Build jobs are scheduled like any other job — the Meat scheduler picks a node with enough CPU and memory. The build context directory must be accessible on that node (via the shared mount or a volume). In a dev cluster, Lima's `virtiofs` mount handles this. In production, you'd use a shared filesystem or sync the context to the node first.

**3. Bun on the target node runs buildah as a subprocess.** This is where the actual Dockerfile execution happens. Buildah needs to be installed on every node that might run builds — it's a single binary, installed via `apt-get install buildah` (Ubuntu/Debian) or `dnf install buildah` (Fedora/RHEL). The dev cluster provisioning installs it automatically.

The build command:

```
buildah bud --storage-driver vfs -f Dockerfile \
  --platform linux/amd64,linux/arm64 \
  --manifest localhost:9117/my-api:v1.2.3 ./my-api
```

`--storage-driver vfs` is the key flag. It tells buildah to use plain file copies instead of overlayfs. This means no kernel modules, no FUSE, no privileged access. Slower than overlay, but works anywhere. Buildah reads the Dockerfile, pulls base images (e.g. `python:3.12-slim` from Docker Hub), runs each instruction, and produces an OCI image. With `--platform linux/amd64,linux/arm64`, it builds for both architectures and creates a manifest list (OCI index) so the image works on mixed clusters.

**4. Buildah pushes to Pickle.** The second subprocess call:

```
buildah push --storage-driver vfs --tls-verify=false \
  localhost:9117/my-api:v1.2.3 docker://localhost:9117/my-api:v1.2.3
```

Pickle's OCI Distribution API lives on the same port as the Bun agent (9117). Buildah speaks the standard Docker registry protocol: it uploads layer blobs via `POST /v2/{name}/blobs/uploads/`, then pushes the manifest via `PUT /v2/{name}/manifests/{tag}`. For multi-platform builds, it pushes each per-architecture manifest first, then the manifest list. Pickle handles all of this — we verified it supports both single-platform manifests and manifest lists.

`--tls-verify=false` because Pickle runs on localhost. No point doing TLS to yourself.

**5. The image is now in Pickle.** `relish images` shows it. Other nodes can pull it via Pickle's replication protocol. When you deploy an app with `image = "my-api:v1.2.3"`, the scheduler sees which nodes already have the layers cached (image locality scoring from Phase 2) and prefers them.

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

No prefix means the build can push anywhere — fine for shared infrastructure images. Layer caching is deferred to Phase 9.

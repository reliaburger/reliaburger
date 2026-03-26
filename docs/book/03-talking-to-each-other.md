# Talking to Each Other

In Chapter 2, our containers found friends — nodes gossip, elect a council, and the scheduler places workloads across the cluster. But every container still shares the host's network stack. That's like putting everyone in the same room and hoping they don't shout over each other.

Phase 3 gives each container its own private network, its own IP address, and a way for other containers to find it. By the end of this chapter, our containers will talk to each other across the cluster without knowing (or caring) where anyone physically lives.

We'll do this in three steps. First, per-container network namespaces — giving every container its own network stack. Then Onion, our eBPF-based service discovery that lets containers find each other by name. And finally Wrapper, the ingress proxy that routes external traffic into the cluster.

Let's start with the plumbing.

## Per-Container Network Namespaces

### Why containers need their own network

Up to now, ProcessGrill runs everything on the host's network and RuncGrill creates a new network namespace but does nothing to configure it. That means containers either share the host (convenient but chaotic) or get an empty namespace with no connectivity (useless).

What we want is simple: every container gets its own IP address. The host can reach the container, and the container can reach the outside world. We use Linux network namespaces and virtual Ethernet (veth) pairs to make this happen.

### The architecture

```
Host namespace                    Container namespace (per container)
┌─────────────────┐              ┌─────────────────┐
│                 │   veth pair  │                 │
│  veth-{id}-h ←──────────────────→ eth0          │
│  10.x.y.1/23    │              │  10.x.y.C/23    │
│                 │              │                 │
│  nftables DNAT  │              │  default route  │
│  host:P → C:P   │              │  → 10.x.y.1     │
└─────────────────┘              └─────────────────┘
```

Each node gets a `/23` subnet from the `10.0.0.0/8` private range. A /23 gives 510 usable host addresses (enough for 500 containers per node), and we have room for 32,768 /23 blocks in the /8 space (enough for 10k+ nodes). Node N's block starts at `10.{(N*2) >> 8}.{(N*2) & 0xFF}.0/23`. So node 0 gets `10.0.0.0/23`, node 1 gets `10.0.2.0/23`, and node 5000 gets `10.39.16.0/23`.

Why /23 and not /24? A /24 only has 254 usable addresses. In a busy cluster, 500 pods on a single node isn't unusual. A /23 doubles that to 510, which covers the target with a bit of headroom.

The gateway sits at the first usable address in the block, and containers start at gateway + 1. A veth pair — think of it as a virtual cable with a plug on each end — connects the container to the host. One end (`eth0`) lives inside the container's namespace, the other (`veth-{id}-h`) lives on the host.

### Three strategies for three runtimes

Not every runtime needs the same approach:

**RuncGrill (root mode):** Full namespace isolation. We create the namespace, set up the veth pair, assign IPs, configure the default route, and use nftables for port mapping. This is the real deal.

**RuncGrill (rootless mode):** Uses `slirp4netns`, the same tool Podman relies on. It creates a TAP device inside the user namespace with a userspace TCP/IP stack. No root needed. Port forwarding goes through its API socket.

**AppleContainerGrill (macOS):** Apple Container already runs each container in a lightweight VM with its own vmnet interface. The network isolation is free. We just need to discover the container's IP via `container inspect`.

**ProcessGrill:** No network isolation. Processes share the host network. This is the cross-platform dev/test fallback.

### Network namespaces in Rust

Here's the core struct that tracks a container's network resources:

```rust
pub struct ContainerNetwork {
    pub namespace_path: PathBuf,   // /var/run/netns/{instance_id}
    pub container_ip: Ipv4Addr,    // 10.0.N.C
    pub gateway_ip: Ipv4Addr,      // 10.0.N.1
    pub host_veth: String,         // veth-{id}-h
    pub container_veth: String,    // eth0
    pub rootless: bool,            // true = Rust proxy, false = nftables
}
```

Setting up the network is a sequence of `ip` commands. We could use the `netlink` interface directly (that's what `ip` does under the hood), but these are one-time setup operations, not hot path. Shelling out to `ip` means we can debug with `ip netns list` and `ip link show` — much easier than inspecting raw netlink messages.

The sequence:

1. Create the namespace: `ip netns add rb-{instance_id}`
2. Create the veth pair: `ip link add veth-{id}-h type veth peer name eth0`
3. Move one end into the namespace: `ip link set eth0 netns rb-{instance_id}`
4. Assign IPs to both ends
5. Bring everything up
6. Set the default route inside the namespace to point at the gateway
7. Enable IP forwarding on the host

The `rb-` prefix on namespace names avoids collisions with other tools that might create network namespaces.

### IP address calculation

The maths behind the /23 addressing is a bit more involved than a simple byte-per-field scheme. Each node's block starts at an offset of `node_index * 2` /24-blocks into the 10.0.0.0/8 space (because a /23 is two /24 blocks):

```rust
fn subnet_base(node_index: u16) -> (u8, u8) {
    let offset = (node_index as u32) * 2;
    let second_octet = (offset >> 8) as u8;
    let third_octet = (offset & 0xFF) as u8;
    (second_octet, third_octet)
}
```

Containers within a node are numbered starting from 0. The gateway takes the first address in the block, and containers start at gateway + 1:

```rust
pub fn container_ip(node_index: u16, container_index: u16) -> Ipv4Addr {
    let (oct2, oct3) = subnet_base(node_index);
    let host_offset = (container_index as u32) + 2;
    let third = oct3.wrapping_add((host_offset >> 8) as u8);
    let fourth = (host_offset & 0xFF) as u8;
    Ipv4Addr::new(10, oct2, third, fourth)
}
```

The `wrapping_add` is intentional. In Rust, default integer arithmetic panics on overflow in debug mode, a common surprise for programmers coming from C or Go where overflow wraps silently. When we want wrapping behaviour (and here we do, because the /23 spans two /24 blocks, so the fourth octet legitimately wraps from one block into the next), we have to say so explicitly.

To assign each node its index, we hash the node's hostname with djb2:

```rust
pub fn node_index_from_id(node_id: &str) -> u16 {
    let hash: u32 = node_id
        .bytes()
        .fold(5381u32, |acc, b| acc.wrapping_mul(33).wrapping_add(b as u32));
    ((hash % 32_767) + 1) as u16
}
```

The `|acc, b|` syntax is a closure (Rust's lambdas). The pipes delimit the parameter list, like parentheses in `def f(acc, b):` in Python or `func(acc, b int)` in Go. The body follows directly. Short closures can be a single expression with no braces; longer ones get `{ }` just like a function. Rust infers the parameter types from context, so we don't need to annotate them.

The `fold` method is Rust's version of a reduce. We start with an accumulator (5381, the traditional djb2 seed) and combine each byte of the node ID into the running hash. djb2 isn't cryptographic, but we don't need it to be. We just need reasonable distribution across 32k buckets so that different nodes don't collide. In production, the council assigns sequential node indices on join, but the hash gives us a sensible default before the cluster is formed.

### Port mapping: two strategies

Containers have their own IPs, but external clients don't know about `10.0.N.C`. We need port mapping to forward traffic from a host port to the container.

**Root mode uses nftables**, Linux's modern packet filtering framework. We create a `reliaburger` table with a prerouting chain for DNAT (Destination Network Address Translation):

```
nft add table ip reliaburger
nft add chain ip reliaburger prerouting { type nat hook prerouting priority -100 ; }
nft add rule ip reliaburger prerouting tcp dport {host_port} dnat to {container_ip}:{service_port}
```

This is kernel-level forwarding. Zero copies, zero userspace overhead. We reuse this same nftables table later for the perimeter firewall.

**Rootless mode uses a Rust TCP proxy.** We can't touch nftables without root, so we spawn a tokio task that binds the host port and forwards connections to the container:

```rust
async fn run_tcp_proxy(
    host_port: u16,
    container_ip: Ipv4Addr,
    container_port: u16,
    shutdown: CancellationToken,
) -> Result<(), std::io::Error> {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", host_port)).await?;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accept = listener.accept() => {
                let (client, _) = accept?;
                // ... forward to container_ip:container_port
            }
        }
    }
    Ok(())
}
```

Each connection gets its own spawned task with bidirectional `tokio::io::copy`. The `CancellationToken` from `tokio_util` lets us shut down the proxy cleanly when the container is torn down — cancel the token, and the `select!` loop exits.

### Why nftables, not iptables

You might wonder why we chose nftables over the more familiar iptables. The answer is scaling.

iptables evaluates rules linearly. Every packet walks the chain from top to bottom until something matches. Ten rules? Fine. Ten thousand rules, one per container port mapping in a busy cluster? That's up to ten thousand comparisons per packet. Kubernetes clusters with large iptables rule sets have measurably higher latency and CPU usage on every node.

nftables takes a different approach. It compiles rules into a bytecode VM running in the kernel, and for certain match types it can use **sets** and **maps** — essentially hash tables or interval trees. Matching a port against a set of 10,000 ports is O(1), not O(n).

Our current code adds individual rules, one per port mapping:

```
nft add rule ip reliaburger prerouting tcp dport 30001 dnat to 10.0.2.2:8080
nft add rule ip reliaburger prerouting tcp dport 30002 dnat to 10.0.2.3:8080
# ... one per container
```

That's fine for Phase 3 where we're proving the plumbing works. But at scale, we should switch to an nftables **map** — a single rule that does an O(1) lookup:

```
nft add map ip reliaburger portmap { type inet_service : ipv4_addr . inet_service \; }
nft add element ip reliaburger portmap { 30001 : 10.0.2.2 . 8080 }
nft add element ip reliaburger portmap { 30002 : 10.0.2.3 . 8080 }
nft add rule ip reliaburger prerouting dnat to tcp dport map @portmap
```

One rule, one hash lookup per packet, regardless of how many port mappings exist.

There's another advantage that matters more in practice: nftables rule updates are **atomic**. You can replace an entire table in a single transaction. iptables serialises on a global chain lock — so if two containers start simultaneously, the second one blocks until the first finishes modifying the rules. In a cluster that's scaling up dozens of containers at once, that lock becomes a bottleneck.

That said, at real production scale, nftables only handles host-to-container port forwarding. The bulk of inter-container traffic goes through Onion's eBPF maps (which we'll build in the next section), and those are always O(1) hash lookups in the kernel. nftables handles the edge case; eBPF handles the common case.

### Wiring it into the OCI spec

The key integration point is the OCI spec. Our `standard_namespaces()` function now takes an optional namespace path:

```rust
pub fn standard_namespaces(netns_path: Option<&str>) -> Vec<OciNamespace> {
    vec![
        // ... pid, ipc, uts, mount ...
        OciNamespace {
            ns_type: "network".into(),
            path: netns_path.map(String::from),
        },
    ]
}
```

When `path` is `Some`, runc joins the pre-created namespace (where our veth is already configured) rather than creating a new empty one. When `None`, runc creates a fresh namespace — the Phase 1 behaviour.

### Rootless networking with slirp4netns

For rootless containers, we use `slirp4netns`, the same tool Podman uses. It implements a userspace TCP/IP stack via a TAP device inside the user namespace:

1. Runc creates a new network namespace (we no longer strip it from the spec)
2. After `runc create`, we get the container's PID
3. We spawn: `slirp4netns --configure --mtu=65520 --disable-host-loopback {pid} tap0`
4. The container gets IP `10.0.2.100` with gateway `10.0.2.2`
5. Port forwarding uses slirp4netns's API socket — we send JSON commands to map ports

The `--disable-host-loopback` flag is important: it prevents the container from reaching services on the host's loopback. Without it, a compromised container could probe the host's `localhost`-only services.

### Apple Container: the easy case

Apple Container runs each container in a lightweight VM with its own vmnet interface. The network isolation comes for free. We just need to discover the IP:

```rust
async fn discover_container_ip(instance: &InstanceId) -> Result<Ipv4Addr, GrillError> {
    let output = Self::container_command(&["inspect", &instance.0], instance).await?;
    let inspect: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let ip_str = inspect["NetworkSettings"]["IPAddress"].as_str()
        .ok_or_else(|| /* ... */)?;
    ip_str.parse::<Ipv4Addr>().map_err(|e| /* ... */)
}
```

After `container start`, we call `container inspect` and fish out the IP from the JSON output. The IP is stored on the `AppleEntry` and exposed via `container_ip()`.

### Testing without root

Most of the netns code needs root (or at least `CAP_NET_ADMIN`). But the pure logic — IP calculation, namespace path generation, nftables rule formatting — is testable without any privileges. The integration tests that actually create namespaces are gated behind `RELIABURGER_NETNS_TESTS=1`.

```rust
#[test]
fn container_ip_first_container_node_1() {
    // Node 1: subnet base = 10.0.2.0/23, gateway = .2.1, first container = .2.2
    let ip = container_ip(1, 0);
    assert_eq!(ip, Ipv4Addr::new(10, 0, 2, 2));
}

#[test]
fn ten_thousand_nodes_fit() {
    let gw = gateway_ip(10_000);
    assert_eq!(gw, Ipv4Addr::new(10, 78, 32, 1));
}

#[test]
fn five_hundred_containers_fit() {
    let ip = container_ip(1, 499);
    assert_eq!(ip, Ipv4Addr::new(10, 0, 3, 245));
}
```

This pattern — test the logic, gate the I/O — means `cargo test` stays fast on any developer's machine, while the full suite runs in a privileged CI environment.

## Onion: Service Discovery Without Servers

### The problem

Every container has its own IP now. But when your web frontend needs to reach Redis, it can't hardcode `10.0.2.5:30891`. That IP changes every time Redis restarts, every time the scheduler moves it to a different node. You need a name: `redis.internal`. Something that always resolves to wherever Redis is currently running.

Kubernetes solves this with CoreDNS (a real DNS server) and kube-proxy (iptables rules or IPVS for load balancing). That's three moving parts: a DNS server to operate, a proxy to configure, and a pile of iptables rules that grow linearly with the number of services. CoreDNS alone consumes 170MB of RAM in a default cluster. And if kube-proxy falls behind on rule updates, you get stale routing.

We're going to do something different. We'll intercept DNS and connections at the socket level using eBPF, before any packets are created. No DNS server process. No proxy in the data path. No iptables rules. The eBPF programs live in the kernel, so they survive Bun crashes — running applications keep connecting to backends even if the orchestrator is temporarily down.

### How it works: the 30-second version

Two eBPF programs, two steps:

1. Your app calls `getaddrinfo("redis.internal")`. The C library constructs a DNS query. Our eBPF program intercepts it *in the kernel*, looks up the name in a hash map, and injects a fake DNS response with a virtual IP: `127.128.0.3`. No DNS packet ever leaves the node.

2. Your app calls `connect(127.128.0.3, 6379)`. A second eBPF program intercepts the `connect()` syscall, looks up the VIP in another hash map, picks a healthy backend via round-robin, and rewrites the destination to `10.0.2.5:30891`. Your app's TCP connection goes directly to the backend. No proxy.

Total added latency: zero. The rewrites happen before the kernel sends any packets. Your app sees a normal TCP connection that just happens to land on the right backend.

### Virtual IPs

Each service gets a virtual IP from the `127.128.0.0/16` range. This lives within the loopback block (`127.0.0.0/8`), so it never conflicts with real network addresses. No packets with these addresses ever leave the node — the `connect()` hook rewrites them before the kernel acts on them.

The VIP is derived deterministically from the app name using SipHash:

```rust
pub struct VirtualIP(pub Ipv4Addr);

impl VirtualIP {
    pub fn from_app_name(name: &str) -> Self {
        let mut hasher = SipHasher24::new_with_keys(
            0xDEAD_BEEF_CAFE_F00D,
            0xBAAD_F00D_DEAD_BEEF,
        );
        name.hash(&mut hasher);
        let hash = hasher.finish();

        let offset = (hash % 65534) as u32 + 1;
        let ip = 0x7F80_0000u32 | (offset & 0xFFFF);
        VirtualIP(Ipv4Addr::from(ip))
    }
}
```

Same name, same VIP, every time, on every node. No coordination needed. SipHash is a keyed hash function (the `new_with_keys` call) originally designed for hash table collision resistance. We use it here because it distributes names evenly across the 65,534 available addresses with very low collision probability. It's not cryptographic, but it doesn't need to be — we just need a good spread.

The `0xDEAD_BEEF_CAFE_F00D` and `0xBAAD_F00D_DEAD_BEEF` keys are fixed seeds. They're arbitrary constants, but using the same ones on every node is what makes the VIP deterministic cluster-wide.

### The service map

Before we get to the eBPF programs, we need the data model they operate on. The `ServiceMap` is Bun's userspace record of which services exist, what their VIPs are, and where their backends live:

```rust
pub struct ServiceMap {
    entries: HashMap<String, ServiceEntry>,
}

pub struct ServiceEntry {
    pub app_name: String,
    pub namespace: String,
    pub vip: VirtualIP,
    pub port: u16,
    pub backends: Vec<BackendInstance>,
    pub firewall_allow_from: Option<Vec<String>>,
}

pub struct BackendInstance {
    pub instance_id: String,
    pub node_ip: Ipv4Addr,
    pub host_port: u16,
    pub healthy: bool,
}
```

When Bun deploys an app with a port, it calls `service_map.register_app("redis", "default", 6379, None)`. That computes the VIP and creates an entry with an empty backend list. As instances start and reach the Running state, Bun calls `add_backend()` with the real node IP and host port. When health checks fail, `set_backend_health()` flips the flag. When an app is stopped, `unregister_app()` removes everything.

On Linux, every mutation to the `ServiceMap` gets synced to the BPF hash maps in the kernel. On macOS and for ProcessGrill, the map still works — it powers `relish resolve` — but there are no eBPF programs reading it.

### The BPF maps

The eBPF programs don't call back to Bun. They read from kernel-resident hash maps that Bun populates. Three maps (plus a supplementary one for namespace isolation):

**`dns_map`**: Maps service names to VIPs. Key is a 256-byte null-terminated string (`redis.internal`), value is a 4-byte IPv4 address in network byte order. When the DNS interception program sees a `.internal` query, it looks up this map.

**`backend_map`**: Maps `(VIP, port)` pairs to backend arrays. Each entry holds up to 32 backends with their real IPs, ports, and health flags, plus a round-robin counter. When the connect hook intercepts a VIP connection, it looks up this map and picks a healthy backend.

**`firewall_map`**: Maps `(source_cgroup_id, destination_app_id)` to allow/deny. This is how we enforce namespace isolation and per-app firewall rules at the connection level.

All three are `BPF_MAP_TYPE_HASH` — kernel hash tables with O(1) lookup. The structs use `#[repr(C)]` so their memory layout matches exactly between the Rust code that writes the maps and the C eBPF code that reads them:

```rust
#[repr(C)]
pub struct BackendKey {
    pub vip: u32,     // network byte order
    pub port: u16,    // network byte order
    pub _pad: u16,
}

#[repr(C)]
pub struct BackendEndpoint {
    pub host_ip: u32,    // network byte order
    pub host_port: u16,  // network byte order
    pub healthy: u8,     // 1 or 0
    pub _pad: u8,
}
```

The `#[repr(C)]` attribute tells Rust to lay out the struct's fields in declaration order with C-compatible alignment and padding. Without it, Rust is free to reorder fields for efficiency, which would break the BPF program's assumptions about where each field lives in memory. The `_pad` fields make the alignment explicit rather than leaving it to the compiler.

### `relish resolve`: debugging service discovery

You can query the service map from the CLI:

```
$ relish resolve redis
Service:  redis
VIP:      127.128.0.3
Port:     6379
Backends: 2/2 healthy

  INSTANCE             NODE               PORT     HEALTH
  redis-0              10.0.2.2           30891    healthy
  redis-1              10.0.4.2           31022    healthy
```

This calls the Bun agent's `/v1/resolve/{name}` endpoint, which reads from the userspace `ServiceMap`. It works on all platforms, even without eBPF — useful for verifying that the service map is correct before debugging the kernel-side programs.

### Wiring the service map into the agent

The service map needs to stay in sync with reality. Four events matter:

**Deploy.** When `deploy()` processes an app with a port, it registers the service immediately — before any instances start. This creates the `ServiceEntry` with the VIP and an empty backend list. The VIP is available for DNS resolution straight away, even though there are no backends yet. A `connect()` at this point gets `ECONNREFUSED`, which is the correct answer: the service exists but isn't ready.

**Instance startup.** When `drive_instance_startup()` transitions an instance to Running (or HealthWait with no health checks), it calls `add_backend()` with the instance's real IP and host port. This is when the service actually becomes reachable. If the container has network isolation, we use its `container_ip`. For ProcessGrill on macOS, we fall back to `127.0.0.1`.

**Health transition.** The health check loop already calls `process_health_result()` and handles Running→Unhealthy and Unhealthy→Running transitions. We hook into the same spot: when the transition fires, we call `set_backend_health()` on the service map. The eBPF connect hook reads the `healthy` flag and skips unhealthy backends during round-robin selection. So a failing health check removes a backend from rotation without touching any iptables rules or proxy configuration. One byte flip in a BPF hash map, and the backend is out.

**Stop.** When `stop_app()` shuts down an app, we remove each instance from the backend list and then unregister the service entirely. After this, DNS queries for the name return nothing (pass through to upstream), and `connect()` calls to the now-stale VIP get `ECONNREFUSED`.

The ordering matters. We register the service *before* starting instances so that the DNS name resolves as early as possible. We unregister *after* stopping so that in-flight connections can drain. And we update health synchronously in the event loop so there's no window where the map disagrees with reality.

### How gossip keeps the service map consistent

Everything above describes what happens on a single node. But a 10-node cluster has 10 service maps, and they all need to agree. When the scheduler places `redis-0` on node A, node B needs to know about it too — otherwise `curl redis.internal` from a container on node B goes nowhere.

This is where the reporting tree from Chapter 2 comes back. Remember the hierarchy: worker nodes report to their assigned council member every 5 seconds, council members aggregate for the leader, and the leader pushes scheduling decisions back down. When the leader tells node A to run `redis-0`, node A starts the container, and the state change propagates through the reporting tree to every other node's Bun agent. Each agent updates its own service map and (on Linux) writes the corresponding BPF map entries.

Gossip plays a different role. Mustard doesn't carry service map data directly — that would be too much traffic for O(log N) convergence with piggybacked updates. Instead, gossip handles *failure detection*. When node A crashes, Mustard marks it as Dead within a few probe cycles (typically 2-5 seconds depending on cluster size). Every Bun agent that receives the Dead notification can then scrub node A's backends from its service map. The backends were already unreachable — the crash took them down — but scrubbing the map means new connections stop trying.

So the data flow is:

1. **Scheduling decisions** flow through the reporting tree (Raft → council → workers). This is how backend entries get *added*.
2. **Failure detection** flows through gossip (Mustard SWIM protocol). This is how backend entries get *removed* when a node dies.
3. **Health check results** are local to each node's Bun agent. They flip the `healthy` flag for backends running on that node.

Can you see why we need both? The reporting tree is accurate but slow — bounded by the 5-second reporting interval. Gossip is fast but coarse — it knows a node is dead, not which specific instance failed. Health checks are precise but local — they know exactly which instance is unhealthy, but only for instances running on the same node. Together, they cover all the failure modes: planned shutdowns (reporting tree), node crashes (gossip), and application bugs (health checks).

There's a subtlety worth noting. During a network partition, a node might be marked Dead by gossip even though it's still running containers. If those containers are serving traffic to local clients (same-node connections don't go through the network), they'll keep working fine. The service map on the partitioned node still has the local backends. Only cross-node connections are affected, which is exactly what you'd expect from a partition. When the partition heals, Mustard's incarnation counter mechanism (remember that from Chapter 2?) ensures the node rejoins cleanly and its backends reappear in everyone's service maps.

### UDP only: a deliberate limitation

Our DNS interception only handles UDP queries. DNS over TCP (used when responses exceed 512 bytes or the server sets the TC truncation flag) bypasses Onion entirely and goes to the upstream resolver.

This is safe because we control both sides. Our `.internal` names are short (under 253 characters) and our responses contain a single A record with a 4-byte VIP. There's nothing to truncate. TCP DNS fallback only triggers when responses are large — zone transfers, DNSSEC chains, many-record answers. We'll never produce those.

<!-- TODO(Phase 3): eBPF programs section (DNS interception + connect rewrite) -->
<!-- TODO(Phase 3): Wrapper ingress proxy section -->
<!-- TODO(Phase 3): nftables perimeter firewall section -->

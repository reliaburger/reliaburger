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

We're going to do something different. DNS resolution goes through a tiny UDP responder built into Bun — no separate CoreDNS to operate. And connections to backends are rewritten at the socket level by an eBPF program, before any packets are created. No proxy in the data path. No iptables rules. The eBPF connect hook lives in the kernel, so running connections survive even if Bun crashes.

### How it works: the 30-second version

Two steps, one in userspace, one in the kernel:

1. Your app calls `getaddrinfo("redis.internal")`. The C library sends a DNS query to `127.0.0.53:53` — a tiny UDP server built into Bun. Bun looks up "redis" in the service map and responds with a virtual IP: `127.128.0.3`. The query never leaves localhost. Takes about 50 microseconds.

2. Your app calls `connect(127.128.0.3, 6379)`. An eBPF program intercepts the `connect()` syscall *before the kernel sends any packets*, looks up the VIP in a hash map, picks a healthy backend via round-robin, and rewrites the destination to `10.0.2.5:30891`. Your app's TCP connection goes directly to the backend. No proxy.

The DNS lookup adds ~50 microseconds (one localhost UDP round trip). The connect rewrite adds zero — it happens before the TCP handshake. Compare that to CoreDNS over the pod network (~500 microseconds) plus kube-proxy iptables traversal. Your app sees a normal TCP connection that just happens to land on the right backend.

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

### Why userspace DNS, not eBPF?

The original design called for fully in-kernel DNS interception using `cgroup/sendmsg4` and `cgroup/recvmsg4` eBPF hooks. We tried it. It doesn't work.

The problem is that these hooks let you modify the *destination address* of a UDP sendmsg, but they can't read the *packet payload*. You can redirect where a DNS query goes, but you can't parse the query name or synthesise a response. The BPF helper you'd need (`bpf_msg_pull_data`) only works with `SK_MSG` programs (stream parsers for TCP), not cgroup socket address hooks.

So we run a userspace DNS responder instead. It's about 80 lines of code in `src/onion/dns.rs`: one `tokio::select!` loop reading from a UDP socket. Bun configures containers' `/etc/resolv.conf` to point at `127.0.0.53`, and the responder handles the rest. For `.internal` names, it looks up the service map and responds. For everything else, it forwards to the upstream resolver.

The cost is ~50 microseconds per DNS lookup (localhost UDP round trip). That's 10x faster than CoreDNS over the pod network, but it's not zero. Most applications cache DNS results anyway, so this hit happens once per connection lifetime, not per request.

The connect rewrite — the part that actually matters for latency — is still fully in-kernel eBPF. Once your app has the VIP from DNS, every `connect()` call is rewritten at zero cost.

### UDP only: a deliberate limitation

The DNS responder only handles UDP queries. DNS over TCP (used when responses exceed 512 bytes or the server sets the TC truncation flag) goes to the upstream resolver, which doesn't know about `.internal` names.

This is safe because we control both sides. Our `.internal` names are short (under 253 characters) and our responses contain a single A record with a 4-byte VIP. There's nothing to truncate. TCP DNS fallback only triggers when responses are large — zone transfers, DNSSEC chains, many-record answers. We'll never produce those.

### Testing service discovery

How do you test code that runs in the kernel? Two approaches, at different levels of fidelity.

**Unit tests on the data model.** The `ServiceMap`, `VirtualIP`, and `#[repr(C)]` types are pure Rust. We test them normally — register a service, add backends, verify resolve returns the right data. These run on any platform, no kernel required:

```rust
#[test]
fn register_and_resolve() {
    let mut map = ServiceMap::new();
    let vip = map.register_app("redis", "default", 6379, None).unwrap();
    let entry = map.resolve("redis").unwrap();
    assert_eq!(entry.vip, vip);
    assert!(entry.backends.is_empty());
}
```

**Integration tests through the agent.** We spin up a real `BunAgent` with `ProcessGrill`, deploy an app via the HTTP API, and then call `/v1/resolve/{name}` to verify the service map was populated correctly. These tests exercise the full deploy → register → add_backend → resolve flow without touching eBPF:

```rust
#[tokio::test]
async fn deploy_app_with_port_registers_in_service_map() {
    let harness = TestHarness::start().await;
    harness.client.apply(&app_with_port_config()).await.unwrap();

    let info = harness.client.resolve("redis").await.unwrap();
    assert_eq!(info.app_name, "redis");
    assert_eq!(info.port, 6379);
}
```

The `stop_app_removes_from_service_map` test verifies the other end: deploy, resolve succeeds, stop, resolve returns 404. And `vip_is_deterministic_across_agents` deploys the same app on two independent agents and verifies they assign identical VIPs — proving the deterministic hash works without any coordination.

**eBPF program tests** (Linux only, gated behind `RELIABURGER_EBPF_TESTS=1`) load real eBPF programs into a real kernel and verify the connect rewrite actually happens. This is the test that matters most:

```rust
#[tokio::test]
async fn ebpf_connect_to_vip_rewrites_destination() {
    let mut ebpf = OnionEbpf::load(&obj_dir, "/sys/fs/cgroup".as_ref()).unwrap();

    // Start a TCP listener — this is our "backend"
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_port = listener.local_addr().unwrap().port();

    // Tell the BPF map: VIP 127.128.x.y:9999 → 127.0.0.1:{backend_port}
    let vip = VirtualIP::from_app_name("test-service");
    // ... populate service map and sync to BPF ...

    // Connect to the VIP. If this succeeds, the kernel rewrote the address.
    let vip_addr = SocketAddr::new(vip.0.into(), 9999);
    let stream = TcpStream::connect_timeout(&vip_addr, Duration::from_secs(2));
    assert!(stream.is_ok());  // The eBPF program did its job
}
```

The application connects to `127.128.x.y:9999`, an address that doesn't exist anywhere. But the eBPF `connect4` hook intercepts the syscall, looks up the VIP in the `backend_map`, finds our listener at `127.0.0.1:{backend_port}`, and rewrites the destination before the TCP handshake starts. The connection succeeds. If the eBPF program weren't attached, you'd get `ECONNREFUSED` (nobody's listening on that VIP).

We also test the failure cases: connecting to a VIP with no backends returns `EPERM` (the BPF hook returns 0 to deny the syscall), and connecting to a non-VIP address passes through untouched.

One surprise: returning 0 from a `cgroup/connect4` hook gives `EPERM`, not `ECONNREFUSED`. The kernel interprets "BPF program returned 0" as "permission denied", not "connection refused". It's a subtle distinction that only matters if your application distinguishes between the two error codes. Most don't.

### Running Linux tests from a MacBook

Here's a problem we hit early: most of the interesting tests need Linux. Network namespaces, veth pairs, runc containers, eBPF programs — none of these exist on macOS. You could push to CI and wait, but that's a slow feedback loop when you're debugging a failing test.

Our solution: `relish dev test`. One command that runs all the Linux-gated tests inside a Lima VM on your Mac. If you've been following along, you already have the `relish` binary — it's `cargo run --bin relish` or just `relish` if you've added `target/debug` to your PATH.

```
$ relish dev test              # run everything
$ relish dev test netns        # just the netns tests
$ relish dev test onion        # just the onion tests
```

The first run takes a couple of minutes — it downloads an Ubuntu VM image, installs Rust, runc, slirp4netns, and clang. After that, the VM persists on disk. Subsequent runs go straight to `cargo test`.

The trick is that Lima mounts your home directory into the VM with read-write access. The repo isn't copied — it's the same files. When you edit code on your Mac and run `relish dev test`, the VM compiles your latest changes. The cargo cache and target directory also persist inside the VM, so incremental builds are fast.

Under the hood, `relish dev test` does three things:

1. Creates a Lima VM named `reliaburger-test` if it doesn't exist (4 CPUs, 4GB RAM, Ubuntu Noble).
2. Starts the VM if it's stopped.
3. Runs `limactl shell reliaburger-test bash -c "cd /path/to/repo && cargo test"` with the Linux test env vars set (`RELIABURGER_RUNC_TESTS=1`, `RELIABURGER_NETNS_TESTS=1`).

The VM provisioning script installs everything the tests need:

```yaml
provision:
  - mode: system
    script: |
      apt-get install -y runc uidmap slirp4netns curl build-essential pkg-config libssl-dev clang llvm
  - mode: user
    script: |
      curl https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
```

This is the same idea as `relish dev create` for dev clusters, but focused on testing rather than running a cluster. You don't need three VMs to run `cargo test` — one is enough.

Why not Docker? Two reasons. First, building Rust inside a Docker container on macOS means either bind-mounting the target directory (slow due to virtiofs overhead for the thousands of small files in a Rust build) or keeping it inside the container (losing it on every rebuild). Lima's VM mount is faster because the VM runs a real Linux kernel with a real filesystem. Second, we need to test network namespaces and cgroup operations, which require privileges that Docker-in-Docker handles poorly.

### What we built

Let's step back and see what Onion gives us.

A container calls `getaddrinfo("redis.internal")`. Bun's built-in DNS responder looks up the service map and responds with a virtual IP — one localhost UDP round trip, about 50 microseconds. The container calls `connect()` with that VIP. An eBPF program intercepts the syscall, picks a healthy backend via round-robin from a kernel hash map, and rewrites the destination. The TCP handshake goes directly to the backend. No proxy in the data path, no iptables rules.

Kubernetes needs CoreDNS (a separate Go binary consuming 170MB of RAM), kube-proxy (thousands of iptables rules, O(n) per packet), and often a service mesh sidecar (Envoy, consuming 50-100MB per pod). We need a single binary with a built-in DNS responder, one eBPF program, and a hash map. The eBPF connect hook persists in the kernel — if Bun crashes, running connections keep working. DNS resolution pauses (since Bun runs the responder), but that only affects new connections. Existing TCP sessions are fine.

We originally planned to do DNS entirely in-kernel too. It turned out the BPF hooks we needed can't read DNS packet payloads. So we went with the pragmatic approach: userspace DNS at 50 microseconds, in-kernel connect rewrite at zero. Pragmatism over purity. The 50 microsecond DNS hit is invisible to any real application, and it saved us from fighting kernel limitations that would have taken weeks to work around (if they're even solvable with current BPF).

## Wrapper: The Front Door

Onion handles traffic inside the cluster. But when a browser hits `myapp.com`, that traffic comes from the internet. It can't use VIPs or eBPF hooks — it needs a real port to connect to. Wrapper is the reverse proxy that receives external traffic on ports 80 and 443 and routes it to the right backend.

### Why not just expose the containers directly?

Each container gets a dynamically allocated host port (30000-31000). You could map DNS to `node-ip:30891` and call it a day. Three problems:

1. The port changes every time the container restarts or moves to a different node.
2. You'd need one DNS record per container per app. With 10 replicas across 5 nodes, that's 50 records to manage.
3. No TLS termination, no load balancing, no health-aware routing.

A reverse proxy solves all three. External clients talk to `myapp.com:443`, and Wrapper figures out where the traffic should go.

### Architecture

Wrapper runs inside Bun as a set of async tasks — not a separate process. But it runs on its own tokio runtime with its own thread pool. This is the key design decision: if someone points a botnet at port 80, the flood of connections saturates Wrapper's threads but can't starve the gossip protocol, the Raft consensus, the health checker, or the scheduler. Resource isolation through separate runtimes.

On top of that, a concurrent connection limit (default 10,000) rejects new connections with 503 once the cap is hit. So a DDoS attacker faces: per-IP rate limiting, a global connection ceiling, and runtime isolation that protects the rest of the system.

### The routing table

The routing table maps `(host, path)` pairs to backend pools:

```rust
pub struct RoutingTable {
    routes: HashMap<String, Vec<PathRoute>>,
}
```

Each host maps to a list of path routes, sorted by path length descending. When a request arrives, we extract the `Host` header, find the matching host (case-insensitive), then walk the path routes looking for the first prefix match. Longest prefix wins — `/api/v1` matches before `/api`, which matches before `/`.

The table is rebuilt from the `ServiceMap` whenever apps with ingress config are deployed, stopped, or have health changes. Rebuilding is cheap (microseconds for typical clusters) and writes are behind a `RwLock`. In-flight requests hold a read lock and are never blocked by a rebuild.

### What happens when things go wrong

Three error codes tell the client exactly what happened:

- **404 Not Found**: No route matches the `Host` header. The request is for a domain we don't know about.
- **502 Bad Gateway**: A route matches, but all backends are unhealthy. The app is deployed but broken.
- **503 Service Unavailable**: The connection limit was reached. We're overloaded.

### Connection draining

When an app is being redeployed (rolling update), the old instances need to finish serving in-flight requests before they're stopped. This is the drain protocol:

1. Bun tells Wrapper: "drain instance web-0, deadline 30 seconds"
2. Wrapper moves the backend from the active pool to a draining pool — no new requests go to it
3. In-flight requests complete normally
4. When all connections are done (or the 30-second deadline hits), Wrapper tells Bun: "drain complete"
5. Bun stops the old container

The app never drops below its replica count during a deploy. If you have 3 replicas and `max_surge = 1`, the sequence is: start replica 4, drain replica 1, start replica 4', drain replica 2, and so on.

### Rate limiting

Each client IP gets a token bucket. Tokens refill at a configured rate (requests per second). When the bucket is empty, the request gets a 429 Too Many Requests response with a `Retry-After` header telling the client exactly when to retry.

Rate limiting is per-node, not cluster-wide. An attacker hitting all nodes gets N times the rate limit. For serious DDoS protection, you'd put something like Cloudflare or AWS Shield in front. Our rate limiter is there for reasonable load shedding, not nation-state defence.

Stale token buckets (no requests for 5 minutes) are garbage collected every 60 seconds to bound memory growth.

### TLS

Phase 3 ships with a TLS stub — we can listen on port 443 with self-signed certificates for testing. Real certificate management (ACME for Let's Encrypt, or cluster CA for air-gapped environments) comes in Phase 4 when we build Sesame, the PKI layer. TLS 1.0 and 1.1 are rejected; only 1.2 and 1.3 are accepted.

## The Perimeter: nftables Firewall

### What we're protecting

A Reliaburger node exposes a lot of ports: container host ports (30000-31000), gossip (9443), Raft (9444), reporting (9445), the management API (9117), plus whatever the operator runs (SSH, monitoring, etc.). Not all of these should be reachable from the outside.

The obvious approach would be a default-deny firewall: block everything, then poke holes for what's needed. We tried that. Turns out, blocking *everything* also blocks SSH, and the first time you apply a default-deny ruleset on a remote server without an out-of-band console, you learn that lesson the hard way.

So we took a different approach. We only block *our* ports. SSH, the operator's monitoring agent, whatever else they're running — we don't touch it. We're a container orchestrator, not a host firewall.

### What gets blocked

The nftables input chain in the `reliaburger` table has `policy accept` (everything passes by default) and explicit `drop` rules for three port ranges:

1. **Container host ports (30000-31000)**: Dynamically allocated by the port allocator. External clients should reach containers through Wrapper (ports 80/443), not by hitting these ports directly.

2. **Cluster ports (9443, 9444, 9445)**: Gossip, Raft consensus, and reporting tree communication. Only cluster node IPs should reach these.

3. **Management port (9117)**: The Bun agent API. Only cluster nodes and admin CIDRs.

Cluster nodes get a blanket `accept` rule that comes *before* all the `drop` rules. So inter-node traffic is never blocked — gossip, scheduling, state replication all work normally. Admin CIDRs get access to the management port specifically.

The order matters in nftables: first match wins. Cluster node accept → admin CIDR accept → drop rules → everything else passes.

### Testable without root

The ruleset generation is a pure function: it takes a config and a set of cluster node IPs, and returns a string of nftables rules. No kernel interaction. We test it on macOS just like any other unit test:

```rust
#[test]
fn ssh_not_mentioned() {
    let config = PerimeterConfig::default();
    let rules = generate_ruleset(&config, &ClusterNodes::new());
    assert!(!rules.contains("dport 22"));
}

#[test]
fn cluster_nodes_bypass_all_blocks() {
    let nodes = cluster_with_nodes(&["10.0.1.1"]);
    let rules = generate_ruleset(&config, &nodes);
    // Accept comes before drop
    let accept_pos = rules.find("10.0.1.1 } accept").unwrap();
    let drop_pos = rules.find("30000-31000 drop").unwrap();
    assert!(accept_pos < drop_pos);
}
```

Applying the rules to the kernel (`nft -f -`) is a separate function that only runs on Linux. The same split we use everywhere: pure logic is cross-platform, I/O is platform-gated.

### Rootless and the firewall

If you're running Bun in rootless mode (no root, user namespaces), the firewall is automatically disabled. nftables needs `CAP_NET_ADMIN`, which non-root users don't have. This is fine for development — single-user dev setups don't need a perimeter firewall. The `PerimeterConfig` has an `enabled` flag that's set to `false` automatically when rootless mode is detected.

You can also disable it manually in `node.toml` for any node that shouldn't apply perimeter rules (e.g., if you're behind an external firewall that handles this).

### TLS: the self-signed stub

Wrapper listens on port 443 with TLS 1.2+ enforced via `rustls` (a memory-safe TLS implementation — no OpenSSL). For Phase 3, we generate a self-signed certificate on startup using `rcgen`:

```rust
pub fn generate_self_signed_cert()
-> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), TlsError> {
    let cert = rcgen::generate_simple_self_signed(
        vec!["localhost".to_string(), "127.0.0.1".to_string()]
    )?;
    Ok((CertificateDer::from(cert.cert), PrivateKeyDer::try_from(cert.key_pair.serialize_der())?))
}
```

Operators can also provide their own cert and key via config (`tls_cert_path`, `tls_key_path`) for environments where a real certificate is available outside of Reliaburger's control.

Phase 4 replaces this with Sesame, our built-in PKI: ACME for public-facing services (Let's Encrypt integration), or cluster CA for air-gapped environments. The self-signed stub is just enough to get the TLS listener working and the handshake tests passing.

## What we learned

### `wrapping_add` is not optional

In C, integer overflow wraps silently. In Go, it wraps silently. In Rust, it panics in debug mode. Our IP calculation code crosses a /24 boundary (that's the whole point of a /23), so the third octet needs to overflow from one block into the next. The first time we ran it, the test panicked. The fix: `wrapping_add`. Two extra characters and a lesson in Rust's "no silent bugs" philosophy.

If you come from C and find this annoying, think about it the other way: every integer operation in your C codebase that doesn't intend to wrap is a latent bug. Rust makes you choose. Explicit is better than implicit, even when it's more typing.

### Shell out to `ip` for one-time setup, eBPF for the hot path

We could have used the `netlink` crate to talk directly to the kernel for veth setup. We chose `ip` commands instead. Not because netlink is hard (it is, but that's not the point), but because debuggability matters more than elegance for one-time setup. When a veth pair isn't working, `ip link show` and `ip netns exec` are your friends. If we'd used netlink, we'd be debugging opaque byte sequences.

The eBPF connect hook is the opposite: it runs on every `connect()` syscall in the hot path. Zero overhead is non-negotiable. Shelling out to anything would be absurd. Match the tool to the frequency.

### Test the logic, gate the I/O

Half of this chapter's code needs root on Linux. But the interesting logic (IP calculation, rule generation, VIP hashing, service map operations, routing table lookups) is pure functions. By splitting them cleanly from the I/O (creating namespaces, loading BPF programs, applying nftables rules), we get fast cross-platform tests for the logic and gated integration tests for the plumbing. `cargo test` on a MacBook runs in 4 seconds. The full suite in a Linux VM takes 30 seconds.

### The BPF hook we wanted didn't exist

We spent two days trying to do DNS in-kernel with `cgroup/sendmsg4` and `cgroup/recvmsg4`. The hooks can modify the destination address but can't read the UDP payload. Can you see the problem? You can redirect a DNS query to your own server, but you can't parse which name was queried or synthesise a response. The BPF helper we'd need only works with `SK_MSG` programs, not cgroup socket address hooks.

50 microseconds in userspace beats two weeks fighting kernel limitations. Pragmatism over purity.

### AtomicU64 for round-robin

The routing table's round-robin counter uses `AtomicU64` with `Ordering::Relaxed`. If you're coming from Go, think `atomic.AddUint64` with no memory barrier. If you're coming from C, think `__atomic_fetch_add` with `__ATOMIC_RELAXED`.

Why relaxed? Because we don't care about precise ordering between threads. If two requests arrive simultaneously and both increment the counter, they'll pick different backends — that's the desired outcome. We're not coordinating anything; we're distributing load. The weaker memory ordering means no cache-line bouncing on most architectures.

### `#[repr(C)]` is your FFI contract

Without `#[repr(C)]`, Rust will reorder struct fields for alignment efficiency. That's great for pure-Rust code and terrible for eBPF maps, where the kernel reads raw bytes at fixed offsets. Every struct shared between Rust and BPF gets `#[repr(C)]` and explicit `_pad` fields. If you forget, the kernel reads garbage from the map — and debugging "why does my BPF program think the port is 0?" is not fun.

## Test count

Phase 3 adds 114 tests, bringing the total to 702. The new tests cover IP calculation (boundary cases, wrapping, max containers per node), service map operations (register, resolve, backend health, unregister), routing table lookups (longest prefix match, round-robin, case insensitivity), firewall rule generation (policy, ordering, SSH exclusion), the DNS responder (`.internal` resolution, upstream passthrough), and eBPF integration (BPF map ops, connect rewrite, backend failover). The eBPF tests are gated behind `RELIABURGER_EBPF_TESTS=1` and require Linux with cgroup v2.

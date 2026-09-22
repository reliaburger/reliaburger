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

The gateway sits at the first usable address in the block, and containers start at gateway + 1. A veth pair — think of it as a virtual cable with a plug on each end — connects the container to the host. One end (`eth0`) lives inside the container's namespace, the other (`veth-{id}-h`) lives on the host. Linux limits interface names to 15 bytes. Short IDs keep that readable form; long IDs use `veth-` plus a stable hash of the *whole* instance ID. Simply cutting off byte 16 looks harmless until `very-long-app-0` and `very-long-app-1` both become the same interface. The two-workload DNS acceptance test found that one.

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
2. Grill starts `runc run` and polls `runc state` for the container init PID
3. We spawn: `slirp4netns --configure --mtu=65520 --disable-host-loopback --api-socket {socket} {pid} tap0`
4. The container gets IP `10.0.2.100` with gateway `10.0.2.2`
5. Port forwarding uses slirp4netns's API socket — we send JSON commands to map ports
6. The instance record stores the socket, mapping and owner PID/start time. A
   replacement Bun either reclaims that exact process or recreates it before
   declaring adoption successful.

The `--disable-host-loopback` flag is important: it prevents the container from reaching services on the host's loopback. Without it, a compromised container could probe the host's `localhost`-only services.

#### One userspace network needs one process owner

Starting networking twice for an instance used to overwrite its `slirp4netns`
handle in a map. The previous helper kept running. Dropping a Tokio `Child`
doesn't kill its process, and we deliberately need that behaviour during Bun
replacement. A plain map insertion cannot distinguish a handoff from a leak.

Replacement now holds the async ownership lock while retiring the old helper,
waiting for exit and installing its successor. Adoption of the same PID and
start time keeps the existing handle; conflicting metadata is refused. When a
different surviving helper takes over, retirement preserves its API socket.
Otherwise the old owner's cleanup could unlink the new owner's socket.

Startup has a stricter lifetime. `PendingSlirp` owns an `Option<Child>` and its
`Drop` implementation requests termination if the future is cancelled. `take()`
moves the child out only after socket readiness and host forwarding succeed.
The published handle can then survive an intentional Bun handoff. This is why
we don't set `kill_on_drop` on every helper indiscriminately.

Socket checks use asynchronous filesystem calls, and one two-second deadline
bounds both readiness and the forwarding handshake. Regression tests cancel a
real helper during startup, stall its forwarding response, replace a failed
network and adopt the same surviving process repeatedly. They check process
exit and socket ownership, not just the number of handles in the map.

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

1. Your app calls `getaddrinfo("redis.internal")`. The C library sends a DNS query to the node-side address of its runc veth (for example `10.0.2.1:53`), where a tiny UDP/TCP server built into Bun is listening. Bun looks up "redis" in the service map and responds with a virtual IP: `127.128.0.3`. The query never leaves the node. Takes about 50 microseconds.

2. Your app calls `connect(127.128.0.3, 6379)`. An eBPF program intercepts the `connect()` syscall *before the kernel sends any packets*, looks up the VIP in a hash map, picks a healthy backend via round-robin, and rewrites the destination to `10.0.2.5:30891`. Your app's TCP connection goes directly to the backend. No proxy.

The DNS lookup adds ~50 microseconds (one node-local UDP round trip). The connect rewrite adds zero — it happens before the TCP handshake. Compare that to CoreDNS over the pod network (~500 microseconds) plus kube-proxy iptables traversal. Your app sees a normal TCP connection that just happens to land on the right backend.

### Virtual IPs

Each service gets a virtual IP from the `127.128.0.0/16` range. This lives within the loopback block (`127.0.0.0/8`), so it never conflicts with real network addresses. No packets with these addresses ever leave the node — the `connect()` hook rewrites them before the kernel acts on them.

Now, what's a "service"? Our first cut said: the app name. `redis` gets a VIP, `web` gets a VIP. That turned out to be a bug. Reliaburger has namespaces, and two teams can each run an app called `api` — one in `default`, one in `payments`. If the VIP is a hash of the bare name, both `api`s hash to the *same* VIP, and whichever registered last wins. One team's traffic silently lands on the other team's backends. Not great.

So a service isn't a name, it's a `(namespace, name)` pair. We wrap that in a newtype:

```rust
pub struct ServiceId {
    pub namespace: String,
    pub name: String,
}

impl ServiceId {
    pub fn qualified(&self) -> String {
        format!("{}__{}", self.namespace, self.name)
    }
}
```

A newtype is Rust's way of giving a plain pair of strings a name and a set of methods. `ServiceId` is a `struct`, not a type alias, so the compiler treats it as distinct from any other two-string pair — you can't accidentally pass a `(host, path)` where a `ServiceId` is expected. The canonical form joins the two with `__` (two underscores). Both a namespace and an app name are DNS-1123 labels, which only allow `[a-z0-9-]`, so an underscore can never appear inside either half. That makes `payments__api` unambiguous to split back apart, even when the app name itself contains a hyphen. It's the same separator our instance IDs use (Chapter 1's `InstanceIdentity`), so the two id schemes read the same way.

The VIP is then derived deterministically from that qualified string using SipHash:

```rust
impl VirtualIP {
    pub fn from_service_id(id: &ServiceId) -> Self {
        Self::from_qualified(&id.qualified())
    }

    pub fn from_qualified(qualified: &str) -> Self {
        let mut hasher = SipHasher24::new_with_keys(
            0xDEAD_BEEF_CAFE_F00D,
            0xBAAD_F00D_DEAD_BEEF,
        );
        qualified.hash(&mut hasher);
        let hash = hasher.finish();

        let offset = (hash % 65534) as u32 + 1;
        let ip = 0x7F80_0000u32 | (offset & 0xFFFF);
        VirtualIP(Ipv4Addr::from(ip))
    }
}
```

`default__api` and `payments__api` hash to different bytes, so they get different VIPs. Same service, same VIP, every time, on every node. No coordination needed. SipHash is a keyed hash function (the `new_with_keys` call) originally designed for hash table collision resistance. We use it here because it distributes names evenly across the 65,534 available addresses with very low collision probability. It's not cryptographic, but it doesn't need to be — we just need a good spread.

The `0xDEAD_BEEF_CAFE_F00D` and `0xBAAD_F00D_DEAD_BEEF` keys are fixed seeds. They're arbitrary constants, but using the same ones on every node is what makes the VIP deterministic cluster-wide.

A good spread isn't a guarantee, though. With 65,534 slots, two *different* services will eventually hash to the same VIP (the birthday problem bites long before the space is full). So the service map keeps a set of allocated VIPs and, on the rare collision, probes deterministic successors — `payments__api#1`, `#2`, and so on — until it finds a free address. The collision is *resolved*, not silently shared. And when a service stops, we release its VIP back to the pool. Previously VIPs lingered forever, which is fine until you churn through enough deploys to notice the leak.

### The service map

Before we get to the eBPF programs, we need the data model they operate on. The `ServiceMap` is Bun's userspace record of which services exist, what their VIPs are, and where their backends live:

```rust
pub struct ServiceMap {
    // Keyed by ServiceId::qualified(), e.g. "payments__api".
    entries: HashMap<String, ServiceEntry>,
    allocated_vips: HashSet<VirtualIP>,
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

When Bun deploys an app with a port, it calls `service_map.register(&ServiceId::new("default", "redis"), 6379, None)`. That computes the namespaced VIP and creates an entry with an empty backend list. Every method that mutates or reads a service takes a `&ServiceId`, so the namespace is threaded through the whole path — the compiler won't let a call site forget it. As instances start and reach the Running state, Bun calls `add_backend()` with the real node IP and host port. When health checks fail, `set_backend_health()` flips the flag. When an app is stopped, `unregister()` removes everything and releases the VIP.

There's one deliberate escape hatch. The `relish resolve <name>` CLI and Smoker's fault-injection targeting still take a bare name — a human typing `relish resolve redis` doesn't want to spell out the namespace. Those go through `resolve_by_name()`, which returns the first match in any namespace. Everything on the deploy and routing path uses the namespaced `resolve()`; the bare-name lookup is only for the human-facing edges.

On Linux, every mutation to the `ServiceMap` gets synced to the BPF hash maps in the kernel. On macOS and for ProcessGrill, the map still works — it powers `relish resolve` — but there are no eBPF programs reading it.

### The BPF maps

The eBPF programs don't call back to Bun. They read from kernel-resident hash maps that Bun populates. Three maps (plus a supplementary one for namespace isolation):

**`dns_map`**: Maps service names to VIPs. Key is a 256-byte null-terminated string, value is a 4-byte IPv4 address in network byte order. It's a leftover from the abandoned in-kernel DNS design (see "Why userspace DNS" below) — the userspace responder resolves names straight from the `ServiceMap` instead, so this map isn't on the resolution path today.

**`backend_map`**: Maps `(VIP, port)` pairs to backend arrays. Each entry holds up to 32 backends with their real IPs, ports, and health flags, plus a round-robin counter. When the connect hook intercepts a VIP connection, it looks up this map and picks a healthy backend.

**`firewall_map`**: Maps `(source_cgroup_id, destination_app_id)` to allow/deny. This is how we enforce namespace isolation and per-app firewall rules at the connection level.

**`cgroup_namespace_map`**: The supplementary map — `cgroup_id → namespace_id`. It's the quiet load-bearing one. The connect hook only runs the isolation check when it can find the *source's* namespace here; if the lookup misses, it lets the connection through. So an empty `cgroup_namespace_map` doesn't fail safe, it fails *open* — every cross-namespace connection is allowed. Populating it is what turns isolation on, and `firewall_map` then carries the explicit exceptions. We learned this the hard way: for a while the resolver that computed these entries had no production caller at all (the same "parsed but never wired" trap we hit with port mappings in Chapter 1). Both maps were empty in every running cluster, so namespace isolation was advertised but inert. The fix is a reconcile in Bun that, on every deploy, redeploy and stop, rebuilds both maps from the live service map and the running instances' cgroup ids — writing what should exist and deleting what shouldn't, so a departed workload's isolation identity can't linger on a cgroup id the kernel later reuses.

All four are `BPF_MAP_TYPE_HASH` — kernel hash tables with O(1) lookup. The structs use `#[repr(C)]` so their memory layout matches exactly between the Rust code that writes the maps and the C eBPF code that reads them:

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

**Deploy.** When `deploy()` processes an app with a port, it registers the service immediately — before any instances start. This creates the `ServiceEntry` with the VIP and an empty backend list. The VIP is available for DNS resolution straight away, even though there are no backends yet. A `connect()` at this point gets `EPERM`, which is the correct answer: the service exists but isn't ready.

**Instance startup.** When `drive_instance_startup()` transitions an instance to Running (or HealthWait with no health checks), it calls `add_backend()` with the instance's real IP and host port. This is when the service actually becomes reachable. If the container has network isolation, we use its `container_ip`. For ProcessGrill on macOS, we fall back to `127.0.0.1`.

**Health transition.** The health check loop already calls `process_health_result()` and handles Running→Unhealthy and Unhealthy→Running transitions. We hook into the same spot: when the transition fires, we call `set_backend_health()` on the service map. The eBPF connect hook reads the `healthy` flag and skips unhealthy backends during round-robin selection. So a failing health check removes a backend from rotation without touching any iptables rules or proxy configuration. One byte flip in a BPF hash map, and the backend is out.

**Stop.** When `stop_app()` shuts down an app, we remove each instance from the backend list and then unregister the service entirely. After this, DNS queries for the name return nothing (pass through to upstream), and `connect()` calls to the now-stale VIP get `EPERM`.

The ordering matters. We register the service *before* starting instances so that the DNS name resolves as early as possible. We unregister *after* stopping so that in-flight connections can drain. And we update health synchronously in the event loop so there's no window where the map disagrees with reality.

### Loading the programs at boot

All of the above assumes the eBPF programs are actually in the kernel. For a long time they weren't. The loader existed, the maps were defined, the `.bpf.c` sources compiled — but nothing ever called `OnionEbpf::load()`, so the field on the agent that would hold the handle was permanently `None`. Everything the connect hook was meant to do fell back to userspace-only behaviour: the service map worked for `relish resolve`, but no `connect()` was ever rewritten in the kernel.

Wiring it in production comes down to three questions, and each has a slightly awkward answer.

**When?** At `bun` startup, gated on an `[ebpf]` config section that defaults to *off*. Loading eBPF needs root, a 5.7+ kernel, and cgroup v2 — none of which hold on a laptop or in the default `cargo test` run. So it's opt-in, like the ingress listener and the DNS responder before it. A node that enables it but is built without the `ebpf` Cargo feature prints a clear warning that enforcement is off, rather than pretending.

**Where are the objects?** `build.rs` compiles the `.bpf.o` files into Cargo's `OUT_DIR`, a hash-suffixed path nobody wants to type. Rather than force an install step, the build script bakes that path into the binary with `cargo:rustc-env=RELIABURGER_BPF_DIR=…`, and the config reads it back with `option_env!`. So a dev or Lima build with `[ebpf] enabled = true` finds its own objects automatically; a packaged install overrides `program_dir` to point at wherever the objects were installed. This is the first time we've used `option_env!` — it's like `env!`, but returns an `Option` instead of failing to compile when the variable is absent, which is exactly right for a default that only exists in eBPF-feature builds.

**What if it fails?** A node that can't load eBPF logs the error and keeps running without kernel enforcement. Refusing to start would be worse: the data plane (containers, health checks, the userspace service map) is entirely independent of the connect hook. Losing the hook degrades service resolution to "no VIP rewriting"; it doesn't take the node down.

The verification for all of this lives in `tests/ebpf.rs`, gated twice — behind the `ebpf` feature *and* `RELIABURGER_EBPF_TESTS=1` — and run inside the Lima VM, never in `make ci`. Nine tests load the real program into a real kernel and check the whole chain: attach to the cgroup, write and read the backend map, rewrite a `connect()` to a VIP, deny a VIP with no backends (`EPERM`, remember), pass a non-VIP through untouched, and resolve `.internal` names through the DNS responder. Green there is the only proof that counts — the loader is Linux-kernel code, and no amount of macOS unit testing substitutes for the kernel actually accepting the program.

### Making the service map cluster-wide: the endpoint catalogue

Everything above describes what happens on a single node. But a 10-node cluster has 10 service maps, and each one only knows about the backends running *on its own node*. When the scheduler places `redis-0` on node A, a container on node B asking for `redis.internal` gets nothing — node B's service map has never heard of redis.

For a long time that was the actual behaviour, and the honest answer was "cross-node service discovery doesn't work yet." Fixing it is what the **endpoint catalogue** does. The idea is small: the leader already knows what's running where, so let it build one cluster-wide map of every service's backends and replicate it to every node.

Where does the leader's knowledge come from? The reporting tree from Chapter 2. Every node reports its running instances — namespace, app name, host port, health — to its council parent every reporting interval; the council aggregates for the leader. The leader's scheduling loop already reads that aggregate. So building the catalogue is a walk over those reports:

```rust
for (node_id, report) in &reports.reports {
    let node_ip = node_ips[node_id];          // from gossip membership
    for app in &report.running_apps {
        let service_id = ServiceId::new(&app.namespace, &app.app_name);
        // group this instance as a backend of its service…
    }
}
desired.endpoint_catalog.reconcile(grouped)?  // preserve VIPs, allocate newcomers
```

`EndpointCatalog::reconcile` preserves allocations from the committed catalogue before allocating newcomers. It uses the same namespaced hash and collision probing as the local map, but does this once on the leader so every node receives the same allocation. The catalogue is a `BTreeMap` keyed by the qualified service id — deterministic JSON, so it snapshots and diffs cleanly.

Now, how does it reach every node? Through Raft. The leader writes the whole catalogue as one `PublishEndpoints` entry:

```rust
RaftRequest::PublishEndpoints {
    expected_generation: desired.endpoint_withdrawals.generation,
    catalog: Box::new(catalog),
}
```

Applying it checks the observed generation, records withdrawals and replaces `DesiredState.endpoint_catalog` — a wholesale swap, so the leader is the single source of truth and a follower never merges half a view. Because it lives in `DesiredState`, it rides the same replication and snapshot machinery as every other cluster fact, and it survives a leader change for free: the new leader inherits the last catalogue and republishes from its own reports on the next tick. The leader only writes when the catalogue actually changed, so a steady cluster isn't churning the log every couple of seconds.

The last hop is getting the catalogue *into* each node's resolution path. Every node's reconciler polls the leader's `/v1/placements/{node}` endpoint every couple of seconds to learn its assignments. We piggyback the catalogue on that response, after registering the consumer in Raft. Council voters also hold the replicated `DesiredState`, but use the same reconciler to install routing. Explicit protocol and state compatibility must match; a serde default is not permission to mix incompatible binaries. The reconciler hands the catalogue to its Bun agent, which overlays it onto the local service map:

```rust
let merged = self.service_map.with_cluster_catalog(&self.cluster_catalog);
```

`with_cluster_catalog` doesn't mutate the local map — that stays the source of truth for what this node runs and syncs to the eBPF backend map. It returns a *merged* view: local services keep their entry and gain any remote backends; a service running only elsewhere is added wholesale with the catalogue's cluster-agreed VIP. That merged view is what gets published to DNS and the ingress routing table. So the moment a service on node B lands in the catalogue, a container on node A resolves `redis.default.internal` to its VIP and the eBPF connect hook rewrites to node B's real address — with no change to the DNS or routing code, because both already read the service map snapshot.

Gossip still plays its part. Mustard doesn't carry catalogue data — that would be too much traffic for O(log N) convergence. It handles *failure detection*: when a node crashes, Mustard marks it Dead within a few probe cycles, and the leader's next catalogue update omits its backends while retaining declared services' VIPs (the reports for a dead node age out). So the data flow is:

1. **What's running where** flows through the reporting tree into the leader's catalogue, then out via Raft (voters) and the placements poll (workers). This is how cross-node backends get *added*.
2. **Failure detection** flows through gossip; a dead node's backends drop out of the next rebuild.
3. **Health check results** are local to each node's Bun agent and flip the `healthy` flag for its own backends before it reports them.

Can you see why we need all three? The reporting tree is accurate but bounded by the reporting interval. Gossip is fast but coarse — it knows a node is dead, not which instance failed. Health checks are precise but local. Together they cover planned shutdowns, node crashes, and application bugs.

There's a subtlety worth noting. During a network partition, a node might be marked Dead by gossip even though it's still running containers. If those containers serve same-node clients (loopback connections never touch the network), they keep working — the partitioned node's *local* service map still has them, and the local map always wins in the merge. Only cross-node connections are affected, which is exactly what you'd expect from a partition. When it heals, Mustard's incarnation counter (remember that from Chapter 2?) rejoins the node cleanly and its backends reappear in the next catalogue.

### Why userspace DNS, not eBPF?

The original design called for fully in-kernel DNS interception using `cgroup/sendmsg4` and `cgroup/recvmsg4` eBPF hooks. We tried it. It doesn't work *at those hooks*.

The problem is that these hooks let you modify the *destination address* of a UDP sendmsg, but they can't read the *packet payload*. You can redirect where a DNS query goes, but you can't parse the query name or synthesise a response. The BPF helper you'd need (`bpf_msg_pull_data`) only works with `SK_MSG` programs (stream parsers for TCP), not cgroup socket address hooks.

That isn't the same as saying eBPF can never answer DNS. TC and XDP packet hooks can read and rewrite packet bytes. The review proof in `poc/dns-tc/` did exactly that: a bounded TC-ingress parser answered mapped `.internal` A queries, repaired checksums and redirected replies into a container namespace without sending matched queries upstream. It also showed the bill. IPv6, TCP fallback, fragments, EDNS, lifecycle, map ownership and observability all become a second DNS implementation we would still need to back with userspace for other runtimes. So TC is feasible, but it isn't the simpler production design today.

So we run a userspace DNS responder instead. It lives in `src/onion/dns.rs`: a `tokio::select!` loop reading from a UDP socket, plus a TCP listener for large answers. Bun configures containers' `/etc/resolv.conf` to point at the responder, and it handles the rest. For `.internal` names, it looks up the service map and responds. For everything else, it forwards to the upstream resolver.

Names are namespace-qualified: `<app>.<namespace>.internal`. A query for
`api.payments.internal` resolves the `api` service in `payments`, and
`api.default.internal` resolves the other `api`, each to its own VIP.

A short name needs a caller identity. Using the node's default namespace seems
convenient until two tenants both run Redis. The responder now looks up the
packet's source address in a snapshot owned by RuncGrill. Runc publishes an
address/namespace binding when it creates the isolated network, before starting
the workload. That includes jobs, init containers and apps without a service
port. Removing a network withdraws the binding before teardown.

The snapshot travels over a `watch` channel. `send_replace` keeps the latest
value even before DNS subscribes; a receiver's `borrow` reads one consistent
snapshot without contending with the runtime's network lock. Duplicate source
addresses with conflicting namespaces resolve to no identity. Unknown sources
get `REFUSED` for short names on both UDP and TCP. Host tools can use qualified
names, and no internal query gets forwarded upstream.

Adoption must restore this ownership too. Bun verifies that the recorded network
namespace and the live container's namespace have the same filesystem identity,
then reads the actual IPv4 address from the kernel. It restores the runtime's
network owner, claims that address in the durable reservation journal, and
republishes the DNS binding. Guessing the address from a restarted counter would let a new
container inherit an old container's namespace identity.

Fault lookup uses the same complete `ServiceId`. A fault authorised for
`payments/redis` cannot affect `default/redis`; overlapping experiments retain
independent ownership until clear or expiry, as Chapter 8 explains.

Tests send real UDP and TCP queries while changing, removing and conflicting a
source binding. Two real runc workloads in different namespaces resolve the same short name to
different VIPs. A privileged adoption test checks the binding before start,
after Bun replacement and after teardown. The old `dns.default_namespace`
configuration field is rejected: an operator preference cannot establish which
workload sent a packet.

The cost is ~50 microseconds per DNS lookup (localhost UDP round trip). That's 10x faster than CoreDNS over the pod network, but it's not zero. Most applications cache DNS results anyway, so this hit happens once per connection lifetime, not per request.

The connect rewrite — the part that actually matters for latency — is still fully in-kernel eBPF. Once your app has the VIP from DNS, every `connect()` call is rewritten at zero cost.

### TCP, and who's allowed to ask

The responder answers `.internal` over both UDP and TCP. A resolver retries over TCP when a UDP reply comes back truncated (the TC bit set), so a UDP-only responder would leave that retry hanging with nobody home. Our answers are small — a single A record with a 4-byte VIP — so truncation is rare, but "rare" isn't "never," and a half-bound responder is a subtle way to lose DNS for one query in a thousand. TCP frames each message with a 2-byte length prefix; the listener reads the length, reads that many bytes, answers, and closes.

The forwarding side had a hardcoded assumption that took a while to surface, because it only bites on networks we don't run on:

```rust
let socket = UdpSocket::bind("0.0.0.0:0").await.ok()?;
socket.connect(upstream).await.ok()?;
```

`0.0.0.0` is the IPv4 wildcard. Point `upstream` at an IPv6 resolver and `connect` fails, `ok()?` turns that into `None`, and the caller sends SERVFAIL — so on a v6-only network *every* non-`.internal` query fails. And it fails in the least helpful way possible: "DNS is broken" rather than "your upstream is v6 and I only speak v4". The socket now binds the family the upstream actually uses.

There's a general shape here. `0.0.0.0` and `127.0.0.1` are so familiar they stop reading as *choices* — they look like "the network" and "here" rather than "IPv4, specifically". Whenever a literal address is baked into code, it's worth asking what happens when someone's world is v6, because the answer is usually "nothing works and the error blames the wrong component".

Not everyone gets to ask, though. The `.internal` zone is a map of the cluster's internal topology, and the responder also forwards ordinary public names. Neither service should be exposed as an open resolver. Every query is gated by a source ACL: loopback and the private ranges containers live in (RFC 1918, plus the `100.64.0.0/10` CGNAT block Lima and runc bridges hand out) are served; a query from a public address gets REFUSED — not answered, not forwarded, refused. The check is a small method on the config:

```rust
pub fn allows(&self, src: IpAddr) -> bool {
    match src {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || /* CGNAT, VIP range */
        }
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}
```

### Fail closed, not open

One more change worth calling out. The original responder was spawned as a detached best-effort task: if it couldn't bind its socket, the error was logged and the node carried on. That's the wrong default. A node that's advertising service discovery but has no resolver is worse than one that refuses to start — the apps deploy, look healthy, and then can't find each other. Bun now binds both UDP and TCP *before* it starts the agent or publishes readiness. Either bind failing aborts startup. If the serving task later exits, its supervisor cancels Bun so the reporting lease expires and the scheduler withdraws the capability. Config validation catches the related trap earlier still: a VIP the responder hands out only routes because the eBPF connect hook rewrites it, so enabling `[dns]` with `[ebpf]` turned off is rejected outright rather than deploying an app whose VIP silently goes nowhere.

There's an ordering snag. The runc gateway address doesn't exist until we create the first workload veth, but we refuse to create that workload until DNS is bound. Binding `0.0.0.0:53` would dodge the race, then collide with `systemd-resolved` and expose port 53 on unrelated interfaces. Linux gives us a narrower tool: `IP_FREEBIND` lets a socket bind one address before an interface owns it. Bun treats the default `0.0.0.0:53` setting as "derive my runc gateway", then binds that derived address with `IP_FREEBIND`.

Rust's standard networking API doesn't expose `IP_FREEBIND`, so this tiny boundary calls `setsockopt`, `bind` and `listen` through `libc`. That's `unsafe`: we're telling Rust that the raw file descriptor and pointers satisfy contracts the compiler can't prove. Each block has a `// SAFETY:` explanation, and the descriptor moves straight into `OwnedFd` so early returns still close it exactly once. The unsafe part is small. The policy around it remains ordinary safe Rust.

### A DNS server that doesn't fall over

The first version of the responder worked in the demo and would have been a disaster in production. It's worth listing what was wrong, because every one of these is a classic UDP server mistake:

1. **One bad packet killed it.** The receive error propagated with `?` straight out of the serve loop. On UDP, `recv_from` can fail for reasons that have nothing to do with your socket — an ICMP port-unreachable from a previous send, for instance. One of those and every container on the node lost DNS until the next restart.
2. **Forwarding was serial.** Upstream queries were awaited inline in the serve loop, on one shared socket. One slow upstream exchange meant every other query — including instant `.internal` lookups — queued behind it.
3. **Replies weren't checked.** The shared upstream socket accepted a datagram from anyone, and whatever arrived first got relayed to the client. Classic cache-poisoning surface.
4. **Unknown `.internal` names leaked upstream.** Ask for `secret-project.internal`, get no local match, and the query — internal service name and all — went to 8.8.8.8.
5. **Every query type got an A record.** AAAA, MX, whatever: here's an IPv4 address. Resolvers get very confused by that.

The hardened version fixes each in turn. Receive errors log and `continue` — the loop is never allowed to die. Public-name forwards spawn a task each (bounded by a `Semaphore` with 64 permits, so a query flood can't spawn unbounded tasks), and each forward uses a *fresh connected socket*: `connect` makes the kernel drop datagrams from any other source, and we additionally check the reply's transaction ID against the query's. A wrong-ID reply — a spoof or a stale packet — is ignored and the client eventually gets SERVFAIL, never the attacker's bytes.

The responder is now properly authoritative for `.internal`: unknown names get NXDOMAIN locally and are never forwarded, AAAA on a known name gets an empty NOERROR ("the name exists, it just has no IPv6"), and unsupported types get NOTIMP. Not one internal byte reaches the upstream.

The Rust-flavoured part is how the responder reads the service map. The agent *owns* its `ServiceMap` — it mutates it freely inside its event loop, no locks. Sharing it with the DNS task via `Arc<RwLock<…>>` would have meant threading lock acquisitions through dozens of agent call sites. Instead the agent publishes a *snapshot* on a `tokio::sync::watch` channel every time the map changes (the same moment it rebuilds the routing table). A watch channel holds exactly one value — the latest — and the responder reads it with `borrow()`, no await, no contention. The cost is a clone of the map per change; deploys are rare and maps are small, so that's a bargain for keeping the agent lock-free. Go programmers will recognise the shape: don't share memory, communicate — except here the borrow checker enforces it.

Containers find the responder through an old-fashioned mechanism: `/etc/resolv.conf`. Runc writes a per-instance file in the OCI bundle and bind-mounts it read-only at `/etc/resolv.conf`; it never edits the shared unpacked image rootfs. The file names the node-side veth gateway, not host loopback. `resolv.conf` has no port syntax, so only port 53 is valid.

Rootful runc is the supported transparent DNS path today. Rootless runc now has supervised slirp networking and published ports, but it still has no route to the node's port-53 responder or resolver injection. ProcessGrill doesn't install a resolver into the host, and Apple Container has no DNS injection in its adapter. Enabling `[dns]` with any of those runtimes fails before adoption or workload creation. The live node capability records readiness, IPv4/IPv6 support and workload reachability; a DNS-enabled scheduling pass excludes nodes that can't prove all three.

### Testing service discovery

How do you test code that runs in the kernel? Two approaches, at different levels of fidelity.

**Unit tests on the data model.** The `ServiceMap`, `VirtualIP`, and `#[repr(C)]` types are pure Rust. We test them normally — register a service, add backends, verify resolve returns the right data. These run on any platform, no kernel required:

```rust
#[test]
fn register_and_resolve() {
    let mut map = ServiceMap::new();
    let id = ServiceId::new("default", "redis");
    let vip = map.register(&id, 6379, None).unwrap();
    let entry = map.resolve(&id).unwrap();
    assert_eq!(entry.vip, vip);
    assert!(entry.backends.is_empty());
}
```

And the collision the whole refactor exists to kill gets its own test — the same name in two namespaces must produce two VIPs:

```rust
#[test]
fn same_name_two_namespaces_get_distinct_vips() {
    let mut map = ServiceMap::new();
    let a = map.register(&ServiceId::new("default", "api"), 3000, None).unwrap();
    let b = map.register(&ServiceId::new("payments", "api"), 3000, None).unwrap();
    assert_ne!(a, b);
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

**eBPF program tests** load real eBPF programs into a real kernel and verify the connect rewrite actually happens. This is the test that matters most. They're gated two ways at once: the BPF loader code only compiles under the `ebpf` Cargo feature (it pulls in the `aya` crate), and the tests only run when `RELIABURGER_EBPF_TESTS=1` is set. So the full incantation on a Linux box with cgroup v2 is:

```sh
RELIABURGER_EBPF_TESTS=1 cargo test --features ebpf --test ebpf
```

Forget the feature flag and the tests don't even compile in; forget the env var and they compile but skip. Both switches, on purpose — eBPF needs root and a recent kernel, so it must never be part of the default `cargo test`.

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

### connect4 has a sibling

The name `cgroup/connect4` gives it away: this hook only sees IPv4 `connect()` calls. IPv6 connects go through a separate hook, `cgroup/connect6`, and for a long time we simply didn't attach one. For the VIP rewrite that's fine — VIPs live in `127.128.0.0/16` and are v4 by construction. For the egress policy that later grew inside this same program (Chapter 10), it was a hole you could drive a truck through: any dual-stack workload could bypass its entire allowlist by connecting over IPv6. Phase 12b added `onion_connect6` to the same object file and attaches it right next to connect4. It does no rewriting (there are no v6 VIPs to rewrite), it's pure policy.

Each health tick checks those live hooks and the enforcement map. The result
drives both workload fencing and the readiness capability published for that
tick. Previously readiness immediately repeated the same probe, paying for
another map read and potentially describing a different observation. The
enforcement method now returns its capability value after handling affected
workloads; readiness consumes that value directly.

This isn't a cache across ticks or requests. The next tick observes the kernel
again, and a separate cluster report also gets fresh evidence. Repairing a
missing enforcement flag still requires a second map read to verify the repair.
That read proves a change took effect; removing it would weaken the boundary.
The regression counts observation calls rather than relying on a benchmark's
timing. A real eBPF test detaches the hooks and requires both workload stop and
withdrawn readiness capability within the existing four-second bound.

One wrinkle worth knowing about: a dual-stack socket reaching an IPv4 server goes through *connect6* with a "v4-mapped" address, `::ffff:a.b.c.d`. The connect6 hook has to spot that pattern and judge the connection against the IPv4 policy, or the mapped form becomes yet another bypass. The kernel also insists that `user_ip6` is read in 32-bit chunks — the verifier rejects byte-wise loads from that context field.

While we were in there, we fixed how a defective object file fails. The map handles used to be fetched lazily, deep inside the agent, with `.unwrap()` — a `.bpf.o` missing a map would panic Bun at the first write, minutes or hours after startup. Now the loader validates every required map and program against a single list the moment the object loads, and refuses with the full roster of what's missing. One clear error at load time beats nine scattered panics at use time.

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

A container calls `getaddrinfo("redis.internal")`. Bun's built-in DNS responder looks up the service map and responds with a virtual IP — one node-local UDP round trip, about 50 microseconds. The container calls `connect()` with that VIP. An eBPF program intercepts the syscall, picks a healthy backend via round-robin from a kernel hash map, and rewrites the destination. The TCP handshake goes directly to the backend. No proxy in the data path, no iptables rules.

Kubernetes needs CoreDNS (a separate Go binary consuming 170MB of RAM), kube-proxy (thousands of iptables rules, O(n) per packet), and often a service mesh sidecar (Envoy, consuming 50-100MB per pod). We need a single binary with a built-in DNS responder, one eBPF program, and a hash map. The eBPF connect hook persists in the kernel — if Bun crashes, running connections keep working. DNS resolution pauses (since Bun runs the responder), but that only affects new connections. Existing TCP sessions are fine.

We originally planned to do DNS entirely in-kernel too. It turned out the cgroup socket-address hooks we chose can't read DNS packet payloads. A TC proof shows packet hooks can do it, but only by adding a second, deliberately narrower parser and lifecycle. So we went with the pragmatic approach: userspace DNS at 50 microseconds, in-kernel connect rewrite at zero. Pragmatism over purity. We'll revisit TC only if measurements show a material reason.

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

The ingress specs the agent feeds into the rebuild are keyed by `(namespace, app_name)`, not the bare name — the same collision fix as the VIPs. Two teams both running an `api` app, each with its own ingress on its own host, need two independent routes. Keyed by name alone, the second would clobber the first; keyed by the pair, they coexist, and each rebuild looks its backends up through the namespaced `ServiceId`.

The prefix match has a sharp edge that's easy to get wrong. If `/api` is a route, does `/apievil` match it? `path.starts_with("/api")` says yes — and that's a routing bug that hands one app's traffic to another. A prefix should match on a path *segment* boundary: `/api` matches `/api` and `/api/v1`, but not `/apievil`. So the real check is `path == prefix || path.starts_with(&format!("{prefix}/"))`. Small helper, and one of those cases where the naive one-liner is subtly wrong in a way tests catch and eyeballs don't.

One more subtlety in the lookup: stripping the port off the `Host` header. `myapp.com:8080` should look up `myapp.com`. The obvious `host.split(':').next()` works — right up until someone sends an IPv6 literal like `[::1]:8080`, where splitting on the first colon gives you `[`. You have to notice the bracket and keep everything up to the closing `]`. IPv6 punishes every parser that assumes a colon means "port".

### What happens when things go wrong

Three error codes tell the client exactly what happened:

- **404 Not Found**: No route matches the `Host` header. The request is for a domain we don't know about.
- **502 Bad Gateway**: A route matches, but all backends are unhealthy. The app is deployed but broken.
- **503 Service Unavailable**: The connection limit was reached. We're overloaded.
- **413 Payload Too Large**: The request body exceeded the configured cap (more on that below).

### Connection draining

When an app is being redeployed (rolling update), the old instances need to finish serving in-flight requests before they're stopped. This is the drain protocol:

1. Bun withdraws the backend from the routing table, waiting for existing route captures to finish
2. Bun starts draining instance web-0 with a 30-second deadline; existing request guards remain counted
3. In-flight requests complete normally
4. The deadline cancels remaining work. Only after its request guards release does Wrapper tell Bun: "drain complete"
5. Bun stops the old container

The app never drops below its replica count during a deploy. If you have 3 replicas and `max_surge = 1`, the sequence is: start replica 4, drain replica 1, start replica 4', drain replica 2, and so on.

"All connections are done" is trickier than it sounds once WebSockets are in play. A plain HTTP request is short: it arrives, gets a response, and it's gone. A WebSocket is a *long-lived splice* — the client and backend exchange frames for minutes or hours after the initial `101 Switching Protocols`. If the drain only counts HTTP requests, it declares "done" the instant the last request returns, then kills a container that still has a chat session or a live log tail flowing through it. So the tracker keeps two counts, and a backend isn't drained until *both* the HTTP count and the WebSocket count reach zero. The deadline asks the splice to stop; it does not substitute for those zero counts. We'll come back to exactly how the proxy keeps that WebSocket count honest.

### Rate limiting

Each client gets a token bucket. Tokens refill at a configured rate (requests per second). When the bucket is empty, the request gets a 429 Too Many Requests response with a `Retry-After` header telling the client exactly when to retry.

There's a question hiding in "each client": keyed by what? Key by IP alone and one client's `/api` traffic spends the same budget as its `/` traffic — two routes with different limits share a bucket, which is wrong. So the bucket key is the pair `(route, client IP)`, where the route is `(host, path prefix)`. A client hammering `/api` can still load `/` at full rate. Same fix shape as the namespaced VIPs and routes: when two things should be independent, put both of them in the key.

The config for a bucket also needs validating before it's ever used. A rate of zero requests per second isn't "unlimited", it's a divide-by-zero waiting to happen when we compute the retry-after. So we reject `rps == 0` (and a `burst` that would overflow when we derive its default) at routing-rebuild time — the route with the bad limit is simply never installed, and the operator gets told which app was rejected. Bad input caught at the door beats a panic in the hot path.

Rate limiting is per-node, not cluster-wide. An attacker hitting all nodes gets N times the rate limit. For serious DDoS protection, you'd put something like Cloudflare or AWS Shield in front. Our rate limiter is there for reasonable load shedding, not nation-state defence.

Stale token buckets (no requests for 5 minutes) are garbage collected every 60 seconds to bound memory growth.

### TLS, per route

Phase 3 shipped a TLS stub: listen on 443 with a self-signed certificate, good enough for tests. The gap the July 2026 review found was that the routing table didn't know which routes actually *wanted* TLS. HTTP and HTTPS shared one router, so a route configured with `tls = "cluster"` was served in plaintext on port 80 just as happily as on 443. Configuring TLS and getting plaintext is worse than no TLS — you *think* you're protected.

The fix carries a `TlsMode` into each `PathRoute`:

```rust
pub enum TlsMode {
    Disabled,
    Cluster,
    Explicit,
}
```

`Disabled` is plain HTTP. `Cluster` issues the ingress certificate from the cluster's Sesame Ingress CA — the air-gapped case, where every client already trusts the cluster root. `Explicit` uses an operator-supplied certificate and key file. A route that asks for anything else — `auto`, `acme`, a typo — is a **config error**, rejected at rebuild, not silently downgraded to plaintext. That's the whole point: an unsupported mode must fail loudly, never fall back to the clear.

Once a route knows it needs TLS, the plain-HTTP listener stops serving it. A request for a TLS route on port 80 gets a `308 Permanent Redirect` to the `https://` URL (308, not 301, because it preserves the method and body — a redirected POST stays a POST). That includes `/.well-known/acme-challenge/`: v1 has no ACME responder, so leaving that path open would create a plaintext exception with nobody legitimate to answer it.

For the cluster-CA path we reuse Sesame's Ingress CA rather than inventing a parallel certificate scheme. Sesame already builds a Root CA and three intermediates — Node, Workload, and Ingress — when a cluster is initialised. `issue_ingress_cert` asks the Ingress CA for an end-entity certificate carrying the ingress hostnames as SANs and the TLS server extended-key-usage, then hands the DER straight to rustls. One CA hierarchy, one trust root, ingress certs included. We deliberately did *not* build full ACME here — that's a lot of protocol for a speculative feature, and the explicit and cluster paths cover the real cases. TLS 1.0 and 1.1 are still rejected; only 1.2 and 1.3 are accepted.

### Switching the proxy on

Confession time. Everything you've just read about the Wrapper — the routing table, rate limiting, draining, TLS — was true of the *library* for a long time before it was true of the *binary*. The July 2026 review found that `run_proxy` had zero production callers. No listener was ever bound. The agent dutifully rebuilt the routing table on every deploy, and nothing ever read it. A complete front door, tested and documented, leaning against the wall next to the doorframe.

The wiring is a good case study in how subsystems connect in Reliaburger, because the proxy lives *outside* the agent's event loop. The agent owns the routing table and rebuilds it on deploys; the proxy reads it on every request. They share it the standard Rust way: `Arc<tokio::sync::RwLock<RoutingTable>>`. The agent hands out a clone of the `Arc` before it moves into its spawned task, and from then on the two tasks communicate through the lock — writers rare (deploys), readers hot (every request). No channel needed, because the proxy never tells the agent anything.

Three details from the wiring are worth keeping:

*Binding is separate from serving.* `bind_proxy` opens the listeners and returns a `BoundProxy` whose addresses you can inspect; `.serve()` starts the traffic. Why split them? Tests. A test binds port 0, the OS assigns a free port, and the test reads the real address before sending requests. If binding and serving were one call, a test would have to guess ports — and port-guessing tests are flaky tests.

*The TLS accept loop spawns the handshake.* A TLS handshake involves round trips, and a client can stretch those out for as long as you let it. Do the handshake inline in the accept loop and one slow client stops every new connection behind it. So the loop accepts the TCP connection, spawns a task, and *that task* does the handshake. An ingress that can be stalled by one malicious dial-up modem isn't much of a front door.

*Rate limiting needs to know who's asking.* The token buckets are per-client-IP, which means the handler needs the peer address. Axum provides this via `ConnectInfo<SocketAddr>` — but only if you serve the router with `into_make_service_with_connect_info`. Forget that (we did, briefly) and the extractor fails at runtime, not compile time. It's one of the few places axum can't type-check your wiring end to end.

The config side is a new `[ingress]` section in node.toml, disabled by default — binding listeners on 80/443 is not something a node should do just because the code can. `relish` and the dashboard don't change at all: `/v1/routes` was already serving the routing table; now the table finally has traffic flowing through it.

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

### Two address families, two tables

An early version of this ruleset lived in a single `table ip reliaburger_fw`. Spot the problem? In nftables, the `ip` family only matches IPv4 packets. Every one of those carefully ordered drop rules was void for IPv6 traffic — a client connecting to `[::1]` equivalent addresses or the node's global v6 address sailed past the "blocked" management port. A firewall that only guards one address family isn't half a firewall; it's a decoy.

The generator now renders the same policy twice: once for `table ip reliaburger_fw` and once for `table ip6 reliaburger_fw` (same name, different family — nftables treats them as distinct tables). Port drops appear in both. Source-address rules go where they belong: v4 cluster nodes and admin CIDRs into the `ip` table, v6 ones into `ip6`. A test pins that a v6 admin CIDR never leaks into the v4 half and vice versa.

Two more hardening notes from the same pass. First, admin CIDRs come from `node.toml`, and the old code interpolated them into the `nft -f` script as raw strings — a config value of `10.0.0.0/8; drop` would have become part of the ruleset. Now every CIDR is parsed into a real address and prefix length, validated (`10.1.2.3/8` with host bits set is an error, not a guess), and only the re-serialised form is ever rendered. Config is input; input gets validated. Second, the `nft` invocation itself now runs under a ten-second `tokio::time::timeout` — a wedged nft process used to be able to hang the agent's event loop indefinitely.

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

Sesame later added cluster-CA certificates, and operators can load a certificate and key from disk. Automatic ACME/Let's Encrypt provisioning didn't make the v1 cut. `auto` and `acme` therefore fail route validation instead of falling back to either plaintext or a self-signed certificate.

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

### A reverse proxy is a translator, not a photocopier (post-audit fix)

Once the ingress proxy was actually serving traffic, an audit caught it copying headers too faithfully. A proxy sits between two separate HTTP connections, and some headers describe *the connection*, not *the message*: `Connection`, `Transfer-Encoding`, `Keep-Alive`, `Upgrade`. RFC 7230 calls these "hop-by-hop", and they must not be passed along. We were forwarding all of them, and worse, copying the backend's `Transfer-Encoding: chunked` onto a response whose body we'd already buffered into a fixed-size chunk. The client saw a header promising chunked framing over a body that wasn't. The fix is a small filter applied in both directions: drop the hop-by-hop set, drop anything the peer named in its own `Connection` header, and let the HTTP library compute framing for the body we actually send.

The WebSocket path had a subtler hole. Our handler read the backend's `101 Switching Protocols`, then returned its own hand-written `101` to the client — without the `Sec-WebSocket-Accept` header the client needs to finish the handshake, and while quietly dropping the backend socket. It looked done in a code review and worked in no real browser. The real version takes the client's upgrade future (`hyper::upgrade::on`), relays the backend's *actual* handshake response verbatim, and then splices the two raw streams with `tokio::io::copy_bidirectional`. After the upgrade, Wrapper is a dumb pipe — it doesn't parse WebSocket frames, it just moves bytes. The lesson: a handshake stub that returns the right status code but the wrong headers is worse than no stub, because the tests that only check the status code go green.

### Holding the permit for as long as it matters (post-audit fix)

The connection limit I described earlier had a leak. The counter went up when the handler started and down when it *returned* — fine for a normal request, badly wrong for a WebSocket. A WebSocket handler returns at the `101`, the moment the splice *begins*. So the count dropped to zero while thousands of live WebSockets were still spliced, and the 10,000-connection ceiling protected nothing.

The clean way to tie "resource held" to "work in progress" in Rust is a *permit* whose lifetime you control. We switched the counter to a `tokio::sync::Semaphore`: a handler acquires an owned permit (`try_acquire_owned` — it returns immediately with a 503 when the pool is empty rather than queueing), and the permit is a value that lives on the stack until it's dropped. For a normal request that's when the handler returns. For a WebSocket, we *move the permit into the splice task* — the same task running `copy_bidirectional` — so it's dropped only when the splice actually closes. Same trick for the drain guard, so a draining backend's WebSocket count stays honest for the life of the connection, not just the handshake. This is ownership doing exactly what it's for: the permit is released precisely when the last thing holding it goes away, and you can't forget to release it because there's no explicit release to forget.

The `101`-then-splice tests are the ones that pin this down. It's not enough to check that a WebSocket connects. You open a WebSocket, hold it, and assert that a *second* connection is refused while the first is live — proving the permit didn't come back at the 101. Then close the first and assert the second now succeeds. A test that only opens one connection would pass against the buggy code.

TLS handshakes got the same "bound the resource" treatment from the other side. Spawning a task per handshake (the fix from a couple of sections ago) stops one slow handshaker blocking the accept loop, but it doesn't stop *ten thousand* slow handshakers spawning ten thousand tasks. So the accept loop now grabs a handshake permit from a second semaphore before it spends any work, and wraps the handshake itself in a `tokio::time::timeout`. A peer that opens a socket and never sends a ClientHello is dropped on the deadline, and its permit returns to the pool. Bounded concurrency plus a deadline: a flood costs a fixed amount of memory and clears itself.

### Streaming, not photocopying, the body (post-audit fix)

The "translator not photocopier" section was about headers. The bodies had their own version of the same sin. The proxy buffered the whole request body (up to a hardcoded 10 MiB) and collected the *entire* backend response into memory before sending a byte to the client. For a server-sent-events stream or a gRPC call or a large file download, that's not a proxy, it's a bucket — the first byte reaches the client only after the last byte arrives, and a big response pins its whole size in RAM.

The response side now streams. `reqwest` gives us `resp.bytes_stream()`, an async stream of chunks as they arrive off the backend socket; `axum::body::Body::from_stream` wires that straight into the response hyper sends to the client. Chunks flow through with backpressure — if the client reads slowly, the read from the backend slows to match, and memory stays flat regardless of body size. The request side keeps a *configurable* cap (not a magic 10 MiB) and rejects an over-cap body with `413 Payload Too Large`, so an unauthenticated client can't ask the proxy to buffer something enormous.

The test for this is deliberately a bit odd: the backend sends a 5 MiB response through a proxy configured with a 1 KiB *request* cap, and we assert the client receives all 5 MiB. If the response were bounded by (or buffered to) the request cap, it would be truncated. Getting every byte back proves the response path is genuinely unbounded and streaming.

### Don't let the client lie about who it is (post-audit fix)

A backend behind a proxy often wants to know the real client's IP and whether the original request was HTTPS. The convention is the `X-Forwarded-For` and `X-Forwarded-Proto` headers, and the proxy is supposed to *set* them. Our header-copy loop forwarded them instead — including any the client sent. So a client could send `X-Forwarded-For: 10.0.0.1` and the backend would believe it came from `10.0.0.1`, defeating every IP allowlist or audit log that trusted the header.

The rule for a forwarding header is: the proxy owns it. We strip whatever the client sent (`X-Forwarded-For`, `X-Forwarded-Proto`, and the RFC 7239 `Forwarded`) and replace them with the proxy's own view — the real peer IP from `ConnectInfo`, and `https` or `http` depending on which listener the request arrived on. The backend gets the truth, and nothing the client puts in those headers survives. The test sends a request with a forged `X-Forwarded-For` and asserts the backend sees the real loopback address, not the forgery.

## Test count

Phase 3 adds 114 tests, bringing the total to 702. The new tests cover IP calculation (boundary cases, wrapping, max containers per node), service map operations (register, resolve, backend health, unregister), routing table lookups (longest prefix match, round-robin, case insensitivity), firewall rule generation (policy, ordering, SSH exclusion), the DNS responder (`.internal` resolution, upstream passthrough), and eBPF integration (BPF map ops, connect rewrite, backend failover).

Most of these run under a plain `cargo test` on any machine. The privileged ones split by gate: `RELIABURGER_NETNS_TESTS=1` (network namespaces, needs root on Linux), `RELIABURGER_RUNC_TESTS=1` (runc containers), and `RELIABURGER_EBPF_TESTS=1` together with `--features ebpf` (the connect-rewrite tests, needs Linux and cgroup v2). On a Mac, `relish dev test` sets all three env vars and the feature flag inside the Lima VM, so one command runs the lot — no need to remember the matrix.

## Release packaging: the eBPF object travels with Bun

A local build can load `onion_connect.bpf.o` from Cargo's output directory.
Copy that executable to another machine and the directory disappears. The old
configuration default therefore worked for development but failed for a release
that shipped only Bun.

An eBPF-enabled Linux build now embeds the object in the executable with Aya's
`include_bytes_aligned!` macro. Like Rust's `include_bytes!`, this reads a file at
compile time and produces a reference to its bytes. Aya's variant also gives the
bytes the alignment its ELF parser requires. `concat!` constructs the file name
from Cargo's `OUT_DIR` environment variable during compilation. No path lookup
happens when the installed binary starts.

Both embedded and explicitly configured objects go through the same map and
program validation and cgroup attachment code. An operator can still set
`ebpf.program_dir` for a custom object. If that override fails, we report the
failure; silently substituting another object would hide a configuration error.
The existing capability checks continue to refuse workloads requiring enforcement
when attachment fails.

The build script checks Cargo's target OS, rather than the OS running the build
script. Those differ when cross-compiling. An embedded-object integration test
loads and attaches the object on a real Linux kernel without supplying an object
directory. This belongs in the privileged eBPF suite; a macOS unit test can't
prove that a Linux kernel accepts a program.

## A route belongs to the cluster

Our first three-VM quickstart started a healthy container and still returned
HTTP 502. We had combined its container IP with its published host port. Those
are two different ways into the same process: `10.0.2.5:8080` reaches it directly
from its node, while `192.168.104.3:40032` goes through that node's port mapping.
Combining the first address with the second port reaches nothing.

The agent now builds local backends from the container IP and the application's
declared port. A runtime sharing the host network keeps the published port.
Remote endpoints retain their node IP and published port. When we merge the
cluster catalogue, we exclude this node's own published endpoints; its local
service map already has the direct addresses. This also avoids routing a local
request through the node's external NAT rules.

There was another gap. Only nodes running a replica received its ingress
configuration. A request landing on an otherwise idle node had no route, even
though that node knew where the container lived. The placements response now
carries all desired ingress configurations alongside the endpoint catalogue.
Each node rebuilds its routes when either changes, including when a route is
removed. We keep these cluster configurations separate from locally deployed
ones, so a placements poll doesn't erase a standalone deployment's routes.

The regression tests use a mock runtime that returns a container IP, check the
port in the resulting backend, and install a remote route on an agent with no
local instances. Removing that route without changing any endpoints must remove
it from the routing table too. A healthy container is only half the story; the
request still has to reach it.


### A finite subnet needs durable ownership

A /23 provides 510 usable addresses. The gateway takes one, so the rootful
runtime has 509 container slots. Incrementing a `u16` counter does not enforce
that boundary: it eventually chooses a broadcast address, spills into another
subnet and wraps onto an occupied slot. Restarting the counter makes matters
worse.

`NetworkLeases` records each instance's index before any network command runs.
The private `.network-leases.json` file also identifies its node subnet and
format. Updates use a unique temporary file, file sync, atomic rename and
directory sync. Every transaction reloads the journal under a process lock;
corrupt data, duplicate indices, symlinks and a changed subnet refuse allocation.
A full pool refuses before creating the instance's bundle. The low-level network
setup function independently rejects indices outside the declared subnet.

The transaction runs in `spawn_blocking`, which moves filesystem work off the
async executor. It owns an `OwnedMutexGuard`, so cancelling the waiting future
does not release that guard while the file write is still running. The OS file
lock also needs explicit release in `Drop`: a concurrent fork can briefly inherit
its file descriptor before exec, so closing only our descriptor can leave the
lock busy. A guard's destructor is a useful place to express that ownership.

Each instance also has a lifecycle mutex. Create, adoption, exit observation
and teardown cannot race for that instance, while different instances can
progress concurrently. The lock map stores `Weak` references: unlike `Arc`, a
`Weak` does not keep the lock alive. `upgrade()` returns `Some(Arc<_>)` only while
an owner still exists, and unused map entries can be discarded.

Failed or cancelled setup leaves its reservation intact until cleanup confirms
that the named namespace, host veth and owned host-port forwarding entries have
disappeared. It inspects the actual nftables map, so a cancellation between
installing a mapping and publishing its handle cannot hide a stale entry. Cleanup can
reconstruct this plan from the journal even if Bun died before publishing an
in-memory network handle. It retains the address on uncertain teardown. A new
Bun adopts verified live kernel addresses into the same journal; a conflicting
owner refuses. Operators must retain this journal with the runtime state and
must not delete it to bypass exhaustion. Cancelled, unadopted plans can consume
capacity until the runtime explicitly retires their owner; there is no expiry
that silently reuses a possibly live address.

Tests fill all 509 slots, reload the pool, retire one slot and reuse exactly
that slot. Concurrent reservations remain distinct. Real rootful tests cancel
a creation while its registry request is stalled, replace the runtime object,
and show that the address is unavailable until recovered teardown succeeds.
They also cover duplicate-create refusal, adoption, failed bundle preparation
and reuse after namespace removal.


### Configuration needs a consumer

Wrapper uses unweighted round-robin across routable backends. Its old public
`lb_strategy` field offered `LeastConnections`, but neither backend selector read
it. We remove that field and enum for 0.1.0. A future strategy needs connection
accounting and selection tests before it earns a setting.

We also remove `WrapperConfig::worker_threads`. Wrapper runs on Bun's existing
Tokio runtime; it doesn't create a separate four-thread runtime. Keeping an unused
field would let callers believe they had configured a resource boundary. Neither
field was part of the node TOML or a serialised routing record, so this changes
only the unreleased Rust library API. Existing routing and proxy tests still
exercise the supported behaviour.


The helper inventory also removes the unused `run_proxy` convenience wrapper.
Bun uses `bind_proxy` and `BoundProxy::serve`: binding is a fallible startup step,
and serving is the owned long-lived task. DNS keeps its standalone
`run_dns_responder` wrapper because the DNS and eBPF integration tests exercise
that public entry point. Its documentation now names that role and distinguishes
it from Bun's readiness-aware binding path.


### Give the DNS codec the whole packet

Send a question whose QCLASS is missing its last byte. Our old decoder still
answered it. It read the name and QTYPE, then assumed the rest of the packet
was sound. It also trusted header counts and joined labels without preserving
the difference between a separator and a literal dot inside one label.

The replacement uses Hickory's protocol codec, with its standard-library
feature and no resolver or DNSSEC engine. The codec checks names, compression
and record boundaries. We require the decoder to consume the complete packet;
unclaimed trailing bytes are an error. Onion then admits one normal IN-class
question, checks the source identity and applies the existing namespace rules.
Unsupported operations, malformed records and unsupported name forms are
refused before either an internal answer or upstream forwarding.

`Message::read(&mut decoder).ok()?` deserves a look. `&mut` lends the decoder
exclusively while the codec advances its cursor. `.ok()` turns a decoding error
into `None`, and `?` returns that absence to the caller. Here absence means the
network task drops the malformed request. It never means an empty successful
answer. This parser doesn't use an `unwrap()` on network data.

The same library encodes responses. That removes our separate walk to find
the end of a question, which would need its own compression handling. Internal
answers still have zero TTL, A records resolve to the service VIP, and known
names queried for AAAA still receive an empty successful response. EDNS0
queries receive an OPT response advertising our bounded UDP payload size.
This doesn't implement DNSSEC, general authoritative hosting or TCP recursion.

The wire corpus includes every truncation of a valid query, wrong counts,
response/opcode misuse, non-IN classes, overlong names, a compression loop,
unclaimed bytes, missing additional records and a literal dot inside a label.
A valid request sent immediately afterwards must still succeed. A TCP case
checks that malformed input closes only that connection. The existing live
namespace, fault, forwarding and TCP/UDP tests retain their original assertions.

### Port zero reserves one transport at a time

Coverage CI caught a DNS startup failure despite asking the kernel for a free
port. UDP and TCP have separate port allocators. The UDP socket selected a port
that an unrelated TCP listener already owned. Binding the TCP half then failed.

When the requested port is zero, the responder now retries that collision up to
16 times. Each attempt keeps its UDP socket until TCP binds, or drops it before
trying again. Explicit ports still fail on conflict; Bun cannot silently move a
configured DNS service elsewhere. The wire tests also hold 64 successful pairs,
check that both transports stay reserved, and verify that dropping a pair
releases both sockets.

### A test backend must honour HTTP too

The request-ID regression used a tiny TCP echo server that read once, advertised
HTTP/1.1 keep-alive and closed after one response. The proxy could reuse that
connection before noticing its close, so the second assertion sometimes saw an
empty error response. A full native suite reproduced the failure. The fixture
now uses axum to parse requests and manage keep-alive, and joins its proxy and
backend tasks. The production forwarding code hasn't changed.


### Retire the command before reusing the address

A namespace already exists. The next `ip link add` invocation is waiting to
create its veth pair. Now kill Bun. Looking only for the veth tells the new Bun
that it is absent, but the old command can still create it. Our physical
regression stops at exactly this point.

Namespace creation, forwarding installation, forwarding inspection and teardown
now accept a `RuntimeCommandExecutor`. There are two concrete implementations:
a direct executor for standalone callers and a claimed executor attached to the
durable generation described in chapter 1. The network operations construct the
same argument arrays in either case. The trait changes who owns execution, not
what command the network operation asks to run.

The claimed executor shares a Tokio mutex containing `Option<IntentCommands>`.
`Some` holds the live claim. An execution error leaves `None`, forcing recovery
from durable evidence rather than allowing a second operation to assume the
first completed. A spawned worker retains the mutex through the operation and
restores the claim on success even when the requesting future is cancelled.
A clone made before sealing keeps normal admission authority only; it cannot
act as a cleanup handle after the generation enters Retiring. Its refusal also
preserves the legitimate cleanup handle.

The real Linux test creates a namespace, pauses the veth command, and sends
SIGKILL to the caller process. Recovery claims the original generation, fences
new work and positively retires the waiting command. Only then does it delete
the namespace and inspect kernel absence. Opening the old gate afterwards
produces no veth. A second physical test runs namespace setup and nftables port
publication to completion, then verifies owned forwarding and network teardown.
Both tests run in the provisioned Linux suite and share its host-network test
group. They use private intent directories and distinct namespace names.

A completed deletion command can return non-zero because its target is already
absent. Teardown therefore inspects the kernel after command completion. A
command execution error is different: it now refuses teardown before that
inspection can be mistaken for final retirement. The owned network path is
qualified independently; production Runc still needs to carry these handles
alongside its durable launcher and rootless-helper records.

### Two containers need two routes

One container could reach its gateway. Adding a second broke host access to
that second address. Both host veth interfaces advertised the node's whole
`/23` allocation as a directly connected network, so Linux sent packets for the
second container through the first container's interface. The regression creates
two real network namespaces and exchanges packets from the host and from each
peer before removing both namespaces.

Keep the `/23` as an allocation pool, but give each endpoint a `/32` address.
The host installs an explicit route to each container through its own veth. Each
container has a direct route to the gateway and a default route through it; it
must not try to ARP for a peer behind another veth. Removing a veth also removes
its routes. These setup commands use the same generation-bound executor as the
other network mutations, so interrupted setup retains its cleanup obligation.

Adoption now requires the `/32` endpoint shape. Durable state 26 refuses older
development networks instead of assuming their connected routes are safe.


## Don't ask membership who you are

A worker retires a local endpoint. Before the next report reaches the leader,
the worker receives a catalogue that still advertises its old host port. If it
merges that entry back into its routing table, retirement has just undone itself.

Our merger already excluded entries from the local node. The mistake was asking
Raft membership for that node's name. A worker can lack council metrics, and a
joining node can have metrics before membership includes it. Neither changes its
configured identity. `ClusterHandle` now carries a `NodeId` directly from cluster
startup; the merger borrows its name regardless of the current membership view.
The existing `Option` still represents whether this is a clustered agent at all.

The integration test sends a delayed catalogue through the agent's command
channel without any council metrics. It checks the resolve response, the snapshot
used by DNS and the ingress backend pool: the remote endpoint survives and both
stale local endpoints disappear. This closes local self-restoration. It does not
prove another node has received a withdrawal; that needs separate acknowledgement
before an address can safely be reused.


## A stopped container can still own an address

The container exits normally. Runc tears down its namespace, and our address pool
makes its IP available again. Now freeze an old service's backend map before
that happens, and start an unrelated container. The old VIP returns HTTP 200
with the new container's identity. This is the natural-exit regression, using
real Runc, a real kernel map and HTTP responses from both workloads.

Stopping execution and releasing an address are separate facts. The opt-in owned
Runc adapter now records a `NetworkReference` before a service workload starts.
It contains the instance identity, runtime generation and original allocation.
The original intent keeps that reference in either `Held` or `Released` state.
`Option<NetworkReferenceState>` also distinguishes workloads that never held a
discovery reference. These are data-bearing enum variants: Rust makes each
variant carry the exact reference whose state it describes.

Natural exit still seals command admission, drains accepted commands and removes
host network resources. It can report that execution stopped. While discovery
holds the address, the durable runtime intent remains Retiring and the address
reservation remains occupied. A matching release first persists Released, then
lets resource retirement finish. A retry reads that receipt; a request from an
older generation cannot release its successor's reservation.

The agent captures the reference before startup. During retirement it confirms
backend withdrawal and policy cleanup before releasing the reference. It checks
the kernel even when userspace no longer lists that backend: a failed earlier
rewrite may have left it installed. It also avoids recreating a backend-map key
whose absence is already confirmed.

This does not turn missing service metadata into evidence. If a recovered runtime
still holds a reference but the agent lacks its original discovery ownership,
cleanup refuses rather than freeing the address. Complete service reconstruction,
remote catalogue acknowledgements and in-flight proxy requests remain separate
work before selecting this adapter in production. State format 27 and OCI intent
version 4 reject older development state that lacks this distinction.


## A routing test needs its own router

The host can ping each container, but one container cannot ping its neighbour.
Our explicit /32 routes are correct. A DROP policy on the host's forwarding
chain can produce exactly this result: host-originated traffic does not pass
through the same chain as traffic travelling between the two interfaces.

The route regression now runs its host and both container namespaces inside a
private outer network namespace. It also gets a private mount tree and `/run`,
so its named namespace mounts cannot escape into the real host. The original
host-to-container and bidirectional peer probes are unchanged. Running the test
inside another namespace with a DROP policy must pass while leaving that outer
policy untouched.

This isolates the routing claim. It does not promise that Bun overrides an
operator's firewall. Direct Linux installations must permit the required
forwarding; the managed VM path gives us a dedicated host configuration.


## Stop unsafe execution even when cleanup refuses

Now combine two failures. Remove a running container's egress enforcement flag
and freeze that map so the repair fails. Also freeze the service backend map.
The old agent tries to withdraw the backend before stopping the container,
receives an error and leaves the payload running without its required policy.
The real-container regression observes that it never stops within 15 seconds.

The retained address gives us another option. After ordinary cleanup fails,
the agent fences execution separately. For a published container address, it
first checks that the runtime still holds the original address reference. It
disables automatic restart, retires initialisers and force-stops the payload,
requiring positive exit evidence. Every affected replica gets an attempt even
if another replica refuses.

Nothing here confirms discovery cleanup. The service key, address reference,
policy ownership and adoption record stay in place. An explicit Stop still
returns an error while the backend map refuses withdrawal. The regression also
starts an unrelated container, proves it serves its own HTTP identity, and
checks that it neither reuses the retained address nor answers through the old
VIP. This is the opt-in owned runtime contract; complete recovery and production
selection remain separate work.


## Recover the allocation, not just the name

Two service names can hash to the same VIP. The second registration probes for
another address. Re-register those services in reverse order after a crash and
the assignments change. Existing kernel routes and grants still refer to the
original allocations. Our recovery regression finds two real colliding names,
reverses their saved inventory and demonstrates that ordinary registration loses
those original identities.

`ServiceMap::from_snapshot` restores each exact VIP, destination identity, port,
backend list and firewall configuration. It rebuilds the address reservations
from those entries. Invalid labels, mismatched identities, duplicate service or
VIP owners, invalid endpoints and duplicate or excessive backends reject the
whole inventory. Construction happens in a new local map; an error drops it
without publishing a partial result.

The argument `&[ServiceEntry]` is a borrowed slice: the function can inspect the
caller's entries without taking ownership. It clones validated entries into the
returned map. The typed error's `&'static str` reason refers to a fixed string
literal whose lifetime covers the whole programme.

This is a recovery primitive, not permission to route traffic. Saved health is
historical evidence. Bun must still load a durable checkpoint, correlate the
original runtime and kernel ownership, and reconcile live backends before
publishing DNS, ingress or kernel routes. That integration remains unfinished.


## Keep cleanup permission across the next crash

A container has exited, but its old VIP may still lead to its address. We must
retain that address until discovery has withdrawn it. Now suppose Bun crashes
between withdrawing the route and releasing the runtime reservation. Where does
the permission to finish cleanup live?

`DiscoveryJournal` saves a complete inventory in a private directory. Service
owners retain their exact allocated entries. Reference owners tie a service to
its original instance, runtime generation and network allocation. `Held` means
that discovery still owes withdrawal. `ReleaseAuthorised` records permission to
ask the runtime to release the address. The caller records that permission first,
performs the release, then forgets the completed reference. Recovery can replay
the permission against the same runtime generation.

These phases are Rust enums, so a transition has a named state rather than a
collection of flags. The journal rejects changing a retained allocation,
forgetting a held reference, or moving an authorised reference back to `Held`.
Services have a similar `Owned` to `Withdrawn` transition. The store cannot prove
that a kernel deletion, remote acknowledgement or runtime release happened.
Its caller must establish those facts before advancing the corresponding phase.

An owned `File` keeps the exclusive filesystem claim alive. Dropping the journal
closes the descriptor and releases the claim. JSON contains a schema number and
all owners together; `#[serde(deny_unknown_fields)]` on the envelope and owner
records makes unexpected fields an error rather than silently dropping them.
The loader also validates exact service allocations and original runtime
references. Only a newly created directory may initialise a claim. An existing
directory with a missing claim keeps refusing, even on repeated recovery attempts;
otherwise the first failed open could create the evidence that the next open trusts. It bounds reads at 16 MiB and refuses missing established state,
corrupt data, conflicting owners and redirected or non-private files.

Saving writes and syncs a temporary file, atomically replaces the checkpoint,
then syncs its directory. What if the last sync fails? The rename might already
have happened. The journal retains its previous in-memory obligations and refuses
further writes until recovery reopens the complete durable state. The failure
test forces replacement to fail, restores the original file and checks that the
same writer still refuses. Merely fixing the filesystem must not silently clear
its uncertainty.

This API performs blocking filesystem work. A future async caller must move the
journal into a blocking worker that retains its claim until the entire operation
finishes, even if the waiting future is cancelled. Bun does not use this store
yet. Publication gates, runtime correlation, remote withdrawal acknowledgements
and recovery qualification remain separate work. Passing storage tests does not
prove a recovered route is safe to publish.


### Move the journal, keep the claim

An async function can still block its executor. Calling the synchronous journal
write inside `async fn` does exactly that when storage stalls. Our regression
pauses a write and checks that the single-thread async runtime can still send a
heartbeat. A second test cancels the waiting caller, verifies another writer
cannot take the filesystem claim, then resumes storage and recovers the saved
obligation. Both tests fail with an inline write.

`open_async` and `persist` run filesystem work through `spawn_blocking`.
`persist(self, next)` consumes the journal: its `self` argument transfers ownership
instead of borrowing with `&mut self`. The `move` closure then owns the journal,
including its open claim file, until the write finishes. Cancelling the awaiting
future does not cancel that already-running blocking operation or release its
claim early. A successful awaited result returns the journal to the caller.
Failure or cancellation requires reopening and reconciling durable state; the
caller cannot continue using the moved value. Rust makes that last rule concrete.

Bun integration must distinguish an unopened journal from a consumed journal
whose write was never acknowledged. Treating both as permission to initialise
empty ownership would defeat the guarantee. These worker methods provide the
ownership boundary; wiring every publication and recovery path remains separate.


## Health changed; did routing change too?

An HTTP probe returns 503 while the kernel backend map refuses updates. Previously,
Bun marked its local backend unhealthy, printed the kernel error and advanced the
instance to Pending for restart. The kernel still routed to that instance. The
real regression observes restart count one even though withdrawal never succeeded.
A separate portable regression shows that successful health changes were not
published to the DNS/ingress snapshot either.

Now Bun builds a candidate health view and checks kernel publication before
replacing the service map and publishing the userspace snapshot. It records the
observed lifecycle health truth even when publication fails, but it does not
advance the restart state machine. The runtime and its cleanup evidence stay owned.

Every subsequent probe retries publication. Waiting for another lifecycle
transition would lose the retry: an already-Unhealthy instance remains Unhealthy
on the next failing probe. Once publication succeeds, that later probe may initiate
the bounded restart policy. A focused test first refuses publication without its
original allocation, restores that same allocation, then verifies the next probe
publishes withdrawal before increasing the restart count. Portless workloads do
not need a backend update.

This confirms the local health update. Remote withdrawal acknowledgements and
requests that already captured an ingress backend remain separate cleanup proofs.


## A replacement does not need another slot

A service already has 32 backends. Updating one instance's address, port or health
still leaves 32 backends, but the old insertion path checked capacity before
looking for that instance and rejected the update. The regression fills the map,
replaces one endpoint and then attempts a genuinely new endpoint.

The map now looks for an existing instance first. Rust's mutable iterator yields
a mutable reference to its endpoint; assigning through that reference replaces
its fields in place, then returns success. Only insertion checks the capacity.
The test verifies the updated endpoint, unchanged count, and that overflow refusal
leaves the entire retained entry unchanged.


### The first reader may arrive later

Deploy an app before attaching a discovery subscriber. Tokio's watch `send`
refuses the update when there are no receivers, leaving the channel's original
empty map in place. A later subscriber therefore misses a completed deployment.
The regression exercises exactly that ordering.

Publication now uses `send_replace`, which retains the latest value whether or
not anyone is listening. The first reader receives the current service and its
backend immediately. The existing health-transition and retry tests also run
against this publication path.

### Notification backpressure must not hold the drain lock

Fill the drain-completion channel, then let another backend finish. An awaited
send used to hold the shared tracker lock until the receiver made space. Request
guards need that same lock to release their counts. A notification consumer
could therefore stop unrelated request cleanup.

`try_send` returns immediately. Its `TrySendError` enum distinguishes a full
queue from a closed receiver. With a full queue we retain the pending completion
and retry on the next sweep; with a closed receiver, polling callers can still
observe completion. The regression fills a one-slot queue, checks that the sweep
returns and the entry remains, consumes the old notification, then verifies that
the next sweep delivers the retained one. No notification is silently lost to
backpressure. Capturing requests before drain starts and confirming their actual
release after deadline cancellation are separate requirements.


### Capture the request before withdrawing the route

A slow request reaches its backend. Then a deployment starts draining that
backend. Counting only requests that arrive *during* the drain misses this one
entirely. The live regression uses a gated HTTP server: it acknowledges receipt,
waits while the test starts draining, then answers only when released. A second
case reaches that server through failover. Both previously reported completion
while the request was still waiting.

Wrapper now takes the routing read lock, chooses its candidates and records a
request guard for all of them before releasing the lock. Bun's route withdrawal
needs the write lock, so it cannot pass a handler that has copied an endpoint
without recording its ownership. Normal requests create active entries; starting
a drain adds a deadline without resetting their counts. The guard conservatively
holds every captured failover candidate until the request finishes. WebSockets
capture just their single target because their handshake has no failover.

A deadline cancels work. It doesn't prove the work stopped. HTTP body reads,
upstream connection/header waits, response streaming and WebSocket handshakes
and splices all observe cancellation. `FuturesUnordered` polls the cancellation
futures for the captured candidates; the first one that resolves stops the
request. The tracker reports completion only after the guard releases both
connection counts. A cancelled task therefore keeps its ownership until its
cleanup actually runs.

Response streaming needs another detail. If a client stops reading, Hyper may
stop polling its response body. A cancellation check inside that body cannot
then run. A small spawned pump reads the upstream into a one-item channel and
races both reads and sends against cancellation. Dropping the response aborts
the pump through `AbortOnDropHandle`, an owning wrapper around the task handle.
That uses tokio-util's `rt` feature. The connection permit stays with the response;
the upstream guard stays with the pump. Buffered bytes need no backend address.
The stalled-consumer regression deliberately never polls the response and still
requires drain completion after cancellation. A live WebSocket regression also
requires the splice to close before completion.

This proves the local Wrapper ownership boundary used by deployment drains.
Durable discovery recovery and remote catalogue acknowledgement still need their
own proof before an allocator can reuse an old address.

### A cancelled response must fail at the client

The client has received seven bytes of a promised 100-byte response. A drain
then cancels its upstream. If the pump simply closes its channel, the response
stream sees ordinary EOF (end of file), and Hyper can finish the downstream
response successfully. Our live proxy regression catches exactly that: the
client accepts an incomplete response without an error.

The pump now returns a `Result<(), std::io::Error>`. Once its chunk channel closes,
the response stream checks that result before choosing successful EOF. A drain
cancellation becomes `ConnectionAborted`; a failed pump task becomes an error
too. Upstream read errors also retain their failure through the response body.
The stream emits a terminal error once, then ends. Normal complete responses
still end cleanly, covered by the large-response case.

This does not move backend ownership back into a possibly stalled downstream
reader. The pump releases its request guard when it finishes; the response owns
the pump's final result until the client can observe it. Completion of cleanup
and successful delivery are different facts, and both need honest reporting.

### Put the journal before publication

The checkpoint store cannot protect a publication nobody records. Bun now has an
opt-in fresh-agent integration: before acknowledging a backend snapshot, it
persists the exact service allocation and attempted backends, then writes the
kernel map. DNS and Wrapper still receive only the confirmed candidate. A storage
failure therefore stops initial deployment before runtime creation; the agent
refuses later publication even if somebody repairs the path underneath it.

The owner is an enum with three states: Disabled, Ready and Uncertain. Starting a
write uses `std::mem::replace` to move the Ready journal into its blocking worker
and leave Uncertain in the agent. Only acknowledged success returns Ready. This
makes cancellation fail closed without losing the worker's exclusive claim.
Disappearing from a new snapshot does not prove retirement: the next checkpoint
retains earlier service allocations until a separate confirmed cleanup can remove
them. Tests reopen the store after agent drop, reject a broken checkpoint before
launch, and retain an old service through publication of a different one.

Integrating the journal also exposed a useful Rust distinction. `Send` means a
value can move between threads; `Sync` means threads can safely share references
to it. Bun's async methods can hold `&self` across an await, so its fields must
support that sharing. Our test-only std mpsc receiver did not. A Tokio one-shot
receiver keeps the same pause/resume test but is safe to share; its blocking wait
still runs only in the blocking writer. The original responsiveness and cancelled
waiter tests remain part of qualification.

This entry point accepts a fresh journal only. Existing ownership, instance
records or runtime launches require full reconciliation before adoption. The
fresh-only recovery gate refuses those inputs before adopting, killing or deleting
runtime evidence. Production selection, original runtime-reference correlation,
withdrawal authorisation and remote acknowledgements remain unfinished. Enabling
this publication producer is not permission to claim those recovery guarantees.

### Journal the runtime's original address reference

An owned runtime can stop execution while retaining its container address for
old discovery consumers. Bun must remember *which* generation and allocation it
holds. Keeping that reference only in a HashMap loses the link when Bun dies.
The new regression starts a workload, drops the agent and reopens the discovery
checkpoint. Before the fix, its service existed but its runtime reference did not.

Bun now persists the exact reference after the runtime retains it and before
Start. The record includes the service, instance, original generation and
container index, with phase Held. A checkpoint failure leaves the runtime's hold
intact and refuses Start. Another regression blocks runtime creation, breaks the
checkpoint after initial service publication, then resumes creation. Previously,
Start still happened and only final backend publication failed.

The publication and reference operations use one update helper. It moves the
journal into its blocking writer and restores Ready only on acknowledgement.
The store's existing transition validation rejects replacement of an original
held generation. Bun also rejects a runtime returning another instance's reference.
Release requires an exact ReleaseAuthorised journal entry; a locally empty route
cannot manufacture that permission. Authorisation and recovery reconciliation
remain the next steps, so this opt-in profile stays outside production selection.

The physical Linux case uses an owned Runc container and a real eBPF VIP. After
controller-task loss it reopens the journal and compares the saved reference with
the runtime's original hold. The fixture then confirms that natural exit leaves
the old address unavailable to a successor while the retained backend exists.
This is controller-task loss, not the later required actual Bun SIGKILL gate.

### Permission must precede physical release

For an agent without cluster membership, successful local withdrawal and request
release supply the discovery proof. Before handing an address back to the runtime,
Bun checks the original service's VIP and port, confirms its backend is absent,
and persists ReleaseAuthorised for the exact original reference. Only then does
it call the runtime. After acknowledgement it removes the reference from the
checkpoint and its in-memory inventory. A failure between those writes leaves a
replayable permission, rather than an ambiguous missing owner.

The ordering test pauses the runtime's release method and reads the checkpoint.
It must already contain ReleaseAuthorised, while the runtime still holds the
address. Resuming release must remove the reference from both inventories. A
broken permission checkpoint must prevent the runtime call entirely. Clustered
agents refuse this local authorisation: they need remote withdrawal evidence,
even when this node's own routing is empty.

A physical owned-Runc/eBPF case then retires an application, observes the cleared
runtime/checkpoint reference, starts a successor on the same address and checks
that the predecessor's VIP cannot reach it. Service-allocation retirement and
recovery replay are separate remaining steps; these address permissions do not
silently forget the service's original VIP.

### Retire the service allocation as well

Stopping the last backend is not the same as retiring its service VIP. The
checkpoint used to retain an Owned allocation after a successful Stop, while the
in-memory allocator made that address available again. A subsequent allocation
could then disagree with the original durable owner.

Stop now checks the exact original VIP and port, requires an empty live backend
set and no remaining runtime references, and confirms release of any historical
ingress candidates. Its caller has already withdrawn the kernel service and
its destination grants. For a standalone agent, Bun first persists Withdrawn
with no backends, then persists removal of that owner. Only after both writes
succeed does it unregister the VIP. A failed write retains the allocation and
fences further journal mutations. A crash after Withdrawn leaves an explicit
retirement stage for recovery to finish.

Three failing-first tests cover successful removal, checkpoint failure and a
clustered agent without runtime references. That last case still refuses local
retirement: the absence of a runtime hold says nothing about remote consumers.
The original conservative-publication test now injects lost private metadata
without calling Stop, so it continues to prove that absence is not retirement.
The real Linux address-reuse case also checks that confirmed service retirement
leaves no service owner in the journal. Full recovery remains separate.

### A retry retires the old address, not the workload identity

An application can keep the same instance name across automatic restarts, but its
new runtime has a different generation. Previously the specialised retry cleanup
removed the old execution record and policy while leaving its network reference
held. The owned runtime correctly refused to create a successor over that owner.

After routing withdrawal, request release and confirmed runtime exit, retry now
clears the old policy and releases its exact address reference through the same
durable permission path as Stop. Only then does it remove execution evidence and
create the successor. Its identity bundle and mount remain: this is still the
same logical workload. A failed permission write prevents successor creation.
The successor's pre-start path records its new generation before allowing Start.

Two failing-first contracts exercise ordering and failed persistence. The real
Linux case kills a running owned container, waits for Bun's ordinary automatic
restart, checks that the journal contains the replacement generation and proves
the original VIP serves it. It then retires the service and checks safe address
reuse. This exercises a workload crash; actual Bun death during retry remains a
separate recovery gate.

### Correlate the two original inventories

Consider a crash after Runc records an address hold but before Bun acknowledges
its discovery checkpoint. The service allocation exists in one journal and the
runtime generation exists in the other. Recovery needs both. The runtime launch
inventory now includes its saved Held or Released reference alongside the
original OCI specification; a Process runtime reports no container address.

The discovery journal can validate that complete inventory and return a proposed
reconciled snapshot. Existing references must match the exact generation and
allocation. A runtime release requires prior ReleaseAuthorised permission. An
unacknowledged hold can be recovered only by matching its original cgroup and
container port to an existing Owned service allocation. Duplicate identities,
missing originals and published backends without holds refuse the entire result.
No routes, runtime calls or checkpoint writes happen during this correlation.

Why use the cgroup? `api-g5-0` can describe ordinal zero of app `api-g5`, or a
replacement of app `api` in generation five. The original OCI cgroup names the
structured owner. Parsing the runtime name and hoping is not enough. Likewise,
a released receipt stays paired with its discovery permission until a later
physical acknowledgement completes the operation. Correlation is evidence for
the recovery transaction, not permission to publish historical healthy backends.

The tests cover the gap between writes, absent and changed originals, premature
release, duplicate identities and ambiguous names. The real Runc recovery test
also checks that its complete inventory exposes the original held reference and
its eventual released receipt. Restoring allocations, withdrawing historical
routing and adopting only validated runtimes are the next integration steps.

### A failed lookup does not prove absence

A permission error reading the kernel backend map used to become `Ok(None)`.
The culprit was `.ok()`: it converts any `Result<T, E>` into `Option<T>`, dropping
the error. Cleanup could therefore treat an unreadable entry as already absent.
An explicit `match` now returns None only for Aya's KeyNotFound variant and
propagates every other failure. The regression first proves an absent key can be
read normally, then denies BPF syscalls in an isolated subprocess using seccomp
(a Linux syscall filter). Before the fix, that real denied lookup still reported
absence. The subprocess confines the filter so it cannot affect later tests.

### Recover before allowing adoption to mutate ownership

The standalone startup entry point now opens the original journal, requires a
complete runtime inventory and persists the correlated result. It reserves each
original VIP and port without publishing old backends. With kernel networking,
every existing backend key must belong to that inventory; an unknown key refuses
recovery before withdrawal. Bun then removes the original routes and destination
grants. The agent stays fenced until every withdrawal succeeds.

A Recovered journal state records that startup reconciliation happened and keeps
that distinction through later consuming writes. Before adoption changes runtime
state, Bun validates the original inventory again, restores policy and replays
pending release permissions. The normal runtime reconciliation can then retire
launches that never acquired an adoption record. Their original address hold is
now available to cleanup even though they have no supervisor entry.

After positive adoption, Bun obtains current container addresses and publishes
checked backend snapshots using the reserved allocations. A workload with a
health check starts in HealthWait with an unhealthy backend. Saved healthy status
is history. Empty services retire through the confirmed withdrawal checkpoints.

The entry point requires startup without concurrent runtime registration and
with all prior node consumers stopped. It refuses clustered recovery until remote
acknowledgements exist. Unit cases cover live adoption, unrecorded cleanup,
pending release, changed runtime evidence and historical health. The physical
Linux case loses the controller task, refuses an unknown kernel owner, then
recovers the same generation and proves VIP routing and later safe reuse.
Actual Bun process death, host reboot and production selection remain separate
qualification gates.

### A new report must not move an existing VIP

Two services can hash to the same natural VIP. Rebuilding allocations from a
sorted list on every update was deterministic, but it wasn't stable: adding an
earlier-sorting service could move an existing one, and removing the first owner
could move a collision-resolved service back to the natural address. Neither
change has anything to do with that service's runtime.

Catalogue reconciliation now reserves all retained allocations before assigning
new ones. It validates the original inventory and refuses duplicates, invalid
identities and exhausted address space. Fresh rebuild also returns Result;
exhaustion no longer silently shares a VIP. The leader uses the committed
catalogue as its starting point and includes every declared port-bearing service,
even when reports contain no backends. Allocation failure preserves the last
published catalogue while ordinary scheduling continues.

Four failing-first tests cover colliding arrivals, departures, conflicting saved
allocations and exhaustion. These stable active allocations are a prerequisite
for remote retirement evidence. They do not authorise reuse after service deletion;
retiring deleted allocations still needs the acknowledgement ledger.

### A successful write is not a permanent receipt

Suppose this scheduler publishes a catalogue, loses leadership, and later takes
charge again. Another leader may have changed the catalogue in between. Comparing
against a local copy of our last write can leave the wrong committed catalogue in
place indefinitely. A Raft request can also commit successfully while its state
machine returns `CouncilResponse::Refused`; transport success isn't application
success.

The scheduler now compares each candidate against the current committed catalogue.
It accepts only `CouncilResponse::Applied` as a successful publication and logs
other replies. No local success cache suppresses future retries. The integration
test uses a real single-node council: replace a previously published catalogue,
require the running scheduler to restore it, then check that unchanged state
produces no extra log entries. This is publication convergence, not proof that
remote consumers have withdrawn an endpoint.


### Remembering consumers that stop answering

A worker can disappear from gossip while still holding an old routing table.
Dropping it from the cleanup list would turn loss of contact into permission to
reuse somebody else's address. We now register each catalogue consumer in Raft
before returning its first placement response. The registration survives snapshots
and has no heartbeat expiry. Repeated polls don't append another registration.

`BTreeSet<String>` stores each node name once, in deterministic order. Unlike a
map, a set has no separate value; membership itself is the fact we need here.
The registration limit refuses new consumers without evicting existing ones.
Where credentials are configured, the internal service credential is required.
The existing credential-free development profile still registers every consumer;
this only adds an obligation and cannot discharge one. With TLS, the peer's validated node
identity must also match the requested node; the deliberately plaintext profile
trusts the cluster service credential. Neither an ordinary API user nor an invalid
certificate can create consumer records.

Operator decommission removes the fenced identity from this census and records
that fact in the immutable retirement result. A repeat returns the same result;
the retired name cannot register again. This follows the existing requirement to
stop or isolate workloads before decommissioning. Gossip suspicion and elapsed
time cannot substitute for that action.

Protocol 15/state 28 introduce the replicated consumer set and registration
request. Fresh development clusters are required. This census establishes who
must be accounted for; generation-specific withdrawal receipts and the gate that
checks them before releasing addresses are still separate work.

### Distinguishing successive executions without publishing their secrets

`default__api-0` can stop and restart under the same name. Its next execution is
a different owner, even if it happens to receive the same host port. A delayed
receipt for the first execution must never free the second one's address.

The runtime inventory now carries `RuntimeGeneration`, an opaque public
fingerprint of the original private execution record. ProcessGrill derives it
from the owner token, and owned Runc derives it from its original intent
generation. SHA-256 uses a different fixed prefix for each runtime, so identical
input bytes in different runtimes don't represent the same identity. The process
owner token authenticates its control socket. Publishing it directly would turn
a diagnostic identifier into permission to control the workload.

The newtype wraps a private String and exposes only its fingerprint through
`as_str()`. Reopening the runtime reproduces the same value; recreating the same
instance gets a new value. Discovery recovery also checks that this fingerprint
matches the original Runc address reference before accepting the inventory.
This change is in-memory runtime evidence only. Carrying it through reports and
catalogues, and requiring generation-specific retirement receipts, comes next.


### Keeping the original execution identity through the reporting path

The worker used to report only a replica ordinal. That loses deployment names
and runtime generations, so two executions using the same node and host port
look identical to remote routing. Reports and catalogue backends now also carry
the canonical instance name and its non-secret execution fingerprint.

Bun reads the original runtime inventory within a bounded deadline. It attaches
an identity only when the instance and complete original specification match.
Duplicate inventory entries, unavailable evidence and changed specifications
produce an unknown identity; resource commitments still appear in the report.
We don't invent an owner from an ordinal or a port number.

The reporting worker preserves this evidence, the leader includes it in the
catalogue, and remote routing includes the fingerprint in its backend key. A
replacement execution therefore has a different key even if it reuses the same
address. Unknown identities retain the older routing key and cannot serve as
retirement proof. Withdrawal acknowledgements remain separate work.

`TryFrom<String>` checks deserialised fingerprints before constructing the
newtype: exactly 64 lowercase hexadecimal characters. Its Result either contains
a valid identity or a short error, without echoing an accidentally supplied
private token. Protocol 16/state 29 describe the changed reporting wire and
replicated catalogue. Fresh pre-release clusters are required.


### A report timeout must not start another filesystem reader

A slow disk can outlast Bun's one-second inventory deadline. Tokio cannot cancel
an already-running blocking filesystem operation merely because its caller stops
waiting. Repeating that report could otherwise accumulate readers.

ProcessControl and owned Runc now share one inventory permit across their clones.
The reader acquires an `OwnedSemaphorePermit`, a token that keeps its semaphore
alive without borrowing the caller. An `async move` task takes ownership of that
token and keeps it until the read actually ends. Dropping the caller's join handle
detaches the task; it doesn't release the permit. A caller cancelled while queued
never starts its read. An I/O error releases the permit so the next report can retry.

Runc constructs a new intent journal for each read, so putting the semaphore in
that temporary journal would achieve nothing. It belongs to the shared runtime
ownership instead. The regression aborts one caller, times out a second, releases
the original operation, and verifies that the cancelled queued read never runs.
A separate case checks recovery after a failed read.

### Removing an endpoint creates an obligation

Suppose a worker disappears while another node still routes requests to its old
host port. Replacing the leader's catalogue doesn't erase that remote node's
copy. Reusing the port at this point could send a request to a different workload.

Council now commits withdrawal obligations alongside catalogue replacement. A
monotonic publication number identifies the outgoing catalogue. Each withdrawal
retains the original service allocation, removed backend execution identities
and the durable consumer identities that may still hold them. Later publications
and Raft snapshots retain those obligations. Nodes registering after a withdrawal
don't inherit it: registration must happen before a node can read a catalogue.

We compare destination ownership separately from health. A health change or a
reordered report doesn't create a new execution, and a partial replacement
shouldn't drain backends that remain in use. A HashSet indexes the node, address,
port and execution identity; deriving `Hash` for the execution newtypes lets Rust
hash those fields together. The stored withdrawal still carries the original
values, including the service port, for eventual kernel and routing removal.

The ledger bounds retained generations, service/backend exposures and outstanding
consumer confirmations. Hitting a limit refuses the publication without evicting
old evidence. `plan_publication` constructs a candidate value; Council installs
it only after all checks pass. `checked_add` returns None on sequence overflow,
so a new publication can never wrap around and reuse an old number.

Decommissioning follows the same rule. After the operator fences a node, Council
discharges that consumer's obligations. If the node also produced endpoints,
Council records their withdrawal for the remaining consumers before committing
any placement, lease or retirement changes. A failed ledger transition therefore
cannot leave a partially decommissioned node.

This is the replicated record, not permission to reuse an address. Consumers
still need durable local withdrawal and authenticated receipts, and producers
must require the committed result before release. The next integration also
reserves retired VIPs in the allocator. State format 30 records the new ledger;
protocol 16 is unchanged in this checkpoint, and pre-release clusters need fresh
state.

### A withdrawal keeps its VIP reserved

A durable withdrawal record is useful only if allocation honours it. The leader
now reserves both the current catalogue's addresses and older withdrawn VIPs
before allocating new services. This covers a service that disappears in the very
update that introduces a colliding newcomer. Existing active services keep their
allocations; a recreated service probes for another address while its previous
allocation still has remote consumers.

Council checks the same rule at publication. It validates service identities,
ports and unique in-range addresses, then rejects collisions with both retained
withdrawals and the withdrawals produced by this update. Refusal leaves the
committed catalogue and history unchanged. A candidate prepared from an older
view therefore cannot bypass reservations simply by proposing its own VIP.

`reserved_vips()` returns `impl Iterator<Item = VirtualIP> + '_`. The caller sees
an iterator of copied addresses, while `'_` ties its borrowed traversal to the
ledger's lifetime. The compiler prevents retaining that traversal after its
ledger disappears. Only withdrawn service allocations reserve VIPs; replacing a
backend does not move an otherwise active service.

These checks protect virtual address assignment. Consumer receipts and the
producer's physical address/host-port release decision remain separate work.


### Refusing a catalogue prepared against an older generation

The scheduler reads generation 12 and starts preparing a catalogue. Meanwhile,
another committed operation removes a node's endpoints and advances publication
to generation 13. The first scheduler mustn't overwrite that newer decision.

`PublishEndpoints` now carries `expected_generation` alongside its candidate.
Council compares it with committed state before checking or changing any
publication data. A mismatch returns Refused and leaves the catalogue and
withdrawal history untouched. This comparison and the eventual update happen
inside one Raft state-machine transition, so another writer cannot slip between
them. The scheduler sends the generation from the same desired-state snapshot
used to build its candidate; after refusal, its next tick reads current state.

Even an identical catalogue needs current evidence. A repeated old request may
have lost its reply, but that doesn't make its old generation current again. A
freshly read identical candidate is still a no-op and doesn't advance the
publication number. Snapshot recovery and endpoint removal during decommission
preserve this rule. The live scheduler regression also submits a stale writer
before checking that the scheduler converges without repeated unchanged writes.

This changes both the cluster request and stored Raft log representation, so the
compatibility boundary advances to protocol 17/state 31. Missing generations
cannot silently default to zero. Fresh pre-release clusters are required.


### Tell each consumer which original routes to withdraw

A worker saw generation 1, disconnected, and returned after two backend changes.
Sending only the latest catalogue loses the identities of the routes it may still
own. Its placement response now includes the current publication generation and
all outstanding withdrawal instructions for that worker. Each instruction retains
the original generation, service allocation, removed backends and whether the VIP
itself is retiring. A worker enrolled after a withdrawal doesn't inherit it.

The API builds the catalogue, generation and instructions from one cloned committed
state. It filters the ledger by the requesting node's registered identity and
copies only that consumer's instructions, without exposing the list of other
consumers. Registration and the existing system/TLS identity checks still precede
this response. Reading an instruction neither acknowledges it nor modifies Raft.

The Rust response uses a `Vec<EndpointWithdrawalInstruction>` for the ordered
instructions and each instruction uses a `BTreeMap<String, ServiceWithdrawal>`
for its original services. Both types own their values, so serialisation can't
observe a subsequent change to committed state. We deliberately omit
`#[serde(default)]` from the catalogue, publication generation and instruction
list: a missing field is an incompatible response, not evidence of no cleanup.
An explicitly empty instruction list remains valid. The HTTP contract advances
to protocol 18; durable state remains 31 because this adds no stored field.

The regression runs the real placement handler against Raft. Two original readers
retain both old generations, while a reader registered later receives only the
withdrawal it owes. Serialisation preserves original execution fingerprints and
repeated reads leave obligations unchanged. Durable consumer processing and
authenticated receipts are the next pieces; receiving instructions alone cannot
authorise the producer to reuse an address.


### A cleanup receipt belongs to one node and one generation

Worker A has withdrawn generation 12. Worker B is offline, and generation 13 has
also left the catalogue. A's receipt mustn't erase B's work or acknowledge 13.
Council removes A only from generation 12's consumer set. The withdrawal record
can disappear when that set becomes empty; the durable consumer registration
stays, so future publications still account for A.

`POST /v1/discovery/withdrawn` requires system credentials and a current node TLS
certificate. The request contains compatibility metadata and the original
generation, but no node name. The handler derives identity from the certificate
after a quorum-backed security read. A caller cannot nominate another consumer,
and a follower cannot forward the request under its own certificate. Missing,
foreign and revoked credentials refuse without modifying the ledger. Plaintext
development registration alone cannot authorise cleanup.

The handler returns success only for a committed `Applied` response. A refused
write, unavailable leader or timeout leaves the caller uncertain. A timeout may
still be followed by a commit, so retries must be safe: an already absent
historical obligation is a no-op. Zero and current/future generations refuse.
Generation numbers never repeat, so a delayed receipt cannot discharge a newer
withdrawal. Raft also rejects unregistered and permanently retired identities.

Removing a consumer first borrows its record with `get_mut`. Once the code has
finished using that borrow, Rust allows removing the now-empty record from the
map. The compiler tracks the last use of the borrow, rather than requiring it to
last to the end of the enclosing block. This lets us express the transition
without cloning the entire ledger or introducing a lock inside the state machine.

Tests exercise the HTTP handler with real signed certificates and a running Raft
node; the fixture supplies the same certificate extension as the TLS listener.
They verify that delayed receipts, late registrations and invalid credentials
leave unrelated obligations intact. A separate snapshot test proves that one
consumer's recorded receipt survives restoration and replay. These tests do not
claim that the consumer has durably withdrawn its local routes yet: wiring that
proof to this endpoint and gating physical producer release remain separate work.

The new Raft command advances compatibility to protocol 19/state 32. Pre-release
clusters must start fresh rather than replay an unknown durable command.


### Queueing a catalogue is not publishing it

The leader sends a new remote backend together with an invalid ingress rate limit.
Previously Bun replaced its catalogue, rebuilt only the valid ingress routes,
logged the error and refreshed DNS anyway. Meanwhile, the placement reconciler
had already treated sending the command as success. Different readers could see
different updates, with no useful failure result for the caller.

The cluster path now validates allocations and builds a separate candidate
routing table. An error discards that candidate and keeps the confirmed catalogue,
DNS snapshot and ingress configuration. After validation, Bun takes the routing
write lock and replaces the views without an intervening await. Identical updates
remain no-ops. The ordinary routing-table builder still reports invalid routes;
the cluster publisher chooses when to install its result.

`SyncClusterCatalog` now carries a `oneshot::Sender<Result<(), BunError>>`. The
sender carries a single reply, whose inner Result distinguishes successful
publication from a refusal. The receiving future can itself fail when its sender
is dropped, so a lost reply is not success either. The placement loop puts one
deadline around both queueing and confirmation, and honours shutdown while it
waits. It retries refusals, lost replies and timeouts before performing the next
placement work.

The regressions first reproduce a changed catalogue after routing refusal, an
invalid VIP accepted into the views, and a deployment queued before the publisher
replies. They also verify a corrected retry and distinguish an explicit refusal,
a lost reply and a positive confirmation. Existing fake agents must now send the
confirmation too; otherwise a test would accidentally model a stalled publisher.

This is an in-process contract, so protocol 19/state 32 remain unchanged. It is
also only publication confirmation. A request captured before the table swap may
still own an old backend. Durable consumer records, confirmed draining and receipt
submission are the next steps; this reply alone cannot authorise address reuse.


### Remembering what a consumer might have published

A consumer can die after installing a route but before reporting success. On
restart, the latest catalogue might already omit that backend. We therefore need
the original attempted publication, including its execution fingerprint, alongside
the exact merged local and remote service view. Saving only the latest desired
catalogue would lose the evidence needed to withdraw the old route.

The discovery journal now accepts a consumer identity and an ordered history of
publication attempts. The identity binds the enrolled node to a fingerprint of
its cluster trust identity; rotating leaf certificates must not change that
binding. Each attempt keeps its committed generation and original catalogue.
`Option<ConsumerOwnership>` lets standalone inventories express the absence of a
cluster consumer explicitly. Generation zero is valid only for an empty catalogue.

Validation reconstructs each effective service map, checks allocation uniqueness,
and requires every remote catalogue backend to appear with its original VIP and
port. Additional local entries remain part of the recorded exposure. Adjacent
attempts may share a generation when only the local view changes, but they cannot
rewrite the catalogue or move backwards. Rust's slice `starts_with` method checks
that an update preserves the whole previous history. Derived `PartialEq` and `Eq`
compare the nested records by value, including backend health and execution IDs.

This foundation deliberately refuses history removal. It caps retained attempts
at 1,024 and combined service/backend records at 65,536, in addition to the
journal's 16 MiB limit. Reaching a limit refuses a write; it never evicts uncertain
ownership. Confirmed-withdrawal phases and safe compaction still need implementing.

The existing journal provides exclusive ownership and cancellation-safe blocking
writes. If the async caller disappears, the worker retains its claim until the
write completes. A failed write fences that writer until reopening and recovery.
Tests interrupt both paths and compare the recovered original evidence. Other
tests reject lost history, changed enrolment identity and conflicting effective
views without changing the checkpoint. Standalone recovery and fresh enablement
refuse consumer evidence they cannot reconcile.

The checkpoint schema advances to 2 and durable state to 33; protocol remains 19.
Publication does not yet call this storage path. Connecting it before publication,
recovering original exposures, confirming drainage and sending receipts remain
separate work. A saved attempt alone grants no permission to reuse an address.

### Keeping delayed catalogue replies behind the current generation

A consumer installs catalogue generation four, then receives a delayed reply for
three. The server already protects against stale catalogue writers, but that
protection does not order replies received by Bun. Previously the reconciler
threw away the response's generation when it sent the catalogue to the agent.
An old response could therefore restore a withdrawn backend.

`SyncClusterCatalog` now carries the committed generation. Bun remembers the last
confirmed generation as `Option<u64>`: `None` means it has not confirmed a cluster
publication in this process, while `Some(0)` represents a confirmed empty initial
catalogue. Those are different states. The `is_some_and` method runs a predicate
only when the option contains a value; the closure receives that value and returns
whether the incoming update conflicts with it.

Bun refuses lower generations and a changed catalogue claiming the same generation.
Zero requires an empty catalogue. Validation or routing failure does not advance
the fence, so a corrected update can retry. An identical catalogue at a higher
generation still advances it: equality of contents does not make the intermediate
publication history disappear. Ingress configuration has its own desired-state
changes, so an unchanged catalogue may still update ingress at the same catalogue
generation. This fence does not order those independent ingress changes.

The publisher also validates the effective merged service map before changing any
view, including the otherwise identical-update path. A valid remote catalogue
can collide with a local allocation after merging. In that case Bun keeps the
previous publication and refuses the update; it does not silently move a VIP
that another owner may still use. Regressions check catalogue, DNS and ingress
retention, corrected retries, identical-generation advancement and merged-view
conflicts. A real HTTP polling test checks that the response generation reaches
the agent command.

This fence is in memory. Restoring it from the durable consumer journal before
new publication remains part of cluster recovery integration. It neither proves
that old requests have drained nor authorises an address release. The command is
internal, so protocol 19/state 33 remain unchanged.

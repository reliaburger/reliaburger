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
8. Accept the containers' traffic in the host's iptables FORWARD chain, because Docker and ufw set its policy to DROP (chapter 9 has the story)

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

### Handing out addresses you can take back

Where does `container_index` come from? The first version incremented a `u16`. A /23 has 510 usable addresses and the gateway takes one, so a node has 509 container slots, and a counter knows nothing about that. Keep deploying and it walks onto the broadcast address, then into the next node's subnet. Restart Bun and it starts from zero, straight onto addresses live containers are still using.

So indices come from a small journal, `.network-leases.json`, next to the runtime state. Bun records an instance's index *before* running any network command, writes the file the careful way (temporary file, `fsync`, atomic rename, `fsync` of the directory) and reloads it under a file lock on every change. A full pool refuses the deploy before it creates anything. A lease goes back only after cleanup has checked the kernel and found the namespace, the host veth and any port forwards gone; if teardown can't prove that, the address stays reserved.

The other half of "whose address is this" is routing. Our first cut gave every host-side veth the node's whole /23 as a connected network. With one container that works. Add a second and Linux has two interfaces claiming the same subnet, so the host's packets for container two go out through container one's veth. Now each endpoint gets a `/32` address, the host installs an explicit route to each container through its own veth, and each container gets a route to its gateway plus a default route through it. The /23 is still the *allocation* pool; it just isn't a network anybody claims to be directly attached to.

These `ip` commands run through the same owned command executor as the runtime itself (Chapter 1), for a reason we found by killing Bun at the worst moment: with an `ip link add` still waiting to run, a new Bun looked for the veth, didn't find it, and treated the slot as clean. Then the old command woke up and created it. Recovery now retires the old generation's commands before it inspects the kernel, and only then does absence mean anything.

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
2. A `createRuntime` OCI hook (Bun's own binary, in a hidden mode) receives the container's init PID and holds creation until the network is ready, so the payload never starts without one
3. Bun starts `slirp4netns --configure --mtu=65520 --disable-host-loopback --api-socket {socket} ... tap0` against that namespace
4. The container gets IP `10.0.2.100` with gateway `10.0.2.2`
5. Port forwarding uses slirp4netns's API socket — we send JSON commands to map ports

The `--disable-host-loopback` flag is important: it prevents the container from reaching services on the host's loopback. Without it, a compromised container could probe the host's `localhost`-only services.

The helper is a process, not kernel state, so it needs an owner that outlives any one Bun. It runs as another role of the same durable runtime record as the container's launcher (Chapter 1 builds that owned lifecycle). Before it execs `slirp4netns`, the helper opens the container's user and network namespaces through `/proc/<pid>` and hands slirp those file descriptors rather than a PID. A PID can be recycled between the check and the use; an open namespace descriptor can't change what it points at. When a replacement Bun adopts the container, it asks the surviving helper for its forwarding table and refuses anything that differs from the original port mapping instead of "fixing" it.

What happens when the helper dies? Every `state()` call supervises it: if the helper's owner reports it gone, Grill starts a new one and reinstalls the forward. The awkward case is the gap in between. The owner polls its child every 10 ms, so for a moment it still says "running" about a helper whose API socket already refuses connections. Our first version treated that moment as fatal. It waited for the API, saw the owner finally report the exit, and returned "helper exited before readiness", quoting the *old* helper's log. A CI flake caught it three times, always under load. The fix is to stop asking "did it die?" and ask "did it die before we could use it?" A helper that dies after we saw it running is simply replaced. A helper this call started gets three attempts, with a short backoff between them. Only if all three die before serving their API does `state()` report an error, now quoting the latest attempt's own stderr. Refusals such as a conflicting forward still fail straight away, because retrying can't fix them.

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

`expected_generation` is a compare-and-swap: the state machine refuses the write unless the catalogue's publication counter still matches the value the scheduler read, so a scheduler working from a stale view can't overwrite a newer decision. Applying it records which endpoints disappeared and replaces `DesiredState.endpoint_catalog` — a wholesale swap, so the leader is the single source of truth and a follower never merges half a view. Because it lives in `DesiredState`, it rides the same replication and snapshot machinery as every other cluster fact, and it survives a leader change for free: the new leader inherits the last catalogue and republishes from its own reports on the next tick. The leader only writes when the catalogue actually changed, so a steady cluster isn't churning the log every couple of seconds.

The last hop is getting the catalogue *into* each node's resolution path. Every node's reconciler polls the leader's `/v1/placements/{node}` endpoint every couple of seconds to learn its assignments. We piggyback the catalogue on that response (after recording the poller in Raft as a consumer of it, for reasons we'll get to at the end of the chapter). Council voters also hold the replicated `DesiredState`, but they install routing through the same reconciler so there's one path to test. The reconciler hands the catalogue to its Bun agent, which overlays it onto the local service map:

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

#### Silence isn't absence

"The new leader inherits the last catalogue and republishes from its own reports" hides a trap, and the V02 soak walked straight into it. A new leader's report aggregator starts empty on purpose: reports received under the old term don't count as current truth, so a stale report can't convince the scheduler that something still runs. For the first few seconds of a term the leader has heard from almost nobody. Its first catalogue rebuild therefore said that nearly every service had no backends, it committed that, and every node dutifully installed it. The connect hook, faithful as ever, answered `EPERM` for every VIP whose backends had "vanished". In the soak a Redis client on the same node as Redis got `Operation not permitted` for two seconds after the leader was killed, and a client elsewhere lost Redis for much longer when the Redis node's Bun was SIGKILLed mid-deploy and took its time to report again. The containers never stopped. Only the leader's knowledge of them did.

The fix is a rule that the scheduler already follows for placement (an unheard node keeps what it runs): *a node that hasn't reported under this leader keeps the backends the committed catalogue gave it*, as long as gossip still counts it as a member at the same address:

```rust
for backend in &service.backends {
    let node_id = NodeId::new(&backend.node_id);
    let still_there = members.iter().any(|member| {
        member.node_id == node_id
            && matches!(member.state, NodeState::Alive | NodeState::Suspect)
            && member.address.ip() == std::net::IpAddr::V4(backend.node_ip)
    });
    if still_there
        && !reports.reports.contains_key(&node_id)
        && !desired.producer_retirements.blocks(&backend.node_id, backend.execution.as_ref())
    {
        backends.push(backend.clone());
    }
}
```

`matches!` is the boolean form of `match`: it's true when the value fits the pattern, and `Alive | Suspect` is one pattern with two alternatives. It saves a four-line `match` that returns `true` or `false`.

Everything that should still remove a backend still does. The node's own report is authoritative the moment it arrives, even when it names nothing. A Dead or Left member, or one that came back at a different address, can't be serving there. A producer retirement (the node asking to release an address) still withdraws the backend, so carrying it forward never blocks an address release for longer than the retirement protocol already does. What we gave up is a small window in which a node that silently lost an instance *and* hasn't reported yet keeps advertising it. A connection there gets `ECONNREFUSED` from a closed port, which is what a crashed backend looks like anyway, rather than `EPERM` from a hook refusing a service that's alive.

### Why userspace DNS, not eBPF?

The original design called for fully in-kernel DNS interception using `cgroup/sendmsg4` and `cgroup/recvmsg4` eBPF hooks. We tried it. It doesn't work *at those hooks*.

The problem is that these hooks let you modify the *destination address* of a UDP sendmsg, but they can't read the *packet payload*. You can redirect where a DNS query goes, but you can't parse the query name or synthesise a response. The BPF helper you'd need (`bpf_msg_pull_data`) only works with `SK_MSG` programs (stream parsers for TCP), not cgroup socket address hooks.

That isn't the same as saying eBPF can never answer DNS. TC and XDP packet hooks can read and rewrite packet bytes. The review proof in `poc/dns-tc/` did exactly that: a bounded TC-ingress parser answered mapped `.internal` A queries, repaired checksums and redirected replies into a container namespace without sending matched queries upstream. It also showed the bill. IPv6, TCP fallback, fragments, EDNS, lifecycle, map ownership and observability all become a second DNS implementation we would still need to back with userspace for other runtimes. So TC is feasible, but it isn't the simpler production design today.

So we run a userspace DNS responder instead. It lives in `src/onion/dns.rs`: a `tokio::select!` loop reading from a UDP socket, plus a TCP listener for large answers. Bun configures containers' `/etc/resolv.conf` to point at the responder, and it handles the rest. For `.internal` names, it looks up the service map and responds. For everything else, it forwards to the upstream resolver.

Names are namespace-qualified: `<app>.<namespace>.internal`. A query for
`api.payments.internal` resolves the `api` service in `payments`, and
`api.default.internal` resolves the other `api`, each to its own VIP.

What about a bare `redis.internal`? Our first answer was "the node's default namespace", which is convenient right up until two tenants on one node both run Redis. A short name only means something if we know who asked. So the responder looks up the packet's source address in a table that Runc maintains: it binds each container address to its namespace when it creates the network, before the workload starts, and withdraws the binding before teardown. The table travels to the DNS task over a `watch` channel, the same trick we'll use for the service map below. An unknown or ambiguous source gets `REFUSED` for a short name; qualified names work for everyone, and no `.internal` query is ever forwarded upstream. The old `dns.default_namespace` setting is now a config error, because an operator's preference can't tell us which workload sent a packet.

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

A later audit sent a question with the last byte of its class field missing. We answered it anyway. The hand-rolled decoder read the name and type, trusted the header's counts, and assumed the rest was fine. Rather than patch our own parser one hole at a time, we swapped it for Hickory's protocol codec (just the wire format, with no resolver or DNSSEC engine attached) and kept only the policy on our side:

```rust
let mut decoder = BinDecoder::new(packet);
let message = Message::read(&mut decoder).ok()?;
if !decoder.is_empty()
    || message.metadata.op_code != OpCode::Query
    || message.queries.len() != 1
    // ... more admission checks ...
```

`&mut decoder` lends the decoder exclusively to `Message::read`, which advances its cursor as it parses. `.ok()` turns the `Result` into an `Option`, dropping the error, and `?` on an `Option` returns `None` from `decode_query` early. Here `None` means "drop this packet", never "answer with nothing". After parsing we insist the decoder consumed every byte (`is_empty()`), so trailing junk is an error rather than something we politely ignore. The test corpus includes every truncation of a valid query, a compression loop, overlong names and a literal dot inside a label, and each case must be followed by a valid query that still succeeds.

The Rust-flavoured part is how the responder reads the service map. The agent *owns* its `ServiceMap` — it mutates it freely inside its event loop, no locks. Sharing it with the DNS task via `Arc<RwLock<…>>` would have meant threading lock acquisitions through dozens of agent call sites. Instead the agent publishes a *snapshot* on a `tokio::sync::watch` channel every time the map changes (the same moment it rebuilds the routing table). A watch channel holds exactly one value — the latest — and the responder reads it with `borrow()`, no await, no contention. The cost is a clone of the map per change; deploys are rare and maps are small, so that's a bargain for keeping the agent lock-free. Go programmers will recognise the shape: don't share memory, communicate — except here the borrow checker enforces it.

Containers find the responder through an old-fashioned mechanism: `/etc/resolv.conf`. Runc writes a per-instance file in the OCI bundle and bind-mounts it read-only at `/etc/resolv.conf`; it never edits the shared unpacked image rootfs. The file names the node-side veth gateway, not host loopback. `resolv.conf` has no port syntax, so only port 53 is valid.

Kubernetes manifests don't say `redis.internal`, though. They say `redis:6379`, or `redis.default` to cross a namespace, because a pod's `resolv.conf` carries a search list. Ours does too now, for a container in `payments`:

```text
nameserver 10.0.2.1
search payments.internal internal
options ndots:2
```

A resolver appends each search suffix in turn to any name with fewer than `ndots` dots, before trying it as written. So `redis` becomes `redis.payments.internal` (the caller's own namespace), and `redis.default` becomes `redis.default.payments.internal` and then `redis.default.internal`. That first attempt can't exist, and the responder has to say so with NXDOMAIN. It used to answer REFUSED, and glibc and musl both treat REFUSED as "stop searching", so `redis.default` quietly failed. Why `ndots:2` rather than Kubernetes' 5? With 1, `redis.default` would go upstream as written before the search list, leaking an internal name to the public resolver. With 5, `api.stripe.com` would pay three local misses before its real lookup. Two covers both short forms and costs a two-label external name like `example.com` two node-local NXDOMAINs.

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

Every health tick checks that both hooks are still attached and the enforcement map is intact. One observation drives two decisions: stopping workloads that need enforcement the kernel no longer provides, and the readiness the node advertises for that tick. Using a single reading for both means the scheduler never hears "enforcing" from a node that has just stopped its workloads for not enforcing.

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

Wrapper runs inside Bun as a set of async tasks — not a separate process, and not a separate runtime either. We originally planned to give it its own tokio runtime with its own thread pool, so a flood on port 80 couldn't starve gossip, Raft or the health checker. The config even had a `worker_threads` knob for it. Nothing ever read the knob, and a setting that pretends to bound resources is worse than no setting, so it's gone. Wrapper shares Bun's one runtime (the project rule: never create a second one), and its protection comes from bounding the work instead.

A concurrent connection limit (default 10,000) rejects new connections with 503 once the cap is hit. So a DDoS attacker faces per-IP rate limiting, a global connection ceiling and bounded TLS handshakes, which together cap how much of the runtime a flood can occupy.

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

### Count the request before you hide the route

The permit fixed WebSockets that started *during* a drain. It said nothing about a slow request that reached its backend a second *before* the drain began. The tracker had never counted it, so the drain reported "complete" while the request was still waiting for its answer, and Bun stopped the container underneath it. A gated test server that acknowledges a request and then waits to be released made this easy to reproduce.

Now the handler takes the routing table's read lock, picks its candidate backends and records a drain guard for all of them before letting go. Withdrawing a route needs the write lock, so Bun can't slip a withdrawal between "this handler copied an endpoint" and "this handler is counted against it". While no backend has answered, the guard holds every failover candidate, because any of them might still get the request.

Once one answers, the others are spectators. We first kept them captured for the whole response, and a review spotted what that does to a rolling deploy: a long server-sent-events stream served by healthy backend A also held failover candidate B. B's drain couldn't finish, and at B's deadline the cancellation meant for B tore down A's perfectly good stream. So the proxy narrows its ownership the moment a response arrives from candidate `idx`:

```rust
let mut drain_guard = drain_guard;
if let Some(guard) = &mut drain_guard {
    guard.keep_only(idx);
}
let terminate: Vec<_> = terminate.into_iter().nth(idx).into_iter().collect();
```

`if let Some(guard) = &mut drain_guard` borrows the guard mutably *inside* the `Option` without taking it out, so `keep_only` can edit it in place. `keep_only` moves the unused instance ids into a throwaway `DrainGuard` and drops it at once; its `Drop` releases them exactly as a finished request would, so there's one release path, not two. The last line keeps only the serving backend's cancellation token: `into_iter()` consumes the vector, `nth(idx)` yields an `Option` holding that one token, and `into_iter().collect()` turns the `Option` back into a zero- or one-element `Vec`.

One more distinction took a couple of rounds to get right. A drain deadline *cancels* work; it doesn't prove the work stopped. The tracker reports completion only after the guards have actually been dropped, and a response body that was cut short by cancellation ends with a `ConnectionAborted` error rather than a clean end-of-stream, so the client can tell it got half a response. Completion of cleanup and successful delivery are different facts, and each gets reported honestly.

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

## An address is only free when everyone has let go

Here's an experiment that should worry you, run against our first version. A container exits normally. Runc tears down its namespace, and the lease journal marks its IP free. Meanwhile an old entry in the kernel's backend map still points a service VIP at that IP (we froze the map in the test, but a failed update or a slow remote node does the same thing). Now start an unrelated container. It gets the recycled address, and the old VIP answers HTTP 200 *with the new container's identity*. Nothing crashed. Traffic just went to the wrong workload.

Stopping a process and releasing its address are two separate facts, and everything in this section follows from keeping them apart. An address stays reserved while anything might still route to it: this node's kernel map, a request Wrapper captured, or another node's copy of the catalogue. Only when every one of those has positively let go does the address go back into the pool. When we can't tell, we keep it. Running out of addresses is an outage you can see; routing to the wrong container is one you can't.

### Writing it down before doing it

A node's half of this lives in a small discovery journal on disk. It records every service allocation Bun has published or tried to publish (exact VIP, port and backends), and every container address a runtime is holding on discovery's behalf. The rule is the same one the lease journal follows: write the journal *before* touching the kernel map, DNS or Wrapper. Then, after a crash, the saved state is always at least as large as what might actually be exposed, and recovery can clean up from it.

Each held address goes through two phases:

```rust
pub enum ReferencePhase {
    /// At least one publication obligation still retains this address.
    Held,
    /// Withdrawal was confirmed; runtime release may be performed or replayed.
    ReleaseAuthorised,
}
```

(In the code's vocabulary, an *obligation* is simply cleanup somebody still owes.) To release an address, Bun checks that the backend really is gone from the live service map, writes `ReleaseAuthorised` to disk, *then* asks the runtime to release it, and only after the runtime acknowledges does it forget the entry. Crash anywhere in that sequence and the next Bun finds either a `Held` entry (it hasn't released yet, and must prove withdrawal again) or a `ReleaseAuthorised` one (it can replay the release, even if the leader is unreachable). There's no state where the address is free but nobody remembers why.

Saving is synchronous file I/O, which must not run on the async executor, and it must not be abandoned halfway if the caller gives up waiting. The journal handles both by giving itself away:

```rust
pub async fn persist(mut self, next: DiscoveryInventory) -> io::Result<Self> {
    tokio::task::spawn_blocking(move || {
        self.save(next)?;
        Ok(self)
    })
    .await
    .map_err(io::Error::other)?
}
```

Look at the first parameter. Most methods take `&self` or `&mut self`, a borrow. This one takes `self`, so calling it *moves* the journal into the function and the caller no longer has it. The `move` closure then takes the journal (including its open lock file) into the blocking thread, and only a successful save hands it back as `Ok(self)`. If the awaiting future is cancelled, the blocking write keeps running and keeps the lock until it finishes. If the save fails, the caller doesn't get the journal back at all. Rust turns "you may not keep writing after an uncertain save" from a comment into a compile error.

The agent has to hold *something* while the journal is away. It keeps an enum:

```rust
let (DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)) =
    std::mem::replace(&mut self.discovery_ownership, DiscoveryOwnership::Uncertain)
else {
    return Err(failure("discovery ownership changed during update".into()));
};
match journal.persist(next).await {
    Ok(journal) => { /* put Ready or Recovered back */ }
    Err(error) => { /* stay Uncertain; reopen from disk later */ }
}
```

`std::mem::replace` swaps a new value into a place and hands you the old one, which is how you move a value out of a struct field you only have `&mut` access to. The `let PATTERN = value else { ... };` form is a *let-else*: if the pattern matches, its bindings (here `journal`) stay in scope after the statement; if not, the `else` block runs and must leave the function. The `|` inside the pattern accepts either variant, as long as both bind the same names. So while a write is in flight the agent is `Uncertain`, and anything that tries to publish or release meanwhile gets refused.

Our first version had a nasty flaw here. It failed closed, but nothing ever reopened the door: one failed write (a full disk for a second, say) left the agent `Uncertain` until someone restarted Bun, publishing nothing while its health check said all was well. Now the agent remembers which directory it owned and, on every tick, reopens the journal from disk and adopts whichever checkpoint is durable. Because the journal is always written before the change it describes, either the old checkpoint or the new one is a safe description. While it's stuck, a critical readiness subsystem called `discovery:journal` reports the failure, so the scheduler stops sending work to a node that can't publish it.

### Recovering after Bun dies, or the machine does

On startup Bun reopens the journal, compares it against the runtime's own inventory of original launches, and reserves every old VIP *without* republishing the old backends. Saved health is history, not evidence. A workload that survived gets its backend back only after positive adoption, and it starts unhealthy until its health check passes again.

The kernel side has its own twist. If Bun dies but the machine doesn't, the pinned eBPF maps and cgroup links are still live and must be adopted. If the whole machine loses power, they're gone. An empty pin directory means completely different things in those two cases, so the kernel ownership record stores the Linux boot ID. Same boot: every pin must be present. Different boot: every pin must be absent, and we rebuild. Anything in between refuses to start. So does a missing lock or manifest file; quietly recreating one would give two processes an "exclusive" claim each.

### Other nodes hold copies too

Everything so far is local. But the endpoint catalogue went to every node, and a node on the far side of a partition may still route to an address long after this one withdrew it. The leader replacing its catalogue doesn't erase anybody else's copy.

So the council keeps a ledger of withdrawals. Every node that polls for placements over mTLS is recorded in Raft as a *consumer* before it receives its first catalogue. When a publication removes endpoints, the leader records which backends left and which consumers might still have them:

```rust
pub struct EndpointWithdrawal {
    /// Qualified service identities with their original destinations.
    pub services: BTreeMap<String, ServiceWithdrawal>,
    /// Durable node identities; absence from gossip does not discharge them.
    pub consumers: BTreeSet<String>,
}

pub struct EndpointWithdrawals {
    /// Generation of the active catalogue, starting at zero before publication.
    pub generation: u64,
    /// Original publication generations, retained until every consumer confirms.
    pub pending: BTreeMap<u64, EndpointWithdrawal>,
}
```

A `BTreeSet` is a sorted set: each node name appears once, in a deterministic order, which keeps Raft snapshots reproducible. Each consumer finds its outstanding withdrawals in the next placements response, removes those routes locally (journal first, as above) and then posts a *receipt* to `/v1/discovery/withdrawn`: an authenticated "I've removed everything from generation 12". The server takes the node's identity from its TLS certificate, never from the request body, so one node can't answer for another. The consumer keeps the receipt queued until it gets exactly a 204; a timeout or a lost reply just means sending it again, which is harmless. When a generation's consumer set is empty, its entry disappears.

The V02 soak found a way to make a receipt impossible to send. Cut the power to the *leader* and the new one builds its first catalogue before every node has reported, so node 1's backends briefly vanish: a withdrawal is recorded, and a moment later node 1's report arrives and the same backends, same executions, are published again. Nodes 1 and 2 were polling throughout and confirmed. The node that had been cut came back to a catalogue that already listed those backends again, and a receipt is proven only when no view still exposes anything the withdrawal removed. It never would be. Worse, its own startup cleanup was queued behind that very withdrawal, so it refused every deploy, for good.

The generation numbers say which reading is right. A withdrawal's generation is the last catalogue that exposed the removed backends, so a catalogue *newer* than that which lists one of them again means the leader deliberately published it again. It's live, not a leftover exposure. A view now only counts a backend match against a withdrawal when the view isn't newer than it. A producer that really retires an execution fences it first, so it can't come back, and any later removal gets a withdrawal of its own.

Withdrawn VIPs stay reserved while their entry is pending:

```rust
pub fn reserved_vips(&self) -> impl Iterator<Item = super::vip::VirtualIP> + '_ {
    self.pending
        .values()
        .flat_map(|withdrawal| withdrawal.services.values())
        .filter_map(|removed| removed.retire_vip.then_some(removed.service.vip))
}
```

The return type says "some iterator yielding `VirtualIP`s" without naming the concrete type (`impl Trait` in return position), and `+ '_` says that iterator borrows from `self`, so the compiler won't let you keep it after the ledger is gone. `bool::then_some(x)` turns `true` into `Some(x)` and `false` into `None`, which is exactly what `filter_map` wants. Publication generations only ever go up, and the planner advances them with `checked_add(1)`, which returns `None` on overflow instead of wrapping, so an old receipt can never match a new generation.

The producer's side mirrors this. Before a node frees the host port or container address of a stopped instance, it asks the leader to *retire* that exact execution at `/v1/discovery/retire`. The leader records a permanent retirement (the code calls it a *fence*: any late report from that execution is ignored from then on, rather than put back in the catalogue) and answers "confirmed" only once every consumer has sent its receipt. Executions are identified by a runtime generation, an opaque SHA-256 fingerprint of the original launch record. A restart gets a new fingerprint even if it reuses the same name and port, so a delayed receipt for the old execution can't free the new one. The request runs as its own task with a one-second wait, so an unreachable leader makes retirement slow, not the whole agent.

To make a growing backlog visible before the ledger fills, the leader degrades a `discovery:withdrawal-backlog` readiness subsystem at 75% occupancy and exports `discovery_withdrawal_ledger_occupancy_ratio` for alerting.

### When a node goes quiet

What happens when a consumer never sends its receipt? For a long time the answer was: we wait. A partitioned node that's still alive may still be routing to old addresses, and handing those addresses to someone else is worse than asking an operator to confirm the node is really gone. Decommissioning it discharges what it owed.

Then we recorded the homepage tour. Step 12 stops one of the three laptop nodes and invites you to watch the cluster put its frontend back. It didn't. Node-1 had started two new frontends and was retiring its old one, and the leader answered every release with "202, still waiting for node-3". Node-3 was a stopped virtual machine. It was never going to answer, and nothing else in the cluster could release an address until it did.

Waiting forever for a stopped node is safe, and useless. But silence is the only thing the leader can actually observe, and silence alone proves nothing: a stopped node and a partitioned one look identical from the outside. So we turned the question around. Instead of the leader deciding when a node has stopped routing, the node promises in advance to stop routing if the leader stops answering, and the leader waits a little longer than that promise. It's a lease, and its two halves never talk to each other.

The node's half is a `ViewLease`. Just before each placement poll, the reconciler reads the clock. Once the leader's answer has been published into the kernel map, DNS and Wrapper, the lease is extended to that reading plus 60 seconds:

```rust
pub struct ViewLease {
    enforced: AtomicBool,
    /// Expiry on [`boot_clock_ns`]; zero means already expired.
    expires_ns: AtomicU64,
}

impl ViewLease {
    pub fn renew(&self, requested_at_ns: u64) -> u64 {
        let lease = u64::try_from(CONSUMER_VIEW_LEASE.as_nanos()).unwrap_or(u64::MAX);
        let expires = requested_at_ns.saturating_add(lease);
        self.expires_ns
            .fetch_max(expires, Ordering::SeqCst)
            .max(expires)
    }
}
```

Look at the receiver: `&self`, not `&mut self`. Rust normally lets you change a value only through an exclusive `&mut` reference, but Wrapper checks this lease on every request while the agent renews it, and both hold it through an `Arc`. Atomic types are the escape hatch. Their methods take `&self` and change the value anyway, because the hardware makes each operation indivisible (Rust calls this *interior mutability*). `fetch_max` stores the larger of the current and the new value and returns the old one, in one step, so a slow answer to an older poll can never shorten a newer lease. We met `Ordering::Relaxed` on Wrapper's round-robin counter; here we use `SeqCst`, the strictest ordering, because the lease is written a few times a second and being easy to reason about is worth far more than a few nanoseconds. Finally, `Duration::as_nanos` returns a `u128`, and `u64::try_from` turns it into a `u64` or an error rather than silently truncating, the way a C cast would.

What exactly should a node stop doing when its lease lapses? Our first answer was "everything". That's the simplest thing that's obviously safe, and it has an ugly consequence: lose the council's quorum for a minute and every node stops routing, including a web server calling the cache replica sitting next to it on the same machine. A control-plane outage becomes a full traffic outage.

So let's be precise about the hazard. The danger is an address that some *other* node released and handed to something new after the leader stopped waiting for us. Now look at what a backend entry actually names. A remote backend is `node_ip:host_port` on another machine, and that machine releases it as soon as the leader says so. A local backend is one of this node's own container addresses (from its own `/23`, reserved in Bun's journal, reached through a `/32` route to the instance's own veth) or a `127.0.0.1` host port from this node's own allocator. Nobody but this node's Bun can release either of those. Another node can't reuse our local addresses, whatever the leader decides.

So a lapsed lease drops remote backends and keeps local ones. Every backend now carries a flag saying which it is:

```rust
pub struct BackendInstance {
    pub instance_id: String,
    pub node_ip: Ipv4Addr,
    pub host_port: u16,
    pub healthy: bool,
    #[serde(default)]
    pub local: bool,
}
```

Only one place sets it to `true`: `local_backend`, the function Bun uses to describe an instance it runs itself. Everything built from the leader's catalogue says `false`, and so does a record that predates the field (that's what `#[serde(default)]` does on a `bool`), which fails closed: a backend we can't prove is local is treated as remote.

The lease is then enforced in three places. Wrapper keeps routing, but asks its route for local backends only, and answers 503 when there aren't any. The choice is an enum rather than a `bool` argument, so a call site reads `select_backends(3, BackendScope::LocalOnly)` instead of a mysterious `true`. The agent loop installs the local part of its last publication in place of the whole, and brings the rest back only from a fresh leader answer. And the kernel gets its own copy, a one-entry `view_lease_map` that the connect hook reads before it picks a backend:

```c
int local_only = 0;
__u32 lease_key = VIEW_LEASE_KEY;
struct view_lease_value *lease =
    bpf_map_lookup_elem(&view_lease_map, &lease_key);
if (lease && lease->enforced == 1 &&
    bpf_ktime_get_boot_ns() >= lease->expires_ns)
    local_only = 1;
/* ... later, in the round-robin loop ... */
if (val->backends[idx].healthy == 1 &&
    (!local_only || val->backends[idx].local == 1))
```

The flag cost nothing in the map. `struct backend_endpoint` already had a spare padding byte after `healthy`, so `_pad` became `local` and the struct stayed eight bytes; the Rust mirror fills it with `u8::from(backend.local)`, the standard `From` conversion that turns `false` into 0 and `true` into 1 without a cast.

The kernel copy is the one that matters when things go properly wrong. If Bun dies, its pinned programs and maps stay attached and keep routing; that's a feature, since a Bun restart shouldn't break anyone's connections. But now the kernel's clock keeps moving with nobody renewing the lease, so after a minute the stale view's remote backends stop being honoured on their own. Its local ones carry on, and that's fine for exactly the reason above: a dead or frozen Bun releases nothing. If one of its containers dies meanwhile, the container's named network namespace, and the address in it, stays put until Bun tears it down, so a connection gets refused on this node instead of landing somewhere new. The clock is `CLOCK_BOOTTIME` rather than `CLOCK_MONOTONIC`, because the monotonic clock stops while a machine is suspended, and a laptop VM that slept through its lease must wake up with it expired, not paused. That helper arrived in Linux 5.8, which became Onion's minimum kernel. (Reading the same clock from Rust takes a `libc::clock_gettime` call inside an `unsafe` block, with the usual `// SAFETY:` comment explaining why handing the kernel a pointer to a local `timespec` is fine.)

There's one more hole, and it's in our own node. Normally, before the leader confirms that an address of ours can be released, it waits for every consumer's receipt, *including ours*: our receipt is what proves our own view stopped routing to the instance. After a discharge, nobody waits for our receipt any more. If the leader then confirmed a release while our lapsed view still routed to that local instance, we could hand its address to a new container and send traffic there. So the producer side now checks for itself:

```rust
if self.own_view_names(id) {
    return Err(BunError::ProducerReleasePending {
        instance_id: id.clone(),
        reason: "this node's own view still routes to it",
    });
}
```

`own_view_names` looks through every publication this node still retains, the ones the kernel or an in-flight Wrapper request might still be using. When the node hasn't been discharged, its own receipt already implies the same thing, so the check never delays a release that would otherwise have gone through. It only bites in the one case where it's needed.

The leader's half is a note of when it last served each consumer, kept in memory and thrown away whenever the Raft term changes. A consumer lapses when the leader hasn't served it for the lease plus a 20-second margin and gossip doesn't list it as alive. The leader then commits `DischargeEndpointConsumer`, which removes the node from the consumer set and from every pending withdrawal it still owed. Withdrawals whose sets empty complete, and the blocked releases go through.

```rust
pub const CONSUMER_VIEW_LEASE: Duration = Duration::from_secs(60);
pub const CONSUMER_DISCHARGE_MARGIN: Duration = Duration::from_secs(20);
pub const CONSUMER_DISCHARGE_AFTER: Duration = Duration::from_secs(
    CONSUMER_VIEW_LEASE.as_secs() + CONSUMER_DISCHARGE_MARGIN.as_secs(),
);
```

That third constant looks roundabout. Why not `CONSUMER_VIEW_LEASE + CONSUMER_DISCHARGE_MARGIN`? A `const` is evaluated by the compiler, so everything in its initialiser must be a `const fn`. `Duration::from_secs` and `as_secs` are; the `+` operator on `Duration` comes from the `Add` trait, and trait methods can't be called in a constant yet. So we add plain seconds and rebuild the `Duration`. The payoff is that the relationship between the two sides lives in one place, and a test pins that the leader always waits longer than any node routes.

Is this safe? Follow one poll. The node reads its clock at `t_s`, just before sending. The leader records its contact at `t_h`, before it even reads who's registered, and causality puts `t_s` before `t_h`. The node stops routing to other nodes at `t_s + 60 s`. The leader discharges no earlier than `t_h + 80 s`. Nobody compares wall clocks; the two machines only need their clocks to tick at roughly the same rate, and 20 seconds of margin over 80 covers far more drift than real hardware has. The awkward cases get the same treatment. Serving a consumer and deciding to discharge it take the same lock, so a poll can't sneak a fresh lease in while its discharge is being written, and a discharge whose write timed out keeps its node unserved until a no-op write proves the log has settled. A new leader counts silence from its own takeover, so it can only wait longer than its predecessor would have. And a node that's partitioned but still gossiping is never discharged at all: it may be alive enough to route, so its releases wait for an operator, exactly as before.

When the node comes back, its first poll registers it again. A restarted Bun withdraws everything it recovered before it publishes anything, as described above. A Bun that lived through a partition holds only its local view, and the answer replaces that in place. That's true even when the answer is the very catalogue generation it published before the lapse, which is the usual case after a short outage: the agent remembers it's running on the local part and installs the whole thing again. The answer is the current catalogue, so every remote address in it is live.

The price is still real, just smaller. A node that can't hear from any leader for a minute stops routing to *other* nodes until one answers. A service with a replica on every node (a daemon set, or simply enough replicas) rides out a council outage untouched; a call whose only backends live elsewhere fails with `EPERM` from `connect()` or 503 from Wrapper. Two corners stay fully closed. A Bun that restarts during the outage routes nothing until a leader answers, because it can't prove what it published before it died. And a local instance that crashes can't be restarted, because releasing its old address needs the leader. That's a council outage long enough to stop scheduling too, and failing closed on the cross-node traffic is the whole point of the protocol. Running out of connections is an outage you can see.

### What went wrong on the way

This design took several attempts, and the mistakes are worth more than the final shape.

The first consumer registration included plaintext development clusters. But a receipt has to come from a TLS identity, so a plaintext node could never pay off what it owed. After about a thousand deploys the ledger filled and the whole cluster stopped publishing catalogue updates. Plaintext clusters now skip the ledger entirely and get the weaker behaviour, which is fine on a laptop and one more reason not to run one on a shared network.

The first way of replacing a view was painfully correct: hide DNS and ingress, empty the kernel map, cancel every captured request, wait for zero, publish the new view. The catalogue generation changes whenever anything deploys anywhere, so a single rolling deploy made every node drop every in-flight request for every service. No test noticed, because none kept traffic flowing while the catalogue changed. Now the journal keeps a short list of views that *might* still be exposed, Bun updates the kernel map in place, drains only the backends that actually left, and marks a receipt ready once no remaining view contains the withdrawn endpoint. The full withdraw-and-wait sequence survives only for the first publication after a restart, when we genuinely can't know what the previous process had captured.

And a small one with a big effect: reading a kernel map entry used `.ok()`, which turns *any* error into `None`. A permission error therefore looked exactly like "the entry is absent", and cleanup would have treated an unreadable route as already gone. The lookup now matches Aya's `KeyNotFound` explicitly and propagates everything else. A failed lookup is not proof of absence.

The lease came out of the tour, and so did two smaller bugs it had been hiding. A rolling deploy whose old instance couldn't be released yet *failed*, so the orchestrator retried it with a new generation of replacements. Each retry counted the replacements it had already started as "existing" instances, and with one more instance than the node wanted, the rollout's first move was to retire one of them: a healthy one. The frontend sat at two running replicas out of three for five minutes, with node-1 logging the same failure every thirty seconds. A release that's merely waiting isn't a failure. The deploy worker now hands the stopped, withdrawn instance to the agent loop, which keeps asking the leader each tick, and the rollout finishes; the retained instance no longer counts as part of the app, so nothing retires it twice. And `relish wtf` had cheerfully reported that every service had a healthy backend throughout, which was true and beside the point. It now compares running replicas with desired ones and names the nodes whose placements aren't running.

Finally, a limit. Rootless Runc can prove who owns its local host port, but nothing yet carries that proof through remote withdrawal. So for 0.1.0 rootless Runc is standalone only: Bun refuses `--cluster` as soon as it selects a rootless runtime, before it touches any state. Container clusters are rootful Linux Runc with eBPF, which is what the macOS quickstart runs inside its Linux VM.

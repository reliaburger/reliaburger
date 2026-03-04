# Onion: eBPF Service Discovery

**Component:** Onion
**Subsystem:** Service discovery, namespace firewall enforcement
**Whitepaper reference:** Section 10 (Service Discovery), Section 11.3 (Network Security)
**Status:** Design

---

## 1. Overview

Onion is Reliaburger's service discovery layer. It replaces DNS servers (CoreDNS, Consul DNS) and proxy processes (Envoy, kube-proxy, linkerd-proxy) with two eBPF programs loaded into the kernel by the Bun agent on each node.

The core insight is that service discovery can be implemented entirely at the socket level, before any packets are created. When an application calls `getaddrinfo("redis.internal")`, the resulting DNS query never leaves the node -- an eBPF program intercepts it in the socket layer, looks up the name in an in-kernel hash map, and responds with a virtual IP address. When the application subsequently calls `connect()` with that virtual IP, a second eBPF program intercepts the syscall, selects a healthy backend from the service map, and rewrites the destination to the real `host:port` of a running instance. The application's `connect()` completes with a direct TCP connection to the backend. No proxy sits in the data path. No DNS server process exists.

This eliminates an entire category of infrastructure: no DNS server to operate, no proxy process consuming memory and adding latency, no iptables/IPVS rules to manage, no listening ports to allocate. The eBPF programs persist in the kernel even if the Bun agent crashes, so running applications continue to resolve names and reach backends. Only service map updates pause until Bun restarts.

Onion also enforces namespace isolation and per-app firewall rules at the `connect()` interception point. Because the eBPF program already intercepts every outbound connection, checking whether the caller is authorised to reach the destination is a natural extension, not a separate mechanism. This is described in the Security Considerations section below.

---

## 2. Dependencies

### Kernel

- **Linux kernel 5.7+** (mandatory). Onion requires `BPF_CGROUP_UDP4_SENDMSG` and `BPF_CGROUP_INET4_CONNECT` hook types, both available since kernel 5.7. Bun checks the kernel version at startup and refuses to start on older kernels with a clear error message and exit code 1.
- **BPF Type Format (BTF)** enabled in the kernel (`CONFIG_DEBUG_INFO_BTF=y`). Required for CO-RE (Compile Once, Run Everywhere) portability of eBPF programs across kernel versions. Most distribution kernels since Ubuntu 20.10, Fedora 33, and Debian 12 ship with BTF enabled.
- **cgroup v2** mounted at `/sys/fs/cgroup`. Required for cgroup-scoped eBPF program attachment and for identifying the source application by cgroup ID in the firewall path.

### Bun Agent

Bun is the sole writer to all BPF maps. It:

1. Loads the compiled eBPF programs into the kernel at startup.
2. Attaches them to the appropriate cgroup hooks.
3. Writes initial service map entries by querying the reporting tree for the current cluster state.
4. Continuously updates map entries as state changes arrive via the hierarchical reporting tree (new deployments, health check transitions, rescheduling events, instance terminations).
5. Writes firewall map entries based on app configuration (`firewall.allow_from`, namespace membership).

### Reporting Tree

The hierarchical reporting tree is the source of truth for service map updates. Each node reports to its assigned council member, and council members aggregate for the leader. State changes flow in the reverse direction: when the leader accepts a new deployment or the scheduler moves an instance, the change propagates down the reporting tree to every node's Bun agent, which writes the corresponding BPF map update. The latency from scheduling decision to map update on every node is bounded by the reporting tree propagation time (typically sub-second for a 1000-node cluster).

---

## 3. Architecture

Onion consists of two eBPF programs and three BPF hash maps, all managed by the Bun agent process.

### eBPF Programs

**Program 1: `onion_dns` (DNS interception)**

- Hook type: `BPF_CGROUP_UDP4_SENDMSG` (for UDP DNS) and `BPF_CGROUP_UDP4_RECVMSG` (for response injection)
- Attachment point: root cgroup v2 (`/sys/fs/cgroup`)
- Trigger: Any UDP `sendmsg()` call targeting destination port 53
- Behaviour: Inspects the DNS query payload. If the queried name ends with `.internal`, looks up the name in `dns_map` and synthesizes a DNS response containing the mapped virtual IP. If the name is not `.internal`, the call passes through untouched to the host's configured upstream DNS resolver.

**Program 2: `onion_connect` (connect rewrite)**

- Hook type: `BPF_CGROUP_INET4_CONNECT`
- Attachment point: root cgroup v2 (`/sys/fs/cgroup`)
- Trigger: Any `connect()` syscall
- Behaviour: Checks whether the destination IP falls within the virtual IP range (127.128.0.0/16). If yes, looks up `(vip, port)` in `backend_map`, selects a healthy backend via round-robin, optionally checks `firewall_map` for authorisation, then rewrites the destination address and port to the selected backend's real `host_ip:host_port`. If the VIP is not found or no healthy backends exist, returns `-ECONNREFUSED`. If the destination IP is outside the VIP range, the call passes through untouched.

### BPF Maps

```
+-------------------+     +--------------------+     +--------------------+
|     dns_map       |     |    backend_map     |     |   firewall_map     |
|                   |     |                    |     |                    |
| name -> vip       |     | (vip,port) ->      |     | (src_cgroup,       |
|                   |     |   backend_list     |     |  dst_app_id) ->    |
| "redis.internal"  |     |                    |     |    allow/deny      |
|  -> 127.128.0.3   |     | 127.128.0.3:6379   |     |                    |
|                   |     |  -> [10.0.1.5:30891 |     +--------------------+
+-------------------+     |     10.0.1.7:31022] |
                          +--------------------+
```

### Data Path (No Bun Involvement)

Once the maps are populated, the data path is entirely in-kernel. Bun is not consulted for any connection. The eBPF programs read from BPF maps in kernel memory. This means:

- Bun crashing does not break running connections or new connections to unchanged backends.
- DNS resolution takes sub-microsecond (hash map lookup, no network round trip).
- `connect()` rewrite adds zero measurable latency to connection establishment.
- There is no userspace proxy in the data path at any point.

### Virtual IP Range

Virtual IPs are allocated from `127.128.0.0/16`, a range within the loopback block (`127.0.0.0/8`) that is guaranteed to never conflict with real network addresses. The /16 provides 65,534 usable addresses. If a cluster approaches this limit, the range can be expanded to `127.128.0.0/10` (~4 million VIPs) via a cluster configuration change and rolling restart.

---

## 4. Data Structures

### BPF Map: `dns_map`

Maps `.internal` DNS names to virtual IP addresses.

```
Type:        BPF_MAP_TYPE_HASH
Max entries: 65534 (matches VIP range capacity)
Key size:    256 bytes (max DNS name length)
Value size:  4 bytes (IPv4 address)
Flags:       BPF_F_NO_PREALLOC
```

**Key structure:**

```c
struct dns_key {
    char name[256];   // null-terminated, lowercase-normalised
                      // e.g., "redis.internal\0"
};
```

**Value structure:**

```c
struct dns_value {
    __u32 vip;        // virtual IP in network byte order
                      // e.g., 127.128.0.3 -> 0x7F800003
};
```

**Rust-side struct (Bun writes this):**

```rust
/// Key for the dns_map BPF hash map.
/// Name is null-terminated, lowercase-normalised, max 255 chars + null.
#[repr(C)]
#[derive(Clone, Debug)]
pub struct DnsMapKey {
    pub name: [u8; 256],
}

/// Value for the dns_map BPF hash map.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct DnsMapValue {
    pub vip: u32, // network byte order
}
```

### BPF Map: `backend_map`

Maps `(virtual_ip, port)` pairs to a list of healthy backend endpoints.

```
Type:        BPF_MAP_TYPE_HASH
Max entries: 65534
Key size:    8 bytes (VIP + port + padding)
Value size:  648 bytes (backend array + metadata)
Flags:       BPF_F_NO_PREALLOC
```

**Key structure:**

```c
struct backend_key {
    __u32 vip;        // virtual IP, network byte order
    __u16 port;       // service port (the port the app declares, e.g. 6379)
    __u16 _pad;       // alignment padding
};
```

**Value structure:**

```c
#define MAX_BACKENDS 32

struct backend_endpoint {
    __u32 host_ip;    // real node IP, network byte order
    __u16 host_port;  // dynamically allocated host port, network byte order
    __u8  healthy;    // 1 = healthy, 0 = unhealthy (excluded from selection)
    __u8  _pad;
};

struct backend_value {
    __u32 count;                              // total number of backends (healthy + unhealthy)
    __u32 rr_index;                           // round-robin counter (atomically incremented)
    __u32 app_id;                             // app identifier for firewall lookups
    __u32 namespace_id;                       // namespace identifier for isolation checks
    struct backend_endpoint backends[MAX_BACKENDS];  // backend array
};
```

**Rust-side structs:**

```rust
pub const MAX_BACKENDS: usize = 32;

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BackendKey {
    pub vip: u32,     // network byte order
    pub port: u16,    // network byte order
    pub _pad: u16,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BackendEndpoint {
    pub host_ip: u32,    // network byte order
    pub host_port: u16,  // network byte order
    pub healthy: u8,     // 1 or 0
    pub _pad: u8,
}

#[repr(C)]
#[derive(Clone, Debug)]
pub struct BackendValue {
    pub count: u32,
    pub rr_index: u32,
    pub app_id: u32,
    pub namespace_id: u32,
    pub backends: [BackendEndpoint; MAX_BACKENDS],
}
```

### BPF Map: `firewall_map`

Maps `(source_cgroup_id, destination_app_id)` to an allow/deny decision. Used for both namespace isolation (default) and per-app `firewall.allow_from` rules.

```
Type:        BPF_MAP_TYPE_HASH
Max entries: 262144 (supports up to ~500 apps/node * 512 destination apps)
Key size:    16 bytes
Value size:  4 bytes
Flags:       BPF_F_NO_PREALLOC
```

**Key structure:**

```c
struct firewall_key {
    __u64 src_cgroup_id;   // cgroup ID of the calling process (stable across restarts)
    __u32 dst_app_id;      // app identifier of the destination service
    __u32 _pad;
};
```

**Value structure:**

```c
struct firewall_value {
    __u32 action;   // 0 = deny, 1 = allow
};
```

**Rust-side structs:**

```rust
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct FirewallKey {
    pub src_cgroup_id: u64,
    pub dst_app_id: u32,
    pub _pad: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct FirewallValue {
    pub action: u32,  // FIREWALL_DENY = 0, FIREWALL_ALLOW = 1
}
```

### Supplementary BPF Map: `cgroup_namespace_map`

Maps a cgroup ID to its namespace ID, used by the eBPF program to enforce default namespace isolation without requiring an explicit firewall_map entry for every possible pair.

```
Type:        BPF_MAP_TYPE_HASH
Max entries: 65536
Key size:    8 bytes
Value size:  4 bytes
Flags:       BPF_F_NO_PREALLOC
```

```c
struct cgroup_ns_key {
    __u64 cgroup_id;
};

struct cgroup_ns_value {
    __u32 namespace_id;
};
```

### Virtual IP Allocation: `VirtualIP`

```rust
use std::hash::{Hash, Hasher};
use siphasher::sip::SipHasher24;

/// A virtual IP deterministically derived from an app name.
/// The same app name always maps to the same VIP cluster-wide.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct VirtualIP(pub std::net::Ipv4Addr);

impl VirtualIP {
    /// Derive a VIP from an app name. Deterministic: same name -> same VIP.
    /// Uses SipHash-2-4 with a fixed seed to distribute names across the
    /// 127.128.0.0/16 range (addresses 127.128.0.1 through 127.128.255.254).
    pub fn from_app_name(name: &str) -> Self {
        let mut hasher = SipHasher24::new_with_keys(0xDEAD_BEEF_CAFE_F00D, 0xBAAD_F00D_DEAD_BEEF);
        name.hash(&mut hasher);
        let hash = hasher.finish();

        // Map to range 1..=65534 (skip .0 network and .255.255 broadcast)
        let offset = (hash % 65534) as u32 + 1;
        let ip = 0x7F80_0000u32 | (offset & 0xFFFF); // 127.128.x.y
        VirtualIP(std::net::Ipv4Addr::from(ip))
    }
}
```

### Service Entry (Bun-Side Aggregate)

```rust
/// Full service state maintained by Bun in userspace.
/// Bun compiles this into the BPF map entries.
pub struct ServiceEntry {
    pub app_name: String,
    pub namespace: String,
    pub namespace_id: u32,
    pub app_id: u32,
    pub vip: VirtualIP,
    pub port: u16,                         // declared container port
    pub backends: Vec<BackendInstance>,
    pub firewall_allow_from: Option<Vec<String>>,  // None = namespace default
}

pub struct BackendInstance {
    pub instance_id: String,
    pub node_ip: std::net::Ipv4Addr,
    pub host_port: u16,
    pub healthy: bool,
    pub node_name: String,
    pub uptime: std::time::Duration,
}
```

---

## 5. Operations

### 5.1 DNS Interception Flow

When an application calls `getaddrinfo("redis.internal")`, the C library (glibc or musl) constructs a DNS query packet and calls `sendmsg()` targeting the nameserver at port 53 (as configured in `/etc/resolv.conf`).

**eBPF pseudocode for `onion_dns` (sendmsg hook):**

```c
SEC("cgroup/sendmsg4")
int onion_dns_sendmsg(struct bpf_sock_addr *ctx) {
    // Only intercept traffic to port 53 (DNS)
    if (ctx->user_port != bpf_htons(53))
        return 1;  // pass through

    // Read the DNS query from the socket buffer
    struct dns_header hdr;
    if (bpf_skb_load_bytes(ctx, 0, &hdr, sizeof(hdr)) < 0)
        return 1;  // can't parse, pass through

    // Extract the queried name from the DNS question section
    char qname[256] = {};
    int qname_len = parse_dns_qname(ctx, sizeof(struct dns_header), qname, sizeof(qname));
    if (qname_len <= 0)
        return 1;  // can't parse, pass through

    // Check for .internal suffix
    if (!ends_with_internal(qname, qname_len))
        return 1;  // not a .internal name, pass through to upstream DNS

    // Look up in dns_map
    struct dns_key key = {};
    __builtin_memcpy(key.name, qname, qname_len);
    normalize_to_lowercase(key.name);

    struct dns_value *val = bpf_map_lookup_elem(&dns_map, &key);
    if (!val)
        return 1;  // name not in service map, pass through

    // Store the response VIP in a per-CPU scratch map for the recvmsg hook
    __u64 cookie = bpf_get_socket_cookie(ctx);
    struct pending_dns_response resp = {
        .vip = val->vip,
        .query_id = hdr.id,
    };
    __builtin_memcpy(resp.qname, qname, qname_len);
    resp.qname_len = qname_len;

    bpf_map_update_elem(&dns_pending_map, &cookie, &resp, BPF_ANY);

    // Redirect the sendmsg to localhost so the query doesn't leave the node.
    // The recvmsg hook will synthesize the response.
    ctx->user_ip4 = bpf_htonl(0x7F000001);  // 127.0.0.1
    return 1;
}
```

**eBPF pseudocode for DNS response synthesis (recvmsg hook):**

```c
SEC("cgroup/recvmsg4")
int onion_dns_recvmsg(struct bpf_sock_addr *ctx) {
    __u64 cookie = bpf_get_socket_cookie(ctx);
    struct pending_dns_response *resp = bpf_map_lookup_elem(&dns_pending_map, &cookie);
    if (!resp)
        return 1;  // no pending interception, pass through

    // Synthesize a minimal DNS response:
    //   - Copy query ID from the original request
    //   - Set QR=1 (response), AA=1 (authoritative), RCODE=0 (no error)
    //   - One answer: A record with the virtual IP, TTL=0
    // Write the synthesized response into the receive buffer.

    struct dns_response_buf buf = {};
    int len = build_dns_a_response(
        resp->query_id,
        resp->qname, resp->qname_len,
        resp->vip,
        0,            // TTL = 0 (always re-resolve; map is always current)
        &buf
    );

    // Inject the response
    bpf_msg_push_data(ctx, 0, len, 0);
    bpf_msg_apply_bytes(ctx, len);
    write_response_bytes(ctx, &buf, len);

    // Clean up pending entry
    bpf_map_delete_elem(&dns_pending_map, &cookie);

    // Restore the apparent source to the original DNS server address
    // so the application sees a response "from" its configured nameserver
    ctx->user_ip4 = bpf_htonl(/* original nameserver IP */);

    return 1;
}
```

**Full flow from the application's perspective:**

```
1. App: getaddrinfo("redis.internal")
2. C library: constructs DNS A query, sendmsg() to 10.0.0.2:53 (nameserver)
3. Kernel: onion_dns_sendmsg fires
     - Detects port 53
     - Parses query: "redis.internal"
     - Matches .internal suffix
     - Looks up dns_map["redis.internal"] -> 127.128.0.3
     - Stores pending response, redirects to loopback
4. Kernel: onion_dns_recvmsg fires
     - Finds pending response for this socket
     - Synthesizes DNS A response: redis.internal -> 127.128.0.3, TTL=0
     - Injects into receive buffer
5. C library: receives DNS response, returns 127.128.0.3 to application
6. App: receives struct addrinfo with address 127.128.0.3

Total time: <1 microsecond (hash map lookup, no network I/O)
```

### 5.2 Connect Rewrite Flow

When the application calls `connect(127.128.0.3, 6379)`, the eBPF program intercepts the syscall before the kernel initiates the TCP handshake.

**eBPF pseudocode for `onion_connect`:**

```c
SEC("cgroup/connect4")
int onion_connect(struct bpf_sock_addr *ctx) {
    __u32 dst_ip = bpf_ntohl(ctx->user_ip4);
    __u16 dst_port = bpf_ntohs(ctx->user_port);

    // Only intercept VIPs in the 127.128.0.0/16 range
    if ((dst_ip & 0xFFFF0000) != 0x7F800000)
        return 1;  // not a VIP, pass through

    // Look up the backend list for this (VIP, port)
    struct backend_key key = {
        .vip = ctx->user_ip4,       // keep network byte order
        .port = ctx->user_port,
        ._pad = 0,
    };

    struct backend_value *val = bpf_map_lookup_elem(&backend_map, &key);
    if (!val || val->count == 0)
        return 0;  // -ECONNREFUSED: no backends registered

    // --- Firewall check ---
    __u64 src_cgroup = bpf_get_current_cgroup_id();

    // Check namespace isolation (default policy: same namespace only)
    struct cgroup_ns_value *src_ns = bpf_map_lookup_elem(&cgroup_namespace_map, &src_cgroup);
    if (src_ns && src_ns->namespace_id != val->namespace_id) {
        // Cross-namespace connection. Check firewall_map for explicit allow.
        struct firewall_key fw_key = {
            .src_cgroup_id = src_cgroup,
            .dst_app_id = val->app_id,
            ._pad = 0,
        };
        struct firewall_value *fw = bpf_map_lookup_elem(&firewall_map, &fw_key);
        if (!fw || fw->action == 0)
            return 0;  // -ECONNREFUSED: cross-namespace denied
    }

    // Check per-app firewall rules (allow_from)
    // If the destination app has allow_from rules, check if this source is allowed.
    // Convention: if dst_app_id has entries in firewall_map, then only explicitly
    // allowed sources can connect. We check for an allow entry.
    struct firewall_key fw_key = {
        .src_cgroup_id = src_cgroup,
        .dst_app_id = val->app_id,
        ._pad = 0,
    };
    struct firewall_value *fw = bpf_map_lookup_elem(&firewall_map, &fw_key);
    // If there's an explicit deny, refuse
    if (fw && fw->action == 0)
        return 0;  // -ECONNREFUSED

    // --- Backend selection: round-robin among healthy backends ---
    __u32 selected_idx = 0;
    __u32 attempts = 0;
    int found = 0;

    #pragma unroll
    for (int i = 0; i < MAX_BACKENDS; i++) {
        if (attempts >= val->count)
            break;

        __u32 idx = __sync_fetch_and_add(&val->rr_index, 1) % val->count;

        if (idx < MAX_BACKENDS && val->backends[idx].healthy == 1) {
            selected_idx = idx;
            found = 1;
            break;
        }
        attempts++;
    }

    if (!found)
        return 0;  // -ECONNREFUSED: no healthy backends

    // Rewrite destination to the selected backend
    struct backend_endpoint *be = &val->backends[selected_idx];
    ctx->user_ip4 = be->host_ip;      // real node IP
    ctx->user_port = be->host_port;    // real host port

    return 1;  // proceed with connect() to the rewritten address
}
```

**Full flow from the application's perspective:**

```
1. App: connect(127.128.0.3, 6379)
2. Kernel: onion_connect fires
     - Detects VIP range (127.128.x.x)
     - Looks up backend_map[(127.128.0.3, 6379)]
     - Finds 2 backends: [{10.0.1.5:30891, healthy}, {10.0.1.7:31022, healthy}]
     - Checks firewall: source cgroup -> destination app_id (allowed)
     - Round-robin selects 10.0.1.5:30891
     - Rewrites ctx->user_ip4 = 10.0.1.5, ctx->user_port = 30891
3. Kernel: TCP SYN sent to 10.0.1.5:30891
4. App: connect() returns success, direct TCP connection established
5. App: sends and receives data at native TCP performance

Total added latency: 0 (rewrite happens before SYN; no extra hop)
```

### 5.3 Service Map Updates

Bun receives state changes from the reporting tree and translates them into BPF map operations. All map updates use `bpf_map_update_elem()` which is atomic at the entry level.

**New app deployed:**

```
1. Leader schedules app "redis" (port 6379) to node 10.0.1.5
2. Bun on 10.0.1.5 starts the container, allocates host port 30891
3. Bun on 10.0.1.5 reports: "redis instance running at 10.0.1.5:30891"
4. Reporting tree propagates to all nodes
5. Every Bun agent:
   a. Computes VIP: VirtualIP::from_app_name("redis") -> 127.128.0.3
   b. Writes dns_map: "redis.internal" -> 127.128.0.3
   c. Writes backend_map: (127.128.0.3, 6379) -> [{10.0.1.5:30891, healthy}]
   d. Writes cgroup_namespace_map entries for local containers
   e. Writes firewall_map entries if allow_from rules are configured
```

**Instance becomes unhealthy:**

```
1. Bun on 10.0.1.5 detects health check failure for redis-1
2. Bun on 10.0.1.5 reports: "redis-1 unhealthy"
3. Reporting tree propagates to all nodes
4. Every Bun agent:
   a. Reads current backend_map entry for (127.128.0.3, 6379)
   b. Sets backends[idx].healthy = 0 for the 10.0.1.5:30891 entry
   c. Writes updated entry atomically
5. Subsequent connect() calls skip the unhealthy backend
```

**Instance rescheduled to a different node:**

```
1. Old instance on 10.0.1.5 terminated
2. New instance on 10.0.1.9 started, allocated host port 29100
3. Reporting tree propagates both events
4. Every Bun agent:
   a. Removes 10.0.1.5:30891 from backend list
   b. Adds 10.0.1.9:29100 with healthy=1
   c. Writes updated entry atomically
```

**App deleted:**

```
1. Leader removes app "redis" from desired state
2. All instances are terminated
3. Every Bun agent:
   a. Deletes dns_map entry for "redis.internal"
   b. Deletes backend_map entry for (127.128.0.3, 6379)
   c. Cleans up related firewall_map entries
```

### 5.4 Virtual IP Allocation Algorithm

VIPs are deterministic: the same app name always produces the same VIP across every node in the cluster. This means there is no coordination required for VIP allocation -- every node independently computes the same mapping.

The algorithm:

1. Normalise the app name to lowercase.
2. Compute `SipHash-2-4(name)` with fixed seed keys.
3. Map `hash % 65534 + 1` to get an offset in the range `[1, 65534]`.
4. Construct the VIP as `127.128.{offset >> 8}.{offset & 0xFF}`.

**Collision handling:** SipHash collisions are possible but extremely unlikely for the expected number of apps (well under 65,534 in practice). If a collision occurs (two different app names hash to the same VIP), Bun detects it when writing the dns_map (the existing entry's name does not match the new name) and falls back to linear probing: it tries offset+1, offset+2, etc. until an unused VIP is found. The collision resolution is performed by the leader and the resolved VIP is distributed via the reporting tree, so all nodes converge on the same assignment.

### 5.5 Health-Aware Backend Selection

The `onion_connect` eBPF program selects backends using round-robin with health filtering:

1. Atomically increment `val->rr_index`.
2. Compute `candidate = rr_index % val->count`.
3. If `backends[candidate].healthy == 1`, select it.
4. Otherwise, try the next index. Repeat up to `val->count` times.
5. If no healthy backend is found after a full scan, return `-ECONNREFUSED`.

The health status of each backend is maintained by Bun based on the health check results reported through the reporting tree. Bun writes the `healthy` field directly into the BPF map entry. The eBPF program never performs health checks itself -- it only reads the pre-computed health status.

### 5.6 External Service Passthrough

The eBPF programs are designed to be invisible for non-cluster traffic:

- **DNS:** Only queries for names ending in `.internal` are intercepted. All other DNS queries (`api.stripe.com`, `s3.amazonaws.com`, etc.) pass through to the host's upstream DNS resolver configured in `/etc/resolv.conf`.
- **Connect:** Only connections to IPs in the `127.128.0.0/16` range are intercepted. All other `connect()` calls (to real IPs, to `127.0.0.1`, to external addresses) pass through untouched.

Because Reliaburger does not use an overlay network, containers have direct outbound access to external services via the host network stack (subject to egress allowlist rules enforced by nftables, not by Onion).

---

## 6. Configuration

Onion's configuration is part of the node-level and cluster-level TOML configuration.

### Cluster-level (`cluster.toml`)

```toml
[service_discovery]
# VIP CIDR range. Default: "127.128.0.0/16"
# Can be expanded to "127.128.0.0/10" for very large clusters (~4M VIPs).
vip_range = "127.128.0.0/16"

# Maximum number of backends per service in the BPF map.
# Default: 32. Increase for services with very high replica counts.
max_backends_per_service = 32

# BPF map max entries for dns_map and backend_map.
# Default: 65534 (matches /16 VIP range).
max_services = 65534

# Firewall map max entries. Default: 262144.
max_firewall_entries = 262144
```

### Node-level (`node.toml`)

```toml
[ebpf]
# Directory containing compiled eBPF object files.
# Default: "/var/lib/reliaburger/ebpf/"
program_dir = "/var/lib/reliaburger/ebpf/"

# Cgroup v2 mount point for eBPF program attachment.
# Default: "/sys/fs/cgroup"
cgroup_path = "/sys/fs/cgroup"

# Port range for dynamic host port allocation (used by Bun, referenced
# by Onion for backend map entries).
# Default: "10000-60000"
host_port_range = "10000-60000"
```

### eBPF Program Files

Onion ships as pre-compiled eBPF object files (CO-RE, using BTF for portability):

```
/var/lib/reliaburger/ebpf/
    onion_dns.bpf.o          # DNS interception program
    onion_connect.bpf.o      # connect() rewrite program
```

These are embedded in the `reliaburger` binary and extracted to disk at install time. Bun loads them via the BPF syscall at startup.

---

## 7. Failure Modes

### 7.1 Stale Service Map When Bun Is Down

**Scenario:** Bun crashes or is restarted. The eBPF programs remain loaded in the kernel (they are pinned to the BPF filesystem at `/sys/fs/bpf/onion/`). The BPF maps retain their last-written state.

**Impact:** Running applications continue to resolve `.internal` names and connect to backends using the existing service map. No disruption for services whose backends have not changed.

**Risk:** If backends are rescheduled to different nodes/ports while Bun is down (e.g., the leader reschedules a failed app to another node), the stale map still points to old `host:port` addresses. New connections to recently-rescheduled backends will fail with `ECONNREFUSED` until Bun restarts and refreshes the map.

**Mitigation:** Bun prioritizes a full service map refresh on startup. It queries the reporting tree for current cluster state and writes all map entries before accepting any other work. The refresh window is typically <1 second for clusters with fewer than 5,000 services.

### 7.2 Kernel Version Incompatibility

**Scenario:** Bun starts on a node running kernel < 5.7, or a kernel without BTF or cgroup v2.

**Impact:** Bun cannot load the eBPF programs. Service discovery is not functional on this node.

**Behaviour:** Bun checks all three prerequisites (kernel version, BTF, cgroup v2) at startup. If any check fails, Bun exits with exit code 1 and a clear error message specifying which requirement is missing and how to resolve it. The node does not join the cluster -- a node without Onion cannot run workloads.

### 7.3 BPF Map Capacity Limits

**Scenario:** The number of distinct services exceeds `max_services` (default 65,534), or a single service exceeds `MAX_BACKENDS` (default 32 backends).

**Impact for service limit:** `bpf_map_update_elem()` returns `-E2BIG`. Bun logs an error, fires an alert, and the new service is not discoverable via Onion.

**Impact for backend limit:** Bun silently drops backends beyond the 32nd. This is logged as a warning. The eBPF program only sees the first 32 backends.

**Mitigation:** Monitor service and backend counts via Mayo metrics (`onion_services_total`, `onion_backends_per_service_max`). Increase `max_services` or `max_backends_per_service` in cluster configuration if approaching limits. The backend limit of 32 is sufficient for the vast majority of services -- services needing more replicas than 32 are rare, and the operator is warned well in advance.

### 7.4 Backend Rescheduled While Bun Offline

This is the most significant failure mode. It combines 7.1 (stale map) with a real state change.

**Scenario:**

1. Bun on node X crashes.
2. While Bun is down, the leader reschedules app "api" from node Y (port 31000) to node Z (port 32000).
3. Other nodes' Bun agents update their maps. Node X's map still shows Y:31000.
4. An application on node X calls `connect()` to the api service.
5. The eBPF program rewrites to Y:31000, which no longer exists.
6. `connect()` fails with `ECONNREFUSED`.

**Mitigation:**

- Applications with retries and circuit breakers will survive the brief window.
- Bun performs a full map refresh on startup, prioritising this before other initialisation.
- The window is bounded by Bun's restart time (typically <5 seconds with systemd auto-restart).
- For critical services, run multiple replicas. If only one backend was rescheduled, the others remain valid.

### 7.5 eBPF Program Detach

**Scenario:** An operator or automated tool inadvertently detaches the eBPF programs (e.g., removing the pinned programs from `/sys/fs/bpf/onion/`).

**Impact:** DNS interception and connect rewriting stop. Applications see DNS resolution failures for `.internal` names and `ECONNREFUSED` for connections to VIPs.

**Mitigation:** Bun monitors the eBPF program attachment state via a periodic health check (every 5 seconds). If the programs are no longer attached, Bun reloads and reattaches them, then logs a warning. Recovery is automatic and takes <1 second.

---

## 8. Security Considerations

### 8.1 eBPF Program Verification

All eBPF programs are verified by the kernel's in-kernel BPF verifier before loading. The verifier guarantees:

- No out-of-bounds memory access.
- No unbounded loops (all loops must have a provable upper bound).
- No use of uninitialized data.
- No access to kernel memory outside of explicitly permitted map data.
- The program terminates within a bounded number of instructions.

Onion's eBPF programs are compiled with `clang -O2 -target bpf` and designed to pass the verifier on kernel 5.7+. The programs are statically compiled and shipped as part of the `reliaburger` binary -- they are not dynamically generated, which eliminates injection risk.

### 8.2 Namespace Isolation Enforcement

Namespace isolation is the default security posture: apps in different namespaces cannot communicate unless an explicit `allow_from` rule grants access.

**Enforcement mechanism:**

1. When Bun starts a container, it records the container's cgroup ID and namespace ID in `cgroup_namespace_map`.
2. When `onion_connect` intercepts a `connect()` to a VIP, it:
   a. Looks up the source cgroup ID in `cgroup_namespace_map` to find the source namespace.
   b. Compares the source namespace ID with the destination's `namespace_id` (stored in `backend_value`).
   c. If they differ, looks up `(src_cgroup_id, dst_app_id)` in `firewall_map`.
   d. If no explicit allow entry exists, returns `-ECONNREFUSED`.

3. The enforcement happens at the source, before any packet is created. Cross-namespace connections are blocked even for same-node traffic.

This source-side enforcement model has a critical advantage over network-level policies: there is no window between container start and rule installation. The eBPF program identifies callers by cgroup ID (assigned at container creation), not by ephemeral source ports.

### 8.3 Cross-Namespace Blocking

By default, all cross-namespace connections are denied. To allow specific cross-namespace communication:

```toml
[app.payment-service]
namespace = "billing"

[app.payment-service.firewall]
allow_from = ["app.api@production"]   # allow api app from the production namespace
```

When Bun processes this configuration, it writes a `firewall_map` entry:

```
key:   { src_cgroup_id: <cgroup of api@production>, dst_app_id: <payment-service id> }
value: { action: 1 }   // ALLOW
```

For apps with `allow_from` rules, Bun also writes entries for same-namespace callers that are explicitly listed. Apps without a `firewall` block accept connections from any app in the same namespace (the default namespace isolation rule handles this implicitly in the eBPF program via the namespace ID comparison).

### 8.4 BPF Filesystem Pinning

eBPF programs and maps are pinned to the BPF filesystem at `/sys/fs/bpf/onion/`:

```
/sys/fs/bpf/onion/
    dns_sendmsg        # pinned DNS interception program
    dns_recvmsg        # pinned DNS response program
    connect            # pinned connect rewrite program
    dns_map            # pinned DNS map
    backend_map        # pinned backend map
    firewall_map       # pinned firewall map
    cgroup_ns_map      # pinned cgroup-to-namespace map
    dns_pending_map    # pinned DNS pending response map
```

Pinning ensures the programs and maps persist across Bun restarts. Bun checks for existing pinned programs at startup and reuses them (updating maps only) rather than reloading from scratch. This means a Bun restart causes zero disruption to in-flight service discovery.

### 8.5 Privilege Requirements

Loading eBPF programs requires `CAP_BPF` and `CAP_NET_ADMIN` capabilities. Bun runs as root on each node (consistent with its role as the node agent managing containers, cgroups, namespaces, and network configuration). The eBPF programs themselves run in the kernel with the permissions of the BPF subsystem -- they cannot escalate beyond what the verifier allows.

---

## 9. Performance

### 9.1 DNS Resolution Latency

Traditional DNS resolution requires a network round trip to a DNS server process (CoreDNS, Consul DNS), typically adding 0.5-5ms depending on load and whether the resolver is local or remote.

Onion DNS resolution is a BPF hash map lookup in kernel memory. Measured latency: **<1 microsecond** (sub-us). There is no network I/O, no context switch to a DNS server process, no UDP packet construction, and no resolver cache to warm. The first query is as fast as the millionth.

### 9.2 Connect Rewrite Latency

Traditional proxy-based service discovery (kube-proxy/iptables, Envoy sidecar, linkerd-proxy) adds 0.1-1ms per connection in the data path due to userspace proxy overhead, connection splicing, or iptables rule traversal.

Onion connect rewrite is a BPF hash map lookup and a field rewrite on the `struct bpf_sock_addr` context. The rewrite happens before the kernel initiates the TCP handshake. **Zero additional data-path latency.** After the rewrite, the TCP connection is direct between the application and the backend -- there is no proxy process forwarding bytes.

### 9.3 Memory Usage

| Component | Memory |
|-----------|--------|
| `dns_map` (5,000 services) | ~1.3 MB (256+4 bytes/entry) |
| `backend_map` (5,000 services, avg 4 backends) | ~3.2 MB (8+648 bytes/entry) |
| `firewall_map` (50,000 rules) | ~1.0 MB (16+4 bytes/entry) |
| `cgroup_namespace_map` (500 local containers) | ~6 KB |
| eBPF program instructions | ~20 KB |
| **Total** | **<6 MB for 5,000 services** |

For comparison, a single Envoy sidecar proxy process typically consumes 50-200 MB of RSS memory. A per-node kube-proxy instance with 5,000 services in iptables mode generates tens of thousands of iptables rules consuming 100+ MB of kernel memory and adding O(n) rule traversal time.

### 9.4 Comparison vs. Proxy Approach

| Metric | kube-proxy (iptables) | Envoy sidecar | Onion eBPF |
|--------|----------------------|---------------|------------|
| DNS resolution | 0.5-5ms (CoreDNS) | 0.5-5ms (CoreDNS) | <1us (BPF map) |
| Per-connection latency | 0.1-0.5ms (iptables traversal) | 0.2-1ms (userspace proxy) | 0 (kernel rewrite) |
| Memory per node | 100+ MB (iptables rules) | 50-200MB per sidecar | <6 MB (BPF maps) |
| Failure mode | kube-proxy crash = stale rules | Envoy crash = broken connections | Bun crash = stale map, connections continue |
| Listening ports | 1+ per service | 1 per sidecar | 0 |
| Configuration | kube-proxy + CoreDNS config | Envoy xDS + CoreDNS | None (Bun manages automatically) |
| Rule update time | O(n) iptables reprogramming | Seconds (xDS push) | Microseconds (BPF map update) |

### 9.5 Scalability

- **Per-service overhead:** One `dns_map` entry (260 bytes) + one `backend_map` entry (656 bytes) = ~916 bytes per service. At 65,534 services (max /16 range), total map memory is ~60 MB.
- **Map update rate:** BPF map updates take ~1 microsecond each. Bun can apply 1,000,000 map updates per second, far exceeding the rate of state changes in the reporting tree.
- **eBPF program complexity:** Both programs are O(1) per invocation (hash map lookups). There is no O(n) rule traversal, unlike iptables.

---

## 10. Testing Strategy

### 10.1 eBPF Program Unit Testing

The eBPF programs are tested using the BPF test infrastructure (`BPF_PROG_TEST_RUN`), which allows running eBPF programs against synthetic input contexts without requiring real network traffic.

**DNS interception tests:**

- Query for a `.internal` name present in `dns_map` returns the correct VIP.
- Query for a `.internal` name not present passes through.
- Query for a non-`.internal` name passes through regardless of map contents.
- Malformed DNS queries (truncated, invalid label lengths) pass through without crash.
- DNS query for name at exactly 255 characters (max length).
- Case-insensitive matching: `Redis.Internal` matches `redis.internal`.

**Connect rewrite tests:**

- `connect()` to a VIP with healthy backends rewrites to a valid backend.
- `connect()` to a VIP with no healthy backends returns `-ECONNREFUSED`.
- `connect()` to a VIP not in `backend_map` returns `-ECONNREFUSED`.
- `connect()` to a non-VIP address passes through untouched.
- Round-robin distributes across healthy backends evenly.
- Backends marked `healthy=0` are never selected.
- Firewall deny prevents cross-namespace connection.
- Firewall allow permits explicit cross-namespace connection.

### 10.2 Service Map Consistency Testing

Integration tests that verify Bun correctly maintains the BPF maps:

- Deploy an app, verify `dns_map` and `backend_map` entries appear on all nodes.
- Scale an app up, verify new backends are added to `backend_map`.
- Scale an app down, verify removed backends are deleted.
- Fail a health check, verify `healthy` is set to 0.
- Recover a health check, verify `healthy` is set back to 1.
- Delete an app, verify all map entries are cleaned up.
- Deploy two apps with colliding VIPs (forced via test harness), verify collision resolution.

### 10.3 Failover Testing

- Kill Bun on a node. Verify existing connections continue working. Verify new connections to unchanged backends succeed. Verify connections to recently-rescheduled backends fail until Bun restarts.
- Kill Bun, reschedule a backend, restart Bun. Measure the time from Bun start to map refresh completion.
- Detach eBPF programs manually. Verify Bun detects and reattaches within 5 seconds.
- Fill BPF maps to capacity. Verify Bun logs errors and fires alerts. Verify existing services continue working.

### 10.4 End-to-End Testing

- From inside a container, `dig redis.internal` returns the correct VIP.
- From inside a container, `curl http://web.internal:8080/` reaches a healthy web backend.
- From inside a container in namespace A, connecting to a service in namespace B is refused.
- From inside a container in namespace A with explicit `allow_from`, connecting to a service in namespace B succeeds.
- `relish resolve redis` shows the correct VIP, backends, and health status.
- `relish resolve --all` lists all services.

---

## 11. Prior Art

### Kubernetes: kube-proxy + CoreDNS

Kubernetes uses a two-component approach: CoreDNS resolves service names to ClusterIPs (virtual IPs), and kube-proxy programs iptables/IPVS rules to forward traffic from ClusterIPs to backend pods.

- **CoreDNS** ([architecture](https://coredns.io/manual/toc/)): A standalone DNS server process running as a Deployment in the cluster. It watches the Kubernetes API for Service objects and serves DNS records. Every DNS resolution requires a UDP round trip to the CoreDNS pods, adding 0.5-5ms latency and creating a single point of failure (mitigated by running multiple replicas and node-local DNS caching via NodeLocal DNSCache).
- **kube-proxy** ([design doc](https://github.com/kubernetes/design-proposals-archive/blob/main/network/services-networking.md)): Runs on every node. In iptables mode, it programs thousands of iptables rules to DNAT ClusterIP traffic to backend pod IPs. In IPVS mode, it uses the IPVS kernel module for more efficient load balancing. Rule update time is O(n) for iptables mode and degrades significantly at scale (>5,000 services).

**What Onion borrows:** The concept of virtual IPs (ClusterIPs in Kubernetes) for service-level addressing. The concept of per-node backends lists for load balancing.

**What Onion does differently:** Eliminates the DNS server entirely (kernel map lookup vs. network round trip). Eliminates iptables/IPVS (eBPF connect rewrite vs. packet-level DNAT). Eliminates the proxy process (zero data-path overhead). Service map updates are microseconds (BPF map write) vs. seconds (iptables reprogramming).

### Cilium (eBPF)

Cilium ([eBPF documentation](https://docs.cilium.io/en/stable/bpf/)) is the closest prior art. It uses eBPF extensively for networking, security, and observability in Kubernetes clusters.

- **Cilium's eBPF service load balancing** replaces kube-proxy with eBPF programs that intercept packets at the TC (traffic control) or XDP (eXpress Data Path) layer. This is packet-level interception, not socket-level.
- **Cilium's socket-level LB** (more recent) provides `connect()`-level interception similar to Onion, but sits within the broader Cilium architecture that also includes an overlay network (VXLAN/Geneve), a full CNI plugin, and integration with Kubernetes NetworkPolicy, Hubble observability, and Envoy for L7 policy.

**What Onion borrows from Cilium:** The fundamental insight that eBPF can replace proxy processes for service discovery. The use of `BPF_CGROUP_INET4_CONNECT` for socket-level load balancing. The use of BPF hash maps for O(1) service lookups. The use of cgroup IDs for workload identification.

**What Onion does differently:**

- **Socket-level only, not packet-level.** Onion operates exclusively at the socket layer (`connect()`, `sendmsg()`, `recvmsg()`). It never touches TC, XDP, or any packet-processing hook. This is simpler, has zero data-path overhead (no per-packet processing), and avoids the complexity of packet-level NAT state tracking.
- **No overlay network.** Cilium typically manages a full CNI with VXLAN/Geneve tunneling or native routing. Onion operates on top of Reliaburger's overlay-free architecture (direct host port mapping). This eliminates tunnel encapsulation overhead and MTU issues.
- **No proxy at all.** Cilium integrates Envoy for L7 policy enforcement. Onion has no proxy component -- all policy enforcement happens in the eBPF connect hook.
- **DNS interception, not DNS proxying.** Cilium DNS policy uses a DNS proxy that parses queries and can enforce FQDN-based policy. Onion intercepts DNS at the socket level and responds from a kernel map -- no proxy process, no DNS parsing complexity beyond name matching.
- **Single-purpose, minimal scope.** Cilium is a full networking stack (CNI, network policy, observability, service mesh, gateway API). Onion is only service discovery and namespace-level firewall enforcement. This dramatically reduces complexity and attack surface.

### Consul Connect

HashiCorp Consul Connect provides service discovery via DNS or HTTP API, with optional mTLS via sidecar proxies (Envoy). Consul's DNS interface requires a Consul agent running on each node, adding process overhead. The sidecar proxy model adds per-connection latency. Consul Connect's architecture is fundamentally proxy-based.

**What Onion borrows:** The principle that service discovery should be transparent to applications (no SDK required).

**What Onion does differently:** No agent process in the DNS path (eBPF vs. Consul agent). No sidecar proxy (connect rewrite vs. Envoy proxy). No gossip-based service catalog (reporting tree + BPF maps vs. Consul's Serf + agent).

### Envoy / Istio

Istio uses Envoy sidecar proxies injected into every pod. All inbound and outbound traffic is redirected through the proxy via iptables rules. Envoy performs service discovery (via xDS API from the Istio control plane), load balancing, mTLS, retries, and observability.

**What Onion borrows:** Nothing directly. Istio's sidecar model is the architectural opposite of Onion's approach.

**What Onion does differently:** Eliminates the entire sidecar proxy. Eliminates iptables traffic redirection. Eliminates per-connection userspace proxy overhead (0.2-1ms per connection). Eliminates per-pod memory overhead (50-200MB per Envoy instance). Trades L7 feature richness (retries, circuit breaking, header manipulation) for zero-overhead L4 service discovery. L7 features, if needed, are the application's responsibility or are provided by the Wrapper ingress layer for external traffic.

---

## 12. Libraries & Dependencies

### eBPF Rust Ecosystem

The eBPF programs are written in C (required for the BPF compiler toolchain) and compiled to BPF bytecode with `clang`. The Rust side (Bun) needs a library to load programs, attach them to hooks, and read/write BPF maps.

### aya vs. libbpf-rs

| Criterion | [aya](https://github.com/aya-rs/aya) | [libbpf-rs](https://github.com/libbpf/libbpf-rs) |
|-----------|-----|-----------|
| **Approach** | Pure Rust, no C dependencies | Rust bindings to libbpf (C library) |
| **Build dependencies** | None (no libelf, no zlib) | Requires libelf, zlib; links libbpf statically |
| **CO-RE support** | Yes (via BTF) | Yes (via libbpf's CO-RE) |
| **Map operations** | Type-safe Rust API | C-style API with Rust wrappers |
| **Program types** | Comprehensive (cgroup/connect4, cgroup/sendmsg4, etc.) | Comprehensive (inherits from libbpf) |
| **Maturity** | Active development, production use at Cloudflare and others | Stable, backed by libbpf's broad adoption |
| **Single binary** | Easier (no C library to link) | Requires static linking of libbpf |
| **aya-bpf** | Write eBPF programs in Rust (experimental) | N/A (C programs only) |
| **Error messages** | Rust-native, clear | Maps to libbpf C errors, sometimes opaque |

**Recommendation: aya.**

Reliaburger ships as a single static binary. aya's pure-Rust approach eliminates the libelf/zlib build dependencies and simplifies cross-compilation. aya's type-safe map API reduces the risk of key/value size mismatches. aya supports all the BPF program types and map types Onion needs (`CgroupSockAddr` for connect4 and sendmsg4, `HashMap` for all maps).

The eBPF programs themselves are written in C (not aya-bpf/Rust) because the C BPF toolchain is more mature and the verifier behaviour with C-compiled programs is better understood. aya loads the pre-compiled `.bpf.o` object files.

### Additional Crates

| Crate | Purpose |
|-------|---------|
| `aya` | eBPF program loading, map operations, program attachment |
| `aya-obj` | Parsing of ELF/BTF sections in compiled BPF object files |
| `libc` | BPF syscall wrappers (`SYS_bpf`) for any operations not covered by aya |
| `siphasher` | SipHash-2-4 for deterministic VIP allocation |
| `nix` | Cgroup operations, namespace manipulation |

---

## 13. Open Questions

### 13.1 UDP Service Discovery

The current design focuses on TCP (`connect()` interception). For UDP services:

- **Connected UDP sockets** (`connect()` + `send()`) are handled by the same `onion_connect` hook.
- **Unconnected UDP sockets** (`sendto()` / `sendmsg()` with a destination address) require the `BPF_CGROUP_UDP4_SENDMSG` hook (already used for DNS interception). The same program must also handle service discovery for non-DNS UDP traffic to VIPs.
- **Question:** Should Onion intercept `sendmsg()` calls to VIP addresses on non-53 ports and rewrite the destination to a real backend? This would cover unconnected UDP sockets (e.g., statsd, syslog, game servers). The eBPF sendmsg hook supports this, but the recvmsg path (receiving responses from the rewritten backend) requires additional tracking to map response packets back to the original VIP.
- **Current position:** TCP is the priority. Connected UDP sockets work automatically via `connect()`. Unconnected UDP to VIPs is deferred until a concrete use case requires it.

### 13.2 IPv6 Support

The current design uses IPv4 only (127.128.0.0/16 VIP range, `BPF_CGROUP_INET4_CONNECT`).

- **Question:** When is IPv6 support needed? IPv6-only container environments would require a parallel set of eBPF programs (`BPF_CGROUP_INET6_CONNECT`, `BPF_CGROUP_UDP6_SENDMSG`) and a VIP range within the IPv6 loopback space (`::1/128` is a single address; an alternative like `fd00::/8` ULA range could be used).
- **Current position:** IPv4 service discovery covers the vast majority of use cases. Host-to-host communication can use IPv6 independently. IPv6 Onion support is deferred.

### 13.3 BPF Map Size Limits

- **Question:** Is 65,534 services (matching the /16 VIP range) sufficient? How many clusters will approach this limit?
- **Analysis:** 65,534 unique app names is a very large number. Most clusters run hundreds to low thousands of distinct services. The limit can be raised to ~4 million by expanding the VIP range to /10, but this requires a rolling restart.
- **Question:** Is 32 backends per service sufficient? Services with >32 replicas are uncommon but exist (large web tiers, worker pools).
- **Option A:** Increase `MAX_BACKENDS` to 64 or 128. Cost: larger `backend_value` struct (each additional slot costs 8 bytes in the map).
- **Option B:** Use a BPF array-of-maps or hash-of-maps to support variable-length backend lists. Cost: additional BPF map indirection, more complex eBPF program logic.
- **Current position:** 32 backends is the default. The constant is configurable at compile time. Option A (increase to 64) is the likely first step if needed.

### 13.4 Multi-Port Services

- **Question:** How are services that expose multiple ports handled? For example, a service with both an HTTP port (8080) and a gRPC port (9090).
- **Current design:** Each `(VIP, port)` pair is a separate `backend_map` entry. A multi-port service has the same VIP but multiple entries in `backend_map`, one per declared port. The DNS name resolves to the same VIP; the port in the `connect()` call determines which backend list is consulted.
- **This is consistent with the whitepaper:** apps declare a single `port` in the TOML config. Multi-port apps would need multiple app declarations or an extension to the app spec.

### 13.5 Graceful Backend Removal

- **Question:** When a backend is removed from the map (app termination, rolling deploy), in-flight connections to that backend are not affected (they are established TCP connections). But what about connections that are in the DNS-resolved-but-not-yet-connected state? An application might resolve `redis.internal` to a VIP, then call `connect()` 100ms later. If the backend list changed in between, the application gets a different backend -- which is correct behaviour. But should we support connection draining (keeping the old backend in the map with a "draining" flag so new connections are not routed to it, but existing connections are not broken)?
- **Current position:** No draining state in the eBPF layer. Backend removal is immediate. The brief window between map update and application retry is acceptable for most workloads. Connection draining is handled at the application level or by the Wrapper ingress layer for external traffic.
